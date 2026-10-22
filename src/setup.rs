//! First-run configuration without editing TOML or emitting credentials.
//! Discovery, answers, rendering and publishing are separate so a future UI
//! can use the same checked configuration and transaction boundary.

use anyhow::{bail, Context, Result};
use futures_util::StreamExt;
use serde::Serialize;
use std::{
    io::{BufRead, Write},
    net::{IpAddr, SocketAddr},
    path::{Path, PathBuf},
    time::Duration,
};

use crate::{config::Config, plugin::pressure::PressureMode, probes::nvidia};

#[derive(Debug, Serialize)]
pub struct Discovery {
    pub gpus: Vec<GpuCandidate>,
    pub gpu_error: Option<String>,
    pub upstreams: Vec<Candidate>,
}

/// Inventory only: discovery is not a live vacancy or pressure verdict.
#[derive(Debug, Serialize)]
pub struct GpuCandidate {
    pub index: u32,
    pub uuid: String,
    pub label: String,
    pub total_mib: u64,
}

#[derive(Debug, Serialize)]
pub struct Candidate {
    pub kind: String,
    pub target: String,
}

fn discovery_client() -> Result<reqwest::Client> {
    Ok(reqwest::Client::builder()
        .no_proxy()
        .redirect(reqwest::redirect::Policy::none())
        .timeout(Duration::from_secs(2))
        .build()?)
}

pub async fn discover() -> Result<Discovery> {
    let inventory = tokio::task::spawn_blocking(nvidia::inventory);
    let client = discovery_client()?;
    let (ollama, openai, comfyui) = tokio::join!(
        detect(&client, "ollama", "http://127.0.0.1:11434", "/api/version"),
        detect(&client, "openai", "http://127.0.0.1:8080", "/v1/models"),
        detect(&client, "comfyui", "http://127.0.0.1:8188", "/system_stats"),
    );
    let (gpus, gpu_error) = match inventory.await.context("GPU discovery worker failed")? {
        Ok(gpus) => (gpus, None),
        Err(error) => (vec![], Some(format!("{error:#}"))),
    };
    Ok(Discovery {
        gpus: gpus
            .into_iter()
            .map(|gpu| GpuCandidate {
                index: gpu.index,
                uuid: gpu.uuid,
                label: gpu.cap.label,
                total_mib: gpu.cap.total_bytes / (1024 * 1024),
            })
            .collect(),
        gpu_error,
        upstreams: [ollama, openai, comfyui].into_iter().flatten().collect(),
    })
}

async fn detect(
    client: &reqwest::Client,
    kind: &str,
    target: &str,
    path: &str,
) -> Option<Candidate> {
    let response = client.get(format!("{target}{path}")).send().await.ok()?;
    if !response.status().is_success() {
        return None;
    }
    let mut body = Vec::new();
    let mut stream = response.bytes_stream();
    while let Some(chunk) = stream.next().await {
        let chunk = chunk.ok()?;
        if body.len().saturating_add(chunk.len()) > 64 * 1024 {
            return None;
        }
        body.extend_from_slice(&chunk);
    }
    let value: serde_json::Value = serde_json::from_slice(&body).ok()?;
    let matches = match kind {
        "ollama" => value["version"].is_string(),
        "openai" => value["data"].is_array(),
        "comfyui" => value["system"].is_object() && value["devices"].is_array(),
        _ => false,
    };
    matches.then(|| Candidate {
        kind: kind.into(),
        target: target.into(),
    })
}

#[derive(Debug)]
struct Answers {
    reclaim: crate::config::ReclaimMode,
    drain_timeout_s: u32,
    gpu_uuid: Option<String>,
    kind: String,
    target: String,
    listen: SocketAddr,
    min_free_mib: u64,
    ram_reserve_mib: u64,
    /// Total memory of the selected GPU and of the machine, where discovery
    /// read them. A budget larger than the hardware could never admit a job.
    gpu_total_mib: Option<u64>,
    ram_total_mib: Option<u64>,
    pressure_mode: PressureMode,
    threshold_pct: f64,
}

/// Strings are serialized as TOML values, never interpolated as TOML syntax.
fn quoted(value: &str) -> String {
    toml::Value::String(value.into()).to_string()
}

fn render(answers: &Answers, token_name: &str) -> Result<String> {
    let target = reqwest::Url::parse(&answers.target).context("invalid upstream URL")?;
    let loopback = target
        .host_str()
        .and_then(|host| host.trim_matches(['[', ']']).parse::<IpAddr>().ok())
        .is_some_and(|ip| ip.is_loopback());
    if !loopback
        || target.scheme() != "http"
        || !target.username().is_empty()
        || target.password().is_some()
    {
        bail!("setup requires an HTTP target with a literal loopback IP and no URL credentials");
    }
    if answers.min_free_mib == 0 {
        bail!("minimum free memory must be greater than zero");
    }
    // The per-job requirement budgets the GPU when one is selected, and
    // system memory on top of the owner's reserve when none is. A budget
    // the hardware cannot hold would refuse every job, so it is refused
    // here instead of producing a configuration that never lends.
    if let Some(total) = answers.ram_total_mib {
        let needed = if answers.gpu_uuid.is_some() {
            answers.ram_reserve_mib
        } else {
            answers.ram_reserve_mib.saturating_add(answers.min_free_mib)
        };
        if needed >= total {
            bail!(
                "{} needs {needed} MiB of the {total} MiB of system memory, so no job could ever be admitted; lower it",
                if answers.gpu_uuid.is_some() {
                    "the owner RAM reserve".to_owned()
                } else {
                    format!(
                        "a job's {} MiB on top of the owner's {} MiB RAM reserve",
                        answers.min_free_mib, answers.ram_reserve_mib
                    )
                }
            );
        }
    }
    if let (Some(total), true) = (answers.gpu_total_mib, answers.gpu_uuid.is_some()) {
        if answers.min_free_mib > total {
            bail!(
                "a job's {} MiB exceeds the selected GPU's {total} MiB, so no job could ever be admitted; lower it",
                answers.min_free_mib
            );
        }
    }
    let reserve = answers
        .ram_reserve_mib
        .checked_mul(1024 * 1024)
        .context("RAM reserve is too large")?;
    let mut text = format!(
        "# Generated by vacancyd setup. Credentials are stored separately.\n\
         [server]\nlisten = \"127.0.0.1:9811\"\n\n\
         [auth]\ntrust_loopback = true\n\
         [[auth.token]]\nname = \"cluster\"\nscopes = [\"borrow\"]\nvalue_file = {}\n\n\
         [owner]\nunlocked_session_is_presence = true\nidle_lends_after_s = 0\n\
         [owner.gpu_pressure]\nmode = {}\nenabled = {}\nthreshold_pct = {}\n\n\
         [[resource]]\nid = \"memory\"\nkind = \"system-memory\"\nreserve = {reserve}\n\n\
         [[resource]]\nid = \"cpu\"\nkind = \"system-cpu\"\nreserve = 85\n\n",
        quoted(token_name),
        toml::Value::try_from(answers.pressure_mode)?,
        answers.gpu_uuid.is_some(),
        answers.threshold_pct,
    );
    let (resource, dependencies) = if let Some(uuid) = &answers.gpu_uuid {
        if !uuid.starts_with("GPU-") {
            bail!("setup requires a stable NVIDIA UUID");
        }
        text.push_str(&format!(
            "[[resource]]\nid = \"gpu\"\nkind = \"nvidia-gpu\"\nselector = {}\n\n",
            quoted(uuid)
        ));
        ("gpu", "[\"memory\", \"cpu\"]")
    } else {
        ("memory", "[\"cpu\"]")
    };
    text.push_str(&format!(
        "[[upstream]]\nid = \"inference\"\nkind = {}\ntarget = {}\nlisten = {}\n\
         resource = {}\nalso_requires = {dependencies}\ndefault_min_free_mib = {}\n\
         # Owner return always refuses new work; configure existing work below.\n\
         on_owner_return = {}\ndrain_timeout_s = {}\n\
         # Upstream jobs may outlive a disconnected proxy request.\n\
         yield_on_veto = false\n",
        quoted(&answers.kind),
        quoted(&answers.target),
        quoted(&answers.listen.to_string()),
        quoted(resource),
        answers.min_free_mib,
        toml::Value::try_from(answers.reclaim)?,
        answers.drain_timeout_s,
    ));
    let config: Config = toml::from_str(&text).context("rendering setup configuration")?;
    config.validate()?;
    Ok(text)
}

pub(crate) fn nonce() -> Result<String> {
    let mut bytes = [0u8; 16];
    getrandom::getrandom(&mut bytes)
        .map_err(|e| anyhow::anyhow!("OS randomness unavailable: {e}"))?;
    Ok(bytes.iter().map(|b| format!("{b:02x}")).collect())
}

struct Transaction {
    temporary: Vec<PathBuf>,
    permanent: Vec<PathBuf>,
    committed: bool,
}

impl Transaction {
    fn write(&mut self, path: &Path, bytes: &[u8], permanent: bool) -> Result<()> {
        let mut file = crate::secure_file::create_new(path)
            .with_context(|| format!("creating private file {}", path.display()))?;
        if permanent {
            self.permanent.push(path.into());
        } else {
            self.temporary.push(path.into());
        }
        file.write_all(bytes)?;
        file.sync_all()?;
        Ok(())
    }

    /// Publish the validated `staged` file as `destination`, durably. This
    /// commits the transaction: the published configuration names the
    /// credentials it wrote, so from here on they are kept.
    fn publish(&mut self, staged: &Path, destination: &Path, replace: bool) -> Result<()> {
        use crate::durable::ReplaceError;
        let published = if replace {
            crate::durable::replace(staged, destination)
        } else {
            // Never replaces a file a racing writer created.
            crate::durable::publish_new(staged, destination)
        };
        match published {
            Ok(()) => {
                self.committed = true;
                Ok(())
            }
            Err(ReplaceError::NotReplaced(error)) => {
                Err(anyhow::Error::new(error).context(if replace {
                    "publishing validated configuration"
                } else {
                    "publishing new configuration without overwriting"
                }))
            }
            Err(ReplaceError::NotDurable(error)) => {
                self.committed = true;
                Err(anyhow::Error::new(error).context(format!(
                    "{} was written, but the change could not be synced to storage; \
                     confirm the file is present after the next restart",
                    destination.display()
                )))
            }
        }
    }
}

impl Drop for Transaction {
    fn drop(&mut self) {
        let mut directories = std::collections::BTreeSet::new();
        for path in self
            .temporary
            .iter()
            .chain(self.permanent.iter().filter(|_| !self.committed))
        {
            match std::fs::remove_file(path) {
                Ok(()) => {
                    directories.insert(path.parent().map(Path::to_path_buf));
                }
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
                Err(error) => eprintln!("Setup cleanup failed for {}: {error}", path.display()),
            }
        }
        // A removal lost to a power cut would leave the setup lock behind and
        // refuse the next run, so make the cleanup durable too.
        for directory in directories.into_iter().flatten() {
            if let Err(error) = crate::durable::sync_dir(&directory) {
                eprintln!(
                    "Setup cleanup could not sync {}: {error}",
                    directory.display()
                );
            }
        }
    }
}

#[derive(Debug)]
struct Installed {
    config: PathBuf,
    token: PathBuf,
    backup: Option<PathBuf>,
}

fn read_existing(path: &Path, replace_starter: bool) -> Result<Option<Vec<u8>>> {
    match std::fs::symlink_metadata(path) {
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(error) => Err(error.into()),
        Ok(meta) => {
            if !meta.file_type().is_file() {
                bail!("configuration path is not a regular file");
            }
            if !replace_starter {
                bail!("configuration exists; use --replace-starter only for the unmodified MSI starter");
            }
            let bytes = std::fs::read(path)?;
            let raw = std::str::from_utf8(&bytes).context("existing configuration is not UTF-8")?;
            if raw.replace("\r\n", "\n").trim()
                != include_str!("../packaging/vacancyd.starter.toml")
                    .replace("\r\n", "\n")
                    .trim()
            {
                bail!("existing configuration is not the unmodified MSI starter; choose a new config path instead");
            }
            Ok(Some(bytes))
        }
    }
}

fn install(answers: &Answers, path: &Path, replace_starter: bool) -> Result<Installed> {
    let parent = path
        .parent()
        .filter(|p| !p.as_os_str().is_empty())
        .unwrap_or_else(|| Path::new("."));
    // Validate all answers before creating credentials or touching an old config.
    let id = nonce()?;
    let token_name = format!("vacancyd-borrow-{id}.token");
    let rendered = render(answers, &token_name)?;
    #[cfg(windows)]
    refuse_unprotected(parent, "configuration directory")?;
    crate::secure_file::create_missing_private_dir(parent)
        .with_context(|| format!("creating the configuration directory {}", parent.display()))?;
    let parent = std::fs::canonicalize(parent)?;
    let config = parent.join(path.file_name().context("config path needs a file name")?);
    let mut transaction = Transaction {
        temporary: vec![],
        permanent: vec![],
        committed: false,
    };
    transaction
        .write(
            &parent.join(".vacancyd-setup.lock"),
            b"Setup in progress. Remove only after confirming no setup is running.\n",
            false,
        )
        .context("another setup may be running; no existing configuration was changed")?;
    #[cfg(windows)]
    let installed_directory =
        replace_starter && parent.file_name().and_then(|n| n.to_str()) == Some("vacancyd");
    #[cfg(windows)]
    if installed_directory {
        crate::windows_acl::protect_config(&config, false)?;
    }
    let original = read_existing(&config, replace_starter)?;
    let token = parent.join(&token_name);
    let value = crate::auth::generate_token()?;
    transaction.write(&token, value.as_bytes(), true)?;
    #[cfg(windows)]
    if installed_directory {
        crate::windows_acl::protect_file(&token)?;
    }
    let staged = parent.join(format!(".vacancyd-setup-{id}.toml"));
    transaction.write(&staged, rendered.as_bytes(), false)?;
    #[cfg(windows)]
    if installed_directory {
        crate::windows_acl::protect_file(&staged)?;
        crate::windows_acl::verify_config(&staged)?;
    }
    // Same validation as check-config, including relative credential paths and
    // actual credential resolution. The staged file shares the final directory.
    let validated = Config::load(&staged)?;
    validated.resolve_credentials()?;
    if read_existing(&config, replace_starter)? != original {
        bail!("configuration changed during setup; refusing to replace it");
    }
    let backup = if let Some(bytes) = original {
        let backup = parent.join(format!("vacancyd-before-setup-{id}.toml"));
        transaction.write(&backup, &bytes, true)?;
        #[cfg(windows)]
        if installed_directory {
            crate::windows_acl::protect_file(&backup)?;
        }
        transaction.publish(&staged, &config, true)?;
        Some(backup)
    } else {
        transaction.publish(&staged, &config, false)?;
        None
    };
    Ok(Installed {
        config,
        token,
        backup,
    })
}

/// On Windows, refuse the directory setup or migration writes in unless it
/// is inside the protected `ProgramData\vacancyd` folder and no other account
/// could redirect the path to it, whether or not it exists yet. It is
/// checked as written, before canonicalizing would remove a link from it.
#[cfg(windows)]
fn refuse_unprotected(directory: &Path, role: &str) -> Result<()> {
    if let Some(reason) = crate::secure_file::replaceable_path(directory)? {
        bail!("refusing the {role} {}: {reason}", directory.display());
    }
    Ok(())
}

/// Copy an existing configuration without changing its bytes. Only a new
/// destination or the exact inert MSI starter may be replaced. Relative
/// credential files (auth tokens and upstream credentials) must be sibling
/// files, so migration cannot silently traverse a tree. A configuration whose
/// admission recovery records the copy would no longer find is refused.
pub fn migrate(source: &Path, destination: &Path, replace_starter: bool) -> Result<()> {
    migrate_with(
        source,
        destination,
        replace_starter,
        std::env::var_os("STATE_DIRECTORY").as_deref(),
    )
}

/// [`migrate`], with the service manager's state directory given rather
/// than read from the environment.
fn migrate_with(
    source: &Path,
    destination: &Path,
    replace_starter: bool,
    state_directory: Option<&std::ffi::OsStr>,
) -> Result<()> {
    use std::io::Read;
    fn bounded_read(path: &Path, limit: u64) -> Result<Vec<u8>> {
        let metadata = std::fs::symlink_metadata(path)?;
        if !metadata.file_type().is_file() {
            bail!("migration source must be a regular file");
        }
        let mut bytes = Vec::new();
        std::fs::File::open(path)?
            .take(limit + 1)
            .read_to_end(&mut bytes)?;
        if bytes.len() as u64 > limit {
            bail!("migration source exceeds size limit");
        }
        Ok(bytes)
    }
    let bytes = bounded_read(source, 1024 * 1024)?;
    let raw = std::str::from_utf8(&bytes).context("configuration must be UTF-8")?;
    // TOML errors contain source snippets; never surface secret-bearing input.
    let config: Config =
        toml::from_str(raw).map_err(|_| anyhow::anyhow!("configuration is invalid TOML"))?;
    config.validate()?;
    let source_parent = source.parent().context("source needs a parent directory")?;
    let parent = destination
        .parent()
        .context("destination needs a parent directory")?;
    #[cfg(windows)]
    refuse_unprotected(parent, "destination directory")?;
    crate::secure_file::create_missing_private_dir(parent)
        .with_context(|| format!("creating the destination directory {}", parent.display()))?;
    let parent = std::fs::canonicalize(parent)?;
    let target = parent.join(
        destination
            .file_name()
            .context("destination needs a file name")?,
    );
    if std::fs::canonicalize(source)? == target {
        bail!("source and destination must differ");
    }
    refuse_stranded_records(&config, source, destination, state_directory)?;
    #[cfg(windows)]
    {
        if parent.file_name().and_then(|n| n.to_str()) != Some("vacancyd") {
            bail!("migration destination requires a dedicated directory named vacancyd");
        }
        // Its permissions are about to be rewritten as an administrator, so
        // no other account may be able to redirect the path to it, even when
        // it already existed. The path as written was checked above; this
        // checks where it led.
        if let Some(reason) = crate::secure_file::replaceable_path(&parent)? {
            bail!(
                "refusing to protect the destination directory {}: {reason}",
                parent.display()
            );
        }
        crate::windows_acl::protect_directory(&parent)?;
    }
    let mut transaction = Transaction {
        temporary: vec![],
        permanent: vec![],
        committed: false,
    };
    transaction.write(
        &parent.join(".vacancyd-setup.lock"),
        b"Migration in progress\n",
        false,
    )?;
    let original = read_existing(&target, replace_starter)?;
    let mut copied = std::collections::BTreeSet::new();
    for path in config.credential_files() {
        if path.is_absolute() {
            #[cfg(windows)]
            crate::windows_acl::verify_config(path)
                .context("external credential permissions need protection before migration")?;
            continue;
        }
        if path.components().count() != 1
            || !matches!(
                path.components().next(),
                Some(std::path::Component::Normal(_))
            )
        {
            bail!("relative credential paths must name a sibling file; migrate nested credentials explicitly first");
        }
        if !copied.insert(path.clone()) {
            continue;
        }
        let credential = bounded_read(&source_parent.join(path), 64 * 1024)?;
        let token_target = parent.join(path);
        transaction.write(&token_target, &credential, true)?;
        #[cfg(windows)]
        crate::windows_acl::protect_file(&token_target)?;
    }
    let id = nonce()?;
    let staged = parent.join(format!(".vacancyd-setup-{id}.toml"));
    transaction.write(&staged, &bytes, false)?;
    #[cfg(windows)]
    {
        crate::windows_acl::protect_file(&staged)?;
        crate::windows_acl::verify_config(&staged)?;
    }
    let staged_config = Config::load(&staged)?;
    staged_config.resolve_credentials()?;
    if read_existing(&target, replace_starter)? != original
        || bounded_read(source, 1024 * 1024)? != bytes
    {
        bail!("configuration changed during migration; destination was not replaced");
    }
    if let Some(original) = original {
        let backup = parent.join(format!("vacancyd-before-setup-{id}.toml"));
        transaction.write(&backup, &original, true)?;
        #[cfg(windows)]
        crate::windows_acl::protect_file(&backup)?;
        transaction.publish(&staged, &target, true)?;
    } else {
        transaction.publish(&staged, &target, false)?;
    }
    // Drop removes the staging lock, leaving the published files.
    drop(transaction);
    Ok(())
}

/// Refuse a migration after which the service would no longer find the
/// source's recovery records: a default or relative `admission_state_dir`
/// resolves beside the configuration, so a copy elsewhere would start with
/// an empty ledger and forget held work.
fn refuse_stranded_records(
    config: &Config,
    source: &Path,
    destination: &Path,
    state_directory: Option<&std::ffi::OsStr>,
) -> Result<()> {
    // Absolute as written, as the service is given them with --config.
    // Canonical paths would print in Windows' verbatim form, which an
    // administrator would then copy into the configuration.
    let source = std::path::absolute(source)?;
    let destination = std::path::absolute(destination)?;
    let current = crate::admission::RecoveryDir::resolve(config, &source, state_directory).path;
    let after = crate::admission::RecoveryDir::resolve(config, &destination, state_directory).path;
    if current == after {
        return Ok(());
    }
    let held = crate::admission::holds_records(&current).with_context(|| {
        format!(
            "reading the admission recovery directory {}; nothing was copied",
            current.display()
        )
    })?;
    if held {
        bail!(
            "the admission recovery directory {} holds records of unfinished work, and the copied configuration would use {} instead, so the service would start without them. Nothing was copied. Under [server] in the source configuration, set admission_state_dir = '{}' and migrate again",
            current.display(),
            after.display(),
            current.display()
        );
    }
    Ok(())
}

fn ask(
    input: &mut impl BufRead,
    output: &mut impl Write,
    question: &str,
    default: &str,
) -> Result<String> {
    write!(output, "{question} [{default}]: ")?;
    output.flush()?;
    let mut line = String::new();
    if input.read_line(&mut line)? == 0 {
        bail!("setup cancelled: input ended; nothing installed");
    }
    let value = line.trim();
    Ok(if value.is_empty() {
        default.to_string()
    } else {
        value.to_string()
    })
}

fn confirm(input: &mut impl BufRead, output: &mut impl Write, question: &str) -> Result<bool> {
    Ok(ask(input, output, question, "no")?.eq_ignore_ascii_case("yes"))
}

fn placement_guidance(kind: &str, gpu_uuid: Option<&str>) -> String {
    let mut guidance = String::from(
        "Setup does not edit the upstream service or firewall. After activation, verify the console's direct-listener state: network-bound direct requests bypass owner protection; unknown means the bind still needs verification.",
    );
    if let Some(uuid) = gpu_uuid {
        let process = match kind {
            "ollama" => "dedicated Ollama process",
            "comfyui" => "dedicated ComfyUI launcher",
            "llamacpp" | "openai" => "dedicated inference-server launcher",
            _ => "dedicated upstream process",
        };
        guidance.push_str(&format!(
            " Configure the {process} to select {uuid}; for CUDA servers, set CUDA_VISIBLE_DEVICES={uuid} where supported. Load a test model and use nvidia-smi to verify that its process runs on {uuid}. CUDA can renumber visible devices internally, so do not treat a displayed numeric device index as the configured NVIDIA UUID."
        ));
    }
    guidance
}

fn reclaim_options(
    input: &mut impl BufRead,
    output: &mut impl Write,
) -> Result<(crate::config::ReclaimMode, u32)> {
    use crate::config::ReclaimMode;
    writeln!(output, "Owner return defaults to refusing new work and draining accepted proxy work for 30 seconds before cancellation.")?;
    if !confirm(input, output, "Change advanced owner-return options?")? {
        return Ok((ReclaimMode::Drain, 30));
    }
    let mode = match ask(input, output, "Existing work: cancel immediately, drain with a time limit, or finish regardless of time (cancel/drain/finish)", "drain")?.as_str() {
        "cancel" => ReclaimMode::Cancel,
        "drain" => ReclaimMode::Drain,
        "finish" => ReclaimMode::Finish,
        _ => bail!("owner-return option must be cancel, drain, or finish"),
    };
    let seconds = if mode == ReclaimMode::Drain {
        ask(
            input,
            output,
            "Drain limit in whole seconds (0 cancels immediately)",
            "30",
        )?
        .parse()
        .context("drain limit must be a nonnegative whole number of seconds")?
    } else {
        30
    };
    Ok((mode, seconds))
}

pub async fn run(path: &Path, replace_starter: bool) -> Result<()> {
    if !cfg!(windows) {
        bail!("guided setup currently supports Windows only; on Linux, start from vacancyd.example.toml (see docs/setup.md); discover remains available on this platform");
    }
    read_existing(path, replace_starter).map_err(|error| {
        if error
            .downcast_ref::<std::io::Error>()
            .is_some_and(|error| error.kind() == std::io::ErrorKind::PermissionDenied)
        {
            error.context("Setup cannot access the protected configuration. For the MSI installation, right-click Setup in the vacancyd Start menu folder and choose Run as administrator, or run setup from an elevated terminal.")
        } else {
            error
        }
    })?;
    let discovery = discover().await?;
    let mut input = std::io::stdin().lock();
    let mut output = std::io::stdout().lock();
    writeln!(output, "Setup writes configuration and a private borrower token. It does not restart services, edit the upstream service, or change firewall/startup settings.")?;
    if let Some(error) = &discovery.gpu_error {
        writeln!(output, "GPU inventory unavailable: {error}")?;
    }
    writeln!(output, "0: CPU-only (system memory and CPU capacity)")?;
    for (i, gpu) in discovery.gpus.iter().enumerate() {
        writeln!(
            output,
            "{}: {} ({} MiB, {})",
            i + 1,
            gpu.label,
            gpu.total_mib,
            gpu.uuid
        )?;
    }
    let index: usize = ask(
        &mut input,
        &mut output,
        "Resource number",
        if discovery.gpus.is_empty() { "0" } else { "1" },
    )?
    .parse()
    .context("resource number must be an integer")?;
    let gpu_uuid = if index == 0 {
        None
    } else {
        Some(
            discovery
                .gpus
                .get(index - 1)
                .context("resource number is out of range")?
                .uuid
                .clone(),
        )
    };
    for candidate in &discovery.upstreams {
        writeln!(
            output,
            "Candidate: {} at {} (binding/isolation not verified)",
            candidate.kind, candidate.target
        )?;
    }
    let candidate = discovery.upstreams.first();
    let kind = ask(
        &mut input,
        &mut output,
        "Dialect: ollama, openai, llamacpp, or comfyui",
        candidate.map(|c| c.kind.as_str()).unwrap_or("ollama"),
    )?;
    let default_target = match kind.as_str() {
        "ollama" => "http://127.0.0.1:11434",
        "openai" | "llamacpp" => "http://127.0.0.1:8080",
        "comfyui" => "http://127.0.0.1:8188",
        _ => bail!("unknown dialect"),
    };
    let target = ask(
        &mut input,
        &mut output,
        "Real service URL (loopback only)",
        default_target,
    )?;
    if !confirm(
        &mut input,
        &mut output,
        "Have you configured a dedicated borrower instance bound only to loopback?",
    )? {
        bail!("configure and verify the dedicated loopback service first; nothing installed");
    }
    writeln!(output, "{}", placement_guidance(&kind, gpu_uuid.as_deref()))?;
    let listen = ask(
        &mut input,
        &mut output,
        "Proxy listen IP:port (use a LAN IP explicitly to expose it)",
        "127.0.0.1:11435",
    )?
    .parse::<SocketAddr>()
    .context("invalid proxy listen address")?;
    if !listen.ip().is_loopback() {
        writeln!(output, "Bearer tokens travel over HTTP. Restrict this listener to a trusted LAN with your firewall; do not expose it to the Internet.")?;
    }
    let min_free_mib = ask(
        &mut input,
        &mut output,
        if gpu_uuid.is_some() {
            "Minimum free VRAM per job, MiB (measured model footprint plus headroom)"
        } else {
            "Minimum free RAM per job beyond the owner's reserve, MiB (measured model footprint plus headroom)"
        },
        "",
    )?
    .parse()
    .context("enter a positive measured memory requirement in MiB")?;
    let ram_reserve_mib = ask(
        &mut input,
        &mut output,
        "RAM to keep free for the owner, MiB",
        "4096",
    )?
    .parse()
    .context("RAM reserve must be an integer")?;
    let mut pressure_mode = PressureMode::Calibrated;
    let mut threshold_pct = 0.0;
    if gpu_uuid.is_some() {
        writeln!(output, "Calibrated (recommended) learns each GPU's ordinary load and keeps its threshold above it, never past the cap. Lending waits for enough successful samples at first; elapsed time depends on the actual sample cadence and read success. Fixed is an advanced manual threshold.")?;
        pressure_mode = match ask(
            &mut input,
            &mut output,
            "GPU pressure mode: calibrated, fixed",
            "calibrated",
        )?
        .as_str()
        {
            "calibrated" => PressureMode::Calibrated,
            "fixed" => PressureMode::Fixed,
            _ => bail!("unknown GPU pressure mode"),
        };
        if pressure_mode == PressureMode::Fixed {
            threshold_pct = ask(
                &mut input,
                &mut output,
                "Non-borrower GPU utilization engage threshold, percent (0 < value <= 100)",
                "",
            )?
            .parse()
            .context("threshold must be a number")?;
        }
    }
    let (reclaim, drain_timeout_s) = reclaim_options(&mut input, &mut output)?;
    let gpu_total_mib = gpu_uuid.as_ref().and_then(|uuid| {
        discovery
            .gpus
            .iter()
            .find(|gpu| &gpu.uuid == uuid)
            .map(|gpu| gpu.total_mib)
    });
    let ram_total_mib = {
        use crate::plugin::probe::Probe;
        crate::probes::system::SystemMemory
            .sample()
            .ok()
            .map(|capacity| capacity.total_bytes / (1024 * 1024))
    };
    let answers = Answers {
        reclaim,
        drain_timeout_s,
        gpu_uuid,
        kind,
        target,
        listen,
        min_free_mib,
        ram_reserve_mib,
        gpu_total_mib,
        ram_total_mib,
        pressure_mode,
        threshold_pct,
    };
    render(&answers, "pending.token")?;
    let budget = if answers.gpu_uuid.is_some() {
        format!("{min_free_mib} MiB free VRAM per job")
    } else {
        format!("{min_free_mib} MiB free RAM per job beyond the owner reserve")
    };
    writeln!(
        output,
        "\n{} -> {} ({})\nRequired: {budget}; owner RAM reserve: {} MiB; CPU ceiling: 85%.",
        answers.listen,
        vacancyd::status::RedactedUrl(&answers.target),
        answers.kind,
        ram_reserve_mib
    )?;
    let policy = match reclaim {
        crate::config::ReclaimMode::Cancel => "cancel accepted proxy work immediately".to_owned(),
        crate::config::ReclaimMode::Drain => {
            format!("allow {drain_timeout_s} seconds to finish, then cancel remaining proxy work")
        }
        crate::config::ReclaimMode::Finish => {
            "allow accepted work to finish without a time limit".to_owned()
        }
    };
    writeln!(output, "Owner return: {policy}. New work is always refused immediately. Upstream servers may continue queued or detached work after a proxy connection is cancelled. No presence policy is automatically relaxed.")?;
    if !confirm(
        &mut input,
        &mut output,
        "Accept this owner-return policy and upstream cancellation limitations?",
    )? {
        writeln!(output, "Cancelled; no files written.")?;
        return Ok(());
    }
    if !confirm(
        &mut input,
        &mut output,
        &format!("Write validated configuration to {}?", path.display()),
    )? {
        writeln!(output, "Cancelled; no files written.")?;
        return Ok(());
    }
    let installed = install(&answers, path, replace_starter)?;
    writeln!(
        output,
        "Configuration: {}\nBorrow-only credential file: {} (value not printed)",
        installed.config.display(),
        installed.token.display()
    )?;
    if let Some(backup) = installed.backup {
        writeln!(output, "Previous starter preserved: {}", backup.display())?;
    }
    writeln!(output, "Not activated. Transfer the credential securely to your gateway, verify upstream isolation and firewall scope, then start/restart vacancyd explicitly. See docs/setup.md. MSI users can start the current session's reporter from Start > vacancyd > Start session agent, without elevation.")?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    struct Directory(PathBuf);
    impl Directory {
        fn new() -> Self {
            let path = crate::secure_file::test_root()
                .join(format!("vacancyd-setup-test-{}", nonce().unwrap()));
            std::fs::create_dir(&path).unwrap();
            Self(path)
        }
        fn files(&self) -> Vec<String> {
            let mut names: Vec<_> = std::fs::read_dir(&self.0)
                .unwrap()
                .map(|e| e.unwrap().file_name().to_string_lossy().into_owned())
                .collect();
            names.sort();
            names
        }
    }
    impl Drop for Directory {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }
    fn answers() -> Answers {
        Answers {
            reclaim: crate::config::ReclaimMode::Drain,
            drain_timeout_s: 30,
            gpu_uuid: Some("GPU-aaaaaaaa-bbbb-cccc-dddd-eeeeeeeeeeee".into()),
            kind: "ollama".into(),
            target: "http://127.0.0.1:11434".into(),
            listen: "127.0.0.1:11435".parse().unwrap(),
            min_free_mib: 4096,
            ram_reserve_mib: 4096,
            gpu_total_mib: Some(12288),
            ram_total_mib: Some(65536),
            pressure_mode: PressureMode::Calibrated,
            threshold_pct: 0.0,
        }
    }

    #[test]
    fn owner_return_setup_supports_default_and_all_advanced_choices() {
        use crate::config::ReclaimMode;
        for (input, expected, seconds) in [
            ("\n", ReclaimMode::Drain, 30),
            ("yes\ncancel\n", ReclaimMode::Cancel, 30),
            ("yes\ndrain\n75\n", ReclaimMode::Drain, 75),
            ("yes\ndrain\n0\n", ReclaimMode::Drain, 0),
            ("yes\nfinish\n", ReclaimMode::Finish, 30),
        ] {
            let (mode, limit) = reclaim_options(&mut input.as_bytes(), &mut Vec::new()).unwrap();
            assert_eq!((mode, limit), (expected, seconds));
            let mut answers = answers();
            answers.reclaim = mode;
            answers.drain_timeout_s = limit;
            let config: Config =
                toml::from_str(&render(&answers, "borrow.token").unwrap()).unwrap();
            assert_eq!(config.upstreams[0].on_owner_return, mode);
            assert_eq!(config.upstreams[0].drain_timeout_s, limit);
        }
        for input in ["yes\ninvalid\n", "yes\ndrain\n-1\n", "yes\ndrain\n", ""] {
            assert!(reclaim_options(&mut input.as_bytes(), &mut Vec::new()).is_err());
        }
    }

    #[test]
    fn placement_guidance_binds_the_selected_uuid_without_claiming_configuration() {
        let uuid = "GPU-aaaaaaaa-bbbb-cccc-dddd-eeeeeeeeeeee";
        for (kind, process) in [
            ("ollama", "Ollama process"),
            ("comfyui", "ComfyUI launcher"),
            ("llamacpp", "inference-server launcher"),
            ("openai", "inference-server launcher"),
        ] {
            let guidance = placement_guidance(kind, Some(uuid));
            let cuda_selector = format!("CUDA_VISIBLE_DEVICES={uuid}");
            assert!(guidance.contains(process));
            assert!(guidance.contains(cuda_selector.as_str()));
            assert!(guidance.contains("nvidia-smi"));
            assert!(guidance.contains("does not edit the upstream service or firewall"));
            assert!(guidance.contains("network-bound direct requests bypass owner protection"));
            assert!(guidance.contains("unknown means the bind still needs verification"));
        }
        let cpu = placement_guidance("ollama", None);
        assert!(!cpu.contains("CUDA_VISIBLE_DEVICES"));
        assert!(cpu.contains("does not edit the upstream service or firewall"));
    }

    #[test]
    fn generated_profiles_keep_presence_and_capacity_guards() {
        let mut a = answers();
        for gpu in [true, false] {
            if !gpu {
                a.gpu_uuid = None;
            }
            let text = render(&a, "borrow.token").unwrap();
            let cfg: Config = toml::from_str(&text).unwrap();
            cfg.validate().unwrap();
            assert!(cfg.owner.unlocked_session_is_presence);
            assert_eq!(cfg.owner.idle_lends_after_s, 0);
            assert_eq!(cfg.owner.gpu_pressure.enabled, gpu);
            assert_eq!(
                cfg.upstreams[0].resource,
                if gpu { "gpu" } else { "memory" }
            );
            assert!(!cfg.upstreams[0].yield_on_veto);
            assert_eq!(
                cfg.upstreams[0].on_owner_return,
                crate::config::ReclaimMode::Drain
            );
            assert_eq!(cfg.upstreams[0].drain_timeout_s, 30);
            assert_eq!(cfg.resources[0].reserve, Some(4096 * 1024 * 1024));
        }
    }

    fn record(state: &crate::state::AppState, id: &str, capacity: crate::plugin::probe::Capacity) {
        let resource = state.config.resources.iter().find(|r| r.id == id).unwrap();
        let runtime = &state.resources[id];
        let lendable = runtime
            .probe
            .lock()
            .unwrap()
            .lendable(&capacity, resource.reserve);
        let mut observation = runtime.capacity.write().unwrap();
        runtime
            .probe_lendable
            .store(lendable, std::sync::atomic::Ordering::Relaxed);
        observation.record_success(capacity);
    }

    #[test]
    fn cpu_only_setup_enforces_the_ram_per_job_on_top_of_the_owner_reserve() {
        const MIB: u64 = 1024 * 1024;
        let mut a = answers();
        a.gpu_uuid = None;
        a.min_free_mib = 8192;
        a.ram_reserve_mib = 4096;
        let mut cfg: Config = toml::from_str(&render(&a, "borrow.token").unwrap()).unwrap();
        cfg.auth.tokens.clear();
        assert_eq!(cfg.upstreams[0].resource, "memory");
        let state = crate::state::AppState::new(cfg).unwrap();
        state.mark_presence_sampled();
        record(
            &state,
            "cpu",
            crate::plugin::probe::Capacity {
                utilization_pct: Some(10),
                ..Default::default()
            },
        );
        let upstream = &state.config.upstreams[0];
        for (free_mib, admitted) in [
            // Below the reserve: not lendable at all.
            (4095, false),
            // The reserve and the job each fit, but not together.
            (8192, false),
            (4096 + 8191, false),
            // Exactly the reserve plus the job.
            (4096 + 8192, true),
            (65536, true),
        ] {
            record(
                &state,
                "memory",
                crate::plugin::probe::Capacity {
                    free_bytes: free_mib * MIB,
                    total_bytes: 65536 * MIB,
                    ..Default::default()
                },
            );
            let decision = crate::gate::admit(&state, upstream, Some("any-model"));
            assert_eq!(decision.is_ok(), admitted, "{free_mib} MiB: {decision:?}");
            if let Err(refusal) = decision {
                assert_eq!(
                    refusal.reason,
                    crate::gate::RefusalReason::InsufficientCapacity
                );
                assert_eq!(refusal.resource.as_deref(), Some("memory"));
            }
        }
    }

    #[test]
    fn setup_refuses_a_budget_the_hardware_can_never_hold() {
        let cpu_only = |min_free_mib, ram_reserve_mib| {
            let mut a = answers();
            a.gpu_uuid = None;
            a.min_free_mib = min_free_mib;
            a.ram_reserve_mib = ram_reserve_mib;
            a
        };
        // 65536 MiB of RAM: a job and the reserve must fit together.
        assert!(render(&cpu_only(60000, 4096), "t").is_ok());
        let error = render(&cpu_only(62000, 4096), "t").unwrap_err().to_string();
        assert!(error.contains("no job could ever be admitted"), "{error}");
        assert!(render(&cpu_only(1_000_000_000_000, 0), "t").is_err());
        assert!(render(&cpu_only(1, 65536), "t").is_err());
        // On a 12288 MiB GPU the job budgets VRAM; RAM keeps only the reserve.
        let mut gpu = answers();
        gpu.min_free_mib = 12288;
        gpu.ram_reserve_mib = 60000;
        assert!(render(&gpu, "t").is_ok());
        gpu.min_free_mib = 12289;
        assert!(render(&gpu, "t").is_err());
        gpu.min_free_mib = 4096;
        gpu.ram_reserve_mib = 65536;
        assert!(render(&gpu, "t").is_err());
        // Without a hardware reading, only the rendered configuration is checked.
        let mut unknown = cpu_only(1_000_000, 0);
        unknown.ram_total_mib = None;
        assert!(render(&unknown, "t").is_ok());
    }

    #[test]
    fn pressure_choices_round_trip_and_fixed_requires_a_valid_threshold() {
        for mode in [PressureMode::Calibrated, PressureMode::Fixed] {
            let mut a = answers();
            a.pressure_mode = mode;
            a.threshold_pct = if mode == PressureMode::Fixed {
                35.0
            } else {
                0.0
            };
            let cfg: Config = toml::from_str(&render(&a, "borrow.token").unwrap()).unwrap();
            assert_eq!(cfg.owner.gpu_pressure.mode, mode);
            assert_eq!(cfg.owner.gpu_pressure.threshold_pct, a.threshold_pct);
        }
        let mut a = answers();
        a.pressure_mode = PressureMode::Fixed;
        for invalid in [0.0, -1.0, 100.1, f64::INFINITY, f64::NAN] {
            a.threshold_pct = invalid;
            assert!(render(&a, "borrow.token").is_err());
        }
    }

    #[test]
    fn migration_preserves_bytes_credentials_and_original_source() {
        let root = Directory::new();
        let source = root.0.join("legacy");
        let destination = root.0.join("vacancyd");
        std::fs::create_dir(&source).unwrap();
        std::fs::create_dir(&destination).unwrap();
        let original = install(&answers(), &source.join("config.toml"), false).unwrap();
        let bytes = std::fs::read(&original.config).unwrap();
        let token = std::fs::read(&original.token).unwrap();
        let target = destination.join("vacancyd.toml");
        std::fs::write(
            &target,
            include_bytes!("../packaging/vacancyd.starter.toml"),
        )
        .unwrap();
        migrate(&original.config, &target, true).unwrap();
        assert_eq!(std::fs::read(&target).unwrap(), bytes);
        assert_eq!(std::fs::read(&original.config).unwrap(), bytes);
        assert_eq!(
            std::fs::read(destination.join(original.token.file_name().unwrap())).unwrap(),
            token
        );
        crate::auth::Authenticator::new(&Config::load(&target).unwrap().auth).unwrap();
        #[cfg(windows)]
        crate::windows_acl::verify_config(&target).unwrap();
    }

    #[test]
    fn migration_carries_an_upstream_credential_file() {
        let root = Directory::new();
        let source = root.0.join("legacy");
        let destination = root.0.join("vacancyd");
        std::fs::create_dir(&source).unwrap();
        std::fs::create_dir(&destination).unwrap();
        let original = install(&answers(), &source.join("config.toml"), false).unwrap();
        let mut text = std::fs::read_to_string(&original.config).unwrap();
        text.push_str(
            "\n[upstream.credential]\nscheme = \"bearer\"\nvalue_file = \"upstream.key\"\n",
        );
        std::fs::write(&original.config, &text).unwrap();
        std::fs::write(source.join("upstream.key"), "fake-upstream-secret\n").unwrap();
        let target = destination.join("vacancyd.toml");
        migrate(&original.config, &target, false).unwrap();
        assert_eq!(
            std::fs::read(destination.join("upstream.key")).unwrap(),
            b"fake-upstream-secret\n"
        );
        let credentials = Config::load(&target)
            .unwrap()
            .resolve_credentials()
            .unwrap();
        assert_eq!(credentials.upstreams.len(), 1);

        // Without its file the migration fails and leaves nothing behind.
        let again = root.0.join("again").join("vacancyd");
        std::fs::create_dir_all(&again).unwrap();
        std::fs::remove_file(source.join("upstream.key")).unwrap();
        let error = migrate(&original.config, &again.join("vacancyd.toml"), false).unwrap_err();
        assert!(!format!("{error:#}").contains("fake-upstream-secret"));
        assert!(!again.join("vacancyd.toml").exists());
        assert!(!again.join("upstream.key").exists());
    }

    /// A configuration whose recovery directory is the default (or relative)
    /// resolves it beside itself, so copying it elsewhere used to leave held
    /// work behind and start the service with an empty ledger.
    #[test]
    fn migration_refuses_to_strand_held_work_and_names_the_setting_that_keeps_it() {
        let root = Directory::new();
        let source = root.0.join("legacy");
        std::fs::create_dir(&source).unwrap();
        // Each migration gets its own destination, which Windows requires to
        // be a directory named vacancyd.
        let target = |name: &str| {
            let destination = root.0.join(name).join("vacancyd");
            std::fs::create_dir_all(&destination).unwrap();
            destination.join("vacancyd.toml")
        };
        let original = install(&answers(), &source.join("config.toml"), false).unwrap();
        let records = source.join("config.admission");
        std::fs::create_dir(&records).unwrap();
        // The lock and the learned-context cache are not held work.
        std::fs::write(records.join(".lock"), b"").unwrap();
        std::fs::write(records.join("learned-contexts.json"), b"{}").unwrap();
        migrate_with(&original.config, &target("idle"), false, None).unwrap();

        std::fs::write(records.join("entry-1.json"), b"{}").unwrap();
        let refused = target("held");
        let error = migrate_with(&original.config, &refused, false, None).unwrap_err();
        let message = format!("{error:#}");
        assert!(
            message.contains("holds records of unfinished work"),
            "{message}"
        );
        assert!(message.contains("Nothing was copied"), "{message}");
        assert!(!refused.exists());
        assert!(!refused.with_file_name(".vacancyd-setup.lock").exists());
        assert!(records.join("entry-1.json").exists());

        // Under a service manager's state directory both copies resolve to
        // the same records, so nothing is stranded.
        let state = root.0.join("state");
        migrate_with(
            &original.config,
            &target("service"),
            false,
            Some(state.as_os_str()),
        )
        .unwrap();

        // Naming the directory, as the refusal says, keeps the records.
        let named = std::path::absolute(&records).unwrap().display().to_string();
        let text = std::fs::read_to_string(&original.config).unwrap().replacen(
            "[server]\n",
            &format!("[server]\nadmission_state_dir = '{named}'\n"),
            1,
        );
        std::fs::write(&original.config, text).unwrap();
        let carried = target("named");
        migrate_with(&original.config, &carried, false, None).unwrap();
        assert_eq!(
            Config::load(&carried).unwrap().server.admission_state_dir,
            named
        );
    }

    #[test]
    fn failed_migration_never_replaces_config_or_an_existing_credential() {
        let root = Directory::new();
        let source = root.0.join("legacy");
        let destination = root.0.join("vacancyd");
        std::fs::create_dir(&source).unwrap();
        std::fs::create_dir(&destination).unwrap();
        let original = install(&answers(), &source.join("config.toml"), false).unwrap();
        let target = destination.join("vacancyd.toml");
        let starter = include_bytes!("../packaging/vacancyd.starter.toml");
        std::fs::write(&target, starter).unwrap();
        let token = destination.join(original.token.file_name().unwrap());
        std::fs::write(&token, b"someone else's file").unwrap();
        assert!(migrate(&original.config, &target, true).is_err());
        assert_eq!(std::fs::read(&target).unwrap(), starter);
        assert_eq!(std::fs::read(&token).unwrap(), b"someone else's file");
        assert!(!destination.join(".vacancyd-setup.lock").exists());
    }

    #[cfg(unix)]
    #[test]
    fn migration_refuses_a_destination_another_account_could_redirect_and_says_why() {
        use std::os::unix::fs::PermissionsExt;
        let root = Directory::new();
        let source = root.0.join("legacy");
        std::fs::create_dir(&source).unwrap();
        let original = install(&answers(), &source.join("config.toml"), false).unwrap();
        // Another account controls the parent the destination would be made in.
        let open = root.0.join("shared");
        std::fs::create_dir(&open).unwrap();
        std::fs::set_permissions(&open, std::fs::Permissions::from_mode(0o777)).unwrap();
        let destination = open.join("vacancyd");

        let error =
            migrate(&original.config, &destination.join("vacancyd.toml"), false).unwrap_err();

        let message = format!("{error:#}");
        assert!(
            message.contains("creating the destination directory"),
            "{message}"
        );
        assert!(message.contains("writable by group or others"), "{message}");
        assert!(!destination.exists());
    }

    #[cfg(windows)]
    #[test]
    fn windows_setup_and_migration_refuse_a_private_directory_outside_the_protected_folder() {
        let root = Directory::new();
        let source = root.0.join("legacy");
        std::fs::create_dir(&source).unwrap();
        let original = install(&answers(), &source.join("config.toml"), false).unwrap();
        // Private to Administrators and LocalSystem, and existing: only its
        // place outside the protected folder is wrong.
        let outside = crate::secure_file::protected_directory_outside_test_root();
        let destination = outside.join("vacancyd");
        crate::windows_acl::create_protected_directory(&destination).unwrap();

        let error = install(&answers(), &outside.join("config.toml"), false).unwrap_err();
        let message = format!("{error:#}");
        assert!(
            message.contains("refusing the configuration directory"),
            "{message}"
        );
        assert!(message.contains("is not inside"), "{message}");
        let error =
            migrate(&original.config, &destination.join("vacancyd.toml"), false).unwrap_err();
        let message = format!("{error:#}");
        assert!(
            message.contains("refusing the destination directory"),
            "{message}"
        );
        assert!(message.contains("is not inside"), "{message}");

        assert_eq!(std::fs::read_dir(&destination).unwrap().count(), 0);
        std::fs::remove_dir(&destination).unwrap();
        assert_eq!(std::fs::read_dir(&outside).unwrap().count(), 0);
        std::fs::remove_dir(outside).unwrap();
    }

    /// Canonicalizing the destination used to remove the junction before
    /// the path was checked, so a user's junction could send an elevated
    /// migration to an unrelated directory named vacancyd.
    #[cfg(windows)]
    #[test]
    fn windows_setup_and_migration_refuse_a_destination_reached_through_a_junction() {
        let root = Directory::new();
        let source = root.0.join("legacy");
        std::fs::create_dir(&source).unwrap();
        let original = install(&answers(), &source.join("config.toml"), false).unwrap();
        let unrelated = root.0.join("unrelated").join("vacancyd");
        std::fs::create_dir_all(&unrelated).unwrap();
        let link = root.0.join("redirect");
        crate::secure_file::junction(&link, &unrelated);

        let error = migrate(&original.config, &link.join("vacancyd.toml"), false).unwrap_err();
        let message = format!("{error:#}");
        assert!(message.contains("reparse point"), "{message}");
        assert!(install(&answers(), &link.join("config.toml"), false).is_err());

        assert_eq!(std::fs::read_dir(&unrelated).unwrap().count(), 0);
        std::fs::remove_dir(&link).unwrap();
    }

    /// An empty destination that ordinary users may write the attributes of
    /// could become a junction between its check and migration's first
    /// write. It is refused, and its permissions are left as they were.
    #[cfg(windows)]
    #[test]
    fn windows_migration_refuses_an_existing_destination_other_accounts_can_write() {
        let root = Directory::new();
        let source = root.0.join("legacy");
        std::fs::create_dir(&source).unwrap();
        let original = install(&answers(), &source.join("config.toml"), false).unwrap();
        let destination = root.0.join("vacancyd");
        std::fs::create_dir(&destination).unwrap();
        let granted = std::process::Command::new("icacls")
            .arg(&destination)
            .args(["/grant", "*S-1-5-32-545:(WA)"])
            .output()
            .unwrap();
        assert!(granted.status.success(), "{granted:?}");

        let error =
            migrate(&original.config, &destination.join("vacancyd.toml"), false).unwrap_err();
        let message = format!("{error:#}");
        assert!(message.contains("access is granted to"), "{message}");

        assert_eq!(std::fs::read_dir(&destination).unwrap().count(), 0);
        let sddl = crate::windows_acl::descriptor_of(&destination).unwrap();
        assert!(sddl.contains(";BU)"), "permissions were rewritten: {sddl}");
    }

    #[test]
    fn malformed_migration_does_not_echo_secret_input() {
        let root = Directory::new();
        let source = root.0.join("bad.toml");
        std::fs::write(&source, b"secret-value-that-must-not-be-printed = [").unwrap();
        let error =
            migrate(&source, &root.0.join("vacancyd").join("config.toml"), false).unwrap_err();
        assert!(!format!("{error:#}").contains("secret-value"));
        assert!(!root.0.join("vacancyd").exists());
    }

    #[test]
    fn unsafe_or_incomplete_answers_create_nothing() {
        let dir = Directory::new();
        for target in [
            "http://192.0.2.4:8080",
            "http://localhost:8080",
            "https://127.0.0.1:8080",
            "http://user:password@127.0.0.1:8080",
        ] {
            let mut a = answers();
            a.target = target.into();
            assert!(install(&a, &dir.0.join("config.toml"), false).is_err());
            assert!(dir.files().is_empty());
        }
        let mut a = answers();
        a.min_free_mib = 0;
        assert!(install(&a, &dir.0.join("config.toml"), false).is_err());
        assert!(dir.files().is_empty());
        a = answers();
        a.kind = "ollama\n[[upstream]]\nid='injected'".into();
        assert!(render(&a, "borrow.token").is_err());
        a = answers();
        a.listen = "127.0.0.1:9811".parse().unwrap();
        assert!(render(&a, "borrow.token").is_err());
    }

    #[test]
    fn new_configuration_resolves_a_private_borrow_only_credential() {
        let dir = Directory::new();
        let installed = install(&answers(), &dir.0.join("config.toml"), false).unwrap();
        assert!(installed.backup.is_none());
        assert_eq!(
            dir.files().len(),
            2,
            "only config and credential should remain"
        );
        let cfg = Config::load(&installed.config).unwrap();
        let auth = crate::auth::Authenticator::new(&cfg.auth).unwrap();
        let token = std::fs::read_to_string(&installed.token).unwrap();
        assert!(!std::fs::read_to_string(&installed.config)
            .unwrap()
            .contains(&token));
        let mut headers = http::HeaderMap::new();
        headers.insert(
            http::header::AUTHORIZATION,
            format!("Bearer {token}").parse().unwrap(),
        );
        let peer = "192.0.2.2".parse().unwrap();
        assert!(auth
            .check(crate::auth::Scope::Borrow, &headers, peer, None)
            .is_ok());
        assert!(auth
            .check(crate::auth::Scope::Admin, &headers, peer, None)
            .is_err());
        assert!(auth
            .check(crate::auth::Scope::Read, &headers, peer, None)
            .is_err());
    }

    #[test]
    fn only_the_exact_starter_can_be_replaced_and_it_is_backed_up() {
        let dir = Directory::new();
        let path = dir.0.join("config.toml");
        let original = include_bytes!("../packaging/vacancyd.starter.toml");
        std::fs::write(&path, original).unwrap();
        assert!(install(&answers(), &path, false).is_err());
        assert_eq!(dir.files(), vec!["config.toml"]);
        let installed = install(&answers(), &path, true).unwrap();
        assert_eq!(std::fs::read(installed.backup.unwrap()).unwrap(), original);
        let before = std::fs::read(&path).unwrap();
        let files = dir.files();
        assert!(install(&answers(), &path, true).is_err());
        assert_eq!(std::fs::read(&path).unwrap(), before);
        assert_eq!(dir.files(), files);
    }

    #[test]
    fn concurrent_setup_does_not_clobber_or_remove_another_transactions_files() {
        let dir = Directory::new();
        let path = dir.0.join("config.toml");
        let barrier = std::sync::Arc::new(std::sync::Barrier::new(2));
        let threads: Vec<_> = (0..2)
            .map(|_| {
                let barrier = barrier.clone();
                let path = path.clone();
                std::thread::spawn(move || {
                    barrier.wait();
                    install(&answers(), &path, false).is_ok()
                })
            })
            .collect();
        let successes = threads
            .into_iter()
            .map(|thread| usize::from(thread.join().unwrap()))
            .sum::<usize>();
        assert_eq!(successes, 1);
        assert_eq!(dir.files().len(), 2);
        Config::load(&path).unwrap();
    }

    #[test]
    fn failed_transaction_removes_only_the_files_it_created() {
        let dir = Directory::new();
        let existing = dir.0.join("keep");
        std::fs::write(&existing, b"untouched").unwrap();
        {
            let mut transaction = Transaction {
                temporary: vec![],
                permanent: vec![],
                committed: false,
            };
            transaction
                .write(&dir.0.join("staged"), b"staged", false)
                .unwrap();
            transaction
                .write(&dir.0.join("credential"), b"test-only", true)
                .unwrap();
            assert!(transaction.write(&existing, b"replacement", true).is_err());
        }
        assert_eq!(dir.files(), vec!["keep"]);
        assert_eq!(std::fs::read(existing).unwrap(), b"untouched");
    }

    #[test]
    fn cancelled_input_never_becomes_confirmation() {
        let mut output = Vec::new();
        assert!(!confirm(&mut &b"\n"[..], &mut output, "Write?").unwrap());
        assert!(!confirm(&mut &b"maybe\n"[..], &mut output, "Write?").unwrap());
        assert!(confirm(&mut &b""[..], &mut output, "Write?").is_err());
    }

    #[tokio::test]
    async fn discovery_accepts_expected_json_but_rejects_oversized_bodies() {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};
        for (response, expected) in [
            (
                "HTTP/1.1 200 OK\r\nContent-Length: 17\r\n\r\n{\"version\":\"0.1\"}".to_string(),
                true,
            ),
            (
                format!(
                    "HTTP/1.1 200 OK\r\nContent-Length: 70014\r\n\r\n{{\"version\":\"{}\"}}",
                    "x".repeat(70000)
                ),
                false,
            ),
        ] {
            let server = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
            let address = server.local_addr().unwrap();
            let task = tokio::spawn(async move {
                let (mut socket, _) = server.accept().await.unwrap();
                let mut buffer = [0u8; 1024];
                assert!(socket.read(&mut buffer).await.unwrap() > 0);
                let _ = socket.write_all(response.as_bytes()).await;
            });
            let client = discovery_client().unwrap();
            let result = detect(
                &client,
                "ollama",
                &format!("http://{address}"),
                "/api/version",
            )
            .await;
            assert_eq!(result.is_some(), expected);
            task.await.unwrap();
        }
    }

    #[tokio::test]
    async fn discovery_never_contacts_a_redirect_destination() {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};
        let destination = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let target = destination.local_addr().unwrap();
        let redirect = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = redirect.local_addr().unwrap();
        let contacted = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
        let observed = contacted.clone();
        let destination_task = tokio::spawn(async move {
            let (mut socket, _) = destination.accept().await.unwrap();
            observed.store(true, std::sync::atomic::Ordering::SeqCst);
            let mut buffer = [0u8; 1024];
            assert!(socket.read(&mut buffer).await.unwrap() > 0);
            let _ = socket
                .write_all(b"HTTP/1.1 200 OK\r\nContent-Length: 17\r\n\r\n{\"version\":\"0.1\"}")
                .await;
        });
        let task = tokio::spawn(async move {
            let (mut socket, _) = redirect.accept().await.unwrap();
            let mut buffer = [0u8; 1024];
            assert!(socket.read(&mut buffer).await.unwrap() > 0);
            socket.write_all(format!("HTTP/1.1 302 Found\r\nLocation: http://{target}/\r\nContent-Length: 0\r\n\r\n").as_bytes()).await.unwrap();
        });
        let result = detect(
            &discovery_client().unwrap(),
            "ollama",
            &format!("http://{address}"),
            "/api/version",
        )
        .await;
        assert!(result.is_none());
        assert!(!contacted.load(std::sync::atomic::Ordering::SeqCst));
        task.await.unwrap();
        destination_task.abort();
    }

    #[tokio::test]
    async fn discovery_deadline_includes_a_stalled_response_body() {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};
        let server = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = server.local_addr().unwrap();
        let task = tokio::spawn(async move {
            let (mut socket, _) = server.accept().await.unwrap();
            let mut buffer = [0u8; 1024];
            assert!(socket.read(&mut buffer).await.unwrap() > 0);
            socket
                .write_all(b"HTTP/1.1 200 OK\r\nContent-Length: 17\r\n\r\n{")
                .await
                .unwrap();
            std::future::pending::<()>().await;
            drop(socket);
        });
        let result = tokio::time::timeout(
            Duration::from_secs(5),
            detect(
                &discovery_client().unwrap(),
                "ollama",
                &format!("http://{address}"),
                "/api/version",
            ),
        )
        .await
        .expect("discovery must not hang on response bodies");
        assert!(result.is_none());
        task.abort();
    }
}
