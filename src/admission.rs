//! Resource ownership outlives the HTTP connection. Never use Drop, a timeout,
//! a free-memory increase or an empty queue as proof that a job has finished.
use std::collections::{BTreeMap, BTreeSet};
use std::sync::Arc;
use std::time::{Duration, Instant};

use crate::config::UpstreamConfig;
use crate::gate::{Demand, Refusal, RefusalReason};
use crate::plugin::adapter::{Completion, RouteFamily, RunnerRequest};
use crate::state::AppState;

const FRESH_CAPACITY_WAIT: Duration = if cfg!(test) {
    Duration::from_millis(250)
} else {
    Duration::from_secs(4)
};
const POST_JOB_SAMPLE: &str = "waiting for a capacity sample taken after the previous job";
const UPSTREAM_BUSY: &str = "upstream reports running or queued work";

/// How often a tracker reads a tracked job's history.
const TRACK_POLL: Duration = if cfg!(test) {
    Duration::from_millis(100)
} else {
    Duration::from_secs(1)
};
/// How often a tracker also reads the queue for a job whose absence can
/// show it ended. The queue can be large, so less often than the history.
const TRACK_QUEUE_EVERY: Duration = if cfg!(test) {
    Duration::from_millis(300)
} else {
    Duration::from_secs(5)
};
/// Consecutive queue checks that must find a job neither queued, running
/// nor finished before its reservation is released.
const TRACK_ABSENT_CHECKS: u32 = 2;
/// How long after a submission's connection failed, before any reply, the
/// upstream may still be validating it and about to queue it; absence counts
/// only after this. It is five minutes, sixty queue intervals rather than
/// the minimum two, because ComfyUI's validation runs custom node checks and
/// slows with the host's load, and a longer wait costs only a delay in a rare
/// failure.
const TRACK_LOST_GRACE: Duration = if cfg!(test) {
    Duration::from_millis(600)
} else {
    Duration::from_secs(5 * 60)
};

/// What [`Ledger::open`] does when the recovery directory does not exist.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Missing {
    /// Create it, private to the service: the default location on first start.
    Create,
    /// Refuse to start. A directory the configuration names that is not there
    /// means the setting or the configuration moved, and an empty ledger
    /// would forget the held work recorded in the old place.
    Refuse,
}

/// Where the admission recovery records live, and whether a missing
/// directory there may be created.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RecoveryDir {
    pub path: std::path::PathBuf,
    pub missing: Missing,
}

impl RecoveryDir {
    /// Resolve `server.admission_state_dir` for the configuration at
    /// `config_path`. `state_directory` is the service manager's state
    /// directory, systemd's `$STATE_DIRECTORY`, when there is one.
    ///
    /// Empty selects the default: `<state_directory>/admission`, or else the
    /// configuration's path with the extension `.admission`. A relative value
    /// resolves against the configuration's directory. Only the default
    /// location is created when missing, whether an empty value selected it
    /// or the value names it, as the packaged Linux instructions do.
    pub fn resolve(
        config: &crate::config::Config,
        config_path: &std::path::Path,
        state_directory: Option<&std::ffi::OsStr>,
    ) -> Self {
        let default = state_directory
            .map(|directory| std::path::PathBuf::from(directory).join("admission"))
            .unwrap_or_else(|| config_path.with_extension("admission"));
        let configured = &config.server.admission_state_dir;
        if configured.is_empty() {
            return Self {
                path: default,
                missing: Missing::Create,
            };
        }
        let path = std::path::PathBuf::from(configured);
        let path = if path.is_absolute() {
            path
        } else {
            config_path
                .parent()
                .unwrap_or(std::path::Path::new("."))
                .join(path)
        };
        let missing = if path == default {
            Missing::Create
        } else {
            Missing::Refuse
        };
        Self { path, missing }
    }
}

/// Where the packaged systemd unit keeps the service's recovery records:
/// `StateDirectory=vacancyd` gives the service `$STATE_DIRECTORY` =
/// `/var/lib/vacancyd`, and an empty `admission_state_dir` resolves there.
#[cfg(target_os = "linux")]
pub const SERVICE_RECOVERY_DIR: &str = "/var/lib/vacancyd/admission";

/// Check a run started outside the service manager against the installed
/// service's recovery records in `service_dir`.
///
/// With `admission_state_dir` empty, the service keeps its records under its
/// `$STATE_DIRECTORY`, but a run without that variable, such as `sudo
/// vacancyd run` by hand, keeps them beside its configuration. Work held in
/// the service's records would then be invisible to it, and it could lend
/// hardware that work may still occupy. So when `service_dir` holds records
/// this refuses, naming the setting that makes both use one location. When
/// it exists without records, or cannot be read, the returned warning says
/// the two locations differ. A set `admission_state_dir` means the same
/// location however vacancyd starts, and a `state_directory` means this run
/// is the service, so neither is checked.
#[cfg(any(target_os = "linux", test))]
pub fn check_beside_service(
    config: &crate::config::Config,
    recovery: &RecoveryDir,
    state_directory: Option<&std::ffi::OsStr>,
    service_dir: &std::path::Path,
) -> anyhow::Result<Option<String>> {
    if !config.server.admission_state_dir.is_empty()
        || state_directory.is_some()
        || recovery.path == service_dir
    {
        return Ok(None);
    }
    let setting = format!("admission_state_dir = '{}'", service_dir.display());
    let unreadable = |error: std::io::Error| {
        format!(
            "could not read the installed service's admission recovery records in {} ({error}). This run cannot tell whether they hold work and keeps its own records in {}. Run it as the service's account, or set {setting} under [server] so every start uses the service's records",
            service_dir.display(),
            recovery.path.display(),
        )
    };
    match std::fs::symlink_metadata(service_dir) {
        Ok(_) => {}
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(error) => return Ok(Some(unreadable(error))),
    }
    match holds_records(service_dir) {
        Ok(true) => anyhow::bail!(
            "the installed service's admission recovery records in {} hold unfinished or held work. This run would keep its records in {} and could lend hardware that work may still occupy. Start vacancyd as the service, or set {setting} under [server] so every start uses those records",
            service_dir.display(),
            recovery.path.display(),
        ),
        Ok(false) => Ok(Some(format!(
            "the installed service keeps its admission recovery records in {}, but this run keeps its own in {}; work one of them holds is invisible to the other. Set {setting} under [server] so every start uses one location",
            service_dir.display(),
            recovery.path.display(),
        ))),
        Err(error) => Ok(Some(unreadable(error))),
    }
}

/// Whether the recovery directory at `path` holds anything [`Ledger::open`]
/// would read as a record of held or unfinished work. A missing directory
/// holds none. The lock and the learned-context cache are not records.
pub fn holds_records(path: &std::path::Path) -> std::io::Result<bool> {
    let entries = match std::fs::read_dir(path) {
        Ok(entries) => entries,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(false),
        Err(error) => return Err(error),
    };
    for entry in entries {
        let name = entry?.file_name();
        if name != ".lock" && name != CONTEXTS_FILE && name != CONTEXTS_TEMP {
            return Ok(true);
        }
    }
    Ok(false)
}

/// Refuse a recovery directory that the service account does not own. Its
/// mode alone does not make it private to the service: another account's
/// private directory is that account's to change.
#[cfg(unix)]
fn ensure_owned_by(
    path: &std::path::Path,
    meta: &std::fs::Metadata,
    service_uid: u32,
) -> anyhow::Result<()> {
    use std::os::unix::fs::MetadataExt;
    anyhow::ensure!(
        meta.uid() == service_uid,
        "admission directory {} is owned by uid {}, not by the account vacancyd runs as (uid {service_uid}); vacancyd does not change an existing directory's owner. Make that account its owner, or point server.admission_state_dir at a directory it owns",
        path.display(),
        meta.uid()
    );
    Ok(())
}

/// The recovery directory at `path`, created when it is missing and
/// `missing` allows it. `Ok(true)` when it was created here. On Unix, its
/// parent is synced whether it was created now or found, so a failed sync
/// after creation must succeed on a later start before work is admitted.
/// The parent sync is a no-op on Windows.
fn recovery_directory(
    path: &std::path::Path,
    missing: Missing,
    sync_dir: impl FnOnce(&std::path::Path) -> std::io::Result<()>,
) -> anyhow::Result<bool> {
    match std::fs::symlink_metadata(path) {
        Ok(meta) => {
            anyhow::ensure!(
                meta.is_dir() && !meta.file_type().is_symlink(),
                "admission state must be a real directory"
            );
            sync_parent(path, sync_dir)?;
            Ok(false)
        }
        Err(e) if e.kind() == std::io::ErrorKind::NotFound && missing == Missing::Refuse => {
            anyhow::bail!(
                "{} does not exist. server.admission_state_dir names it, but vacancyd creates only its default recovery directory. This prevents a moved configuration, a changed setting or a typo from starting without the records of held work. Point admission_state_dir at the directory that holds the records or, for a new installation, create this one empty and private to the account the service runs as",
                path.display()
            )
        }
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
            create_recovery_directory(path, sync_dir)?;
            Ok(true)
        }
        Err(e) => Err(e.into()),
    }
}

/// Create a missing recovery directory, private on Unix from the start.
/// On Unix, sync its parent before writing recovery records so the new
/// directory entry reaches storage. `sync_dir` is
/// [`crate::durable::sync_dir`] outside tests and is a no-op on Windows.
fn create_recovery_directory(
    path: &std::path::Path,
    sync_dir: impl FnOnce(&std::path::Path) -> std::io::Result<()>,
) -> anyhow::Result<()> {
    crate::secure_file::create_private_dir_all(path)?;
    sync_parent(path, sync_dir)
}

fn sync_parent(
    path: &std::path::Path,
    sync_dir: impl FnOnce(&std::path::Path) -> std::io::Result<()>,
) -> anyhow::Result<()> {
    let parent = path
        .parent()
        .filter(|parent| !parent.as_os_str().is_empty())
        .unwrap_or_else(|| std::path::Path::new("."));
    sync_dir(parent).map_err(|error| {
        anyhow::Error::new(error).context(format!(
            "making the admission directory {} durable in {}",
            path.display(),
            parent.display()
        ))
    })
}

#[derive(Default)]
pub struct Ledger {
    /// Live capacity publication shares this ledger's admission mutex. Leases,
    /// reclaim owners and startup topology are never rewritten by tuning.
    capacity: Option<CapacityTuning>,
    next: u64,
    journal: Option<std::path::PathBuf>,
    _journal_lock: Option<std::fs::File>,
    entries: BTreeMap<u64, Entry>,
    after: BTreeMap<String, Instant>,
    yielding: BTreeMap<String, usize>,
    pending_yields: BTreeSet<String>,
    pending_reclaims: BTreeSet<String>,
    priors: BTreeMap<(String, String), u64>,
    measurements: Vec<Entry>,
    contexts: BTreeMap<ContextKey, LearnedContext>,
    pending_contexts: Vec<PendingContext>,
}

#[derive(Clone)]
struct CapacityTuning {
    revision: String,
    upstreams: BTreeMap<String, (u64, BTreeMap<String, crate::config::ModelRequirement>)>,
}

impl Ledger {
    pub(crate) fn publish_capacity(&mut self, config: &crate::config::Config, revision: String) {
        self.capacity = Some(CapacityTuning {
            revision,
            upstreams: config
                .upstreams
                .iter()
                .map(|upstream| {
                    (
                        upstream.id.clone(),
                        (upstream.default_min_free_mib, upstream.models.clone()),
                    )
                })
                .collect(),
        });
    }

    pub(crate) fn capacity_revision(&self) -> Option<&str> {
        self.capacity
            .as_ref()
            .map(|capacity| capacity.revision.as_str())
    }

    /// Overlay only live-safe capacity on the startup route. Applying saved
    /// dependencies here would reinterpret leases and cancellation ownership.
    pub(crate) fn effective_upstream(&self, startup: &UpstreamConfig) -> UpstreamConfig {
        let mut effective = startup.clone();
        if let Some((default, models)) = self
            .capacity
            .as_ref()
            .and_then(|capacity| capacity.upstreams.get(&startup.id))
        {
            effective.default_min_free_mib = *default;
            effective.models = models.clone();
        }
        effective
    }
}

/// A reservation that is still present, as owner-return cancellation sees it.
pub struct Remaining {
    pub id: u64,
    pub tracking_id: Option<String>,
    pub held: bool,
}

/// Work that owner-return cancellation takes on: its upstream job id, and
/// whether the upstream acknowledged a submission under that id.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CancelClaim {
    pub entry: u64,
    pub tracking_id: String,
    pub confirmed: bool,
}

/// Upstream, canonical model and the runner parameters a request set.
type ContextKey = (String, String, RunnerRequest);

/// The context an upstream loaded a model with for one kind of request,
/// learned from the first model observation after such a request finished.
/// It outlives unloads: only the residency proof itself must be fresh.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct LearnedContext {
    pub context_length: u64,
    /// The model digest the context was learned for, when reported. A
    /// different digest is a different model under the same name.
    pub digest: Option<String>,
}

/// A finished request whose loaded context has not been observed yet.
struct PendingContext {
    key: ContextKey,
    finished: Instant,
    /// When a later request on the upstream was admitted. An observation
    /// from then on may describe the runner that request loaded instead.
    superseded: Option<Instant>,
}

/// Learned contexts are kept beside the recovery records, so a restart does
/// not forget what requests were seen to load while a model stays resident.
/// The file is a cache, not state of record: losing it only means a resident
/// model is charged a cold load until a request teaches its context again.
const CONTEXTS_FILE: &str = "learned-contexts.json";
const CONTEXTS_TEMP: &str = "learned-contexts.json.tmp";
const CONTEXTS_FILE_MAX: u64 = 1024 * 1024;
const CONTEXTS_VERSION: u32 = 1;

#[derive(serde::Serialize, serde::Deserialize)]
#[serde(deny_unknown_fields)]
struct ContextsFile {
    version: u32,
    contexts: Vec<ContextRecord>,
}

/// One learned context on disk. Only contexts learned with a model digest
/// are kept, so a model replaced under the same name while vacancyd was
/// stopped never inherits the old model's context.
#[derive(serde::Serialize, serde::Deserialize)]
#[serde(deny_unknown_fields)]
struct ContextRecord {
    upstream: String,
    model: String,
    /// `native` or `openai`: the route family of the requests that taught it.
    route: String,
    /// The request's explicit `num_ctx`, or `null` for requests that left it
    /// to the upstream's default.
    num_ctx: Option<u64>,
    context_length: u64,
    digest: String,
}

/// How long a finished request waits for the observation that teaches its
/// loaded context.
const PENDING_CONTEXT_FOR: Duration = Duration::from_secs(30);
/// Learned contexts kept per upstream model, and in total.
const CONTEXTS_PER_MODEL: usize = 8;
const CONTEXTS_MAX: usize = 256;

struct Entry {
    upstream: String,
    model: Option<String>,
    resources: Vec<String>,
    started: Instant,
    started_unix_s: u64,
    recovered_age_s: u64,
    completion: Completion,
    tracking_id: Option<String>,
    /// The upstream acknowledged a submission under `tracking_id`. An id
    /// vacancyd assigned before forwarding is unconfirmed until the reply
    /// names it: without that reply, the job may exist under the id, may be
    /// queued later, or may never have been queued.
    tracking_confirmed: bool,
    /// Owner-return cancellation reached this entry while its submission
    /// was still in flight, so whoever resolves the submission cancels it.
    cancel_on_submit: bool,
    phase: &'static str,
    primary: String,
    free_before: u64,
    loaded_at: Option<Instant>,
    measured: bool,
    observes_models: bool,
    /// What the request asked of the runner, and its canonical model, while
    /// its outcome can still teach the context the upstream loads for such
    /// requests.
    runner: Option<(RunnerRequest, String)>,
}

fn unix_seconds() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs()
}

/// A tracking id fit to appear in an upstream URL path.
pub(crate) fn is_safe_tracking_id(id: &str) -> bool {
    !id.is_empty()
        && id.len() <= 128
        && id
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'_')
}

/// A random (version 4) UUID in canonical lowercase form, for vacancyd to
/// assign as the upstream job id of a tracked submission.
pub fn new_tracking_id() -> anyhow::Result<String> {
    let mut bytes = [0u8; 16];
    getrandom::getrandom(&mut bytes)
        .map_err(|error| anyhow::anyhow!("no random bytes for a job id: {error}"))?;
    bytes[6] = (bytes[6] & 0x0f) | 0x40;
    bytes[8] = (bytes[8] & 0x3f) | 0x80;
    let hex: String = bytes.iter().map(|b| format!("{b:02x}")).collect();
    Ok(format!(
        "{}-{}-{}-{}-{}",
        &hex[..8],
        &hex[8..12],
        &hex[12..16],
        &hex[16..20],
        &hex[20..]
    ))
}

/// The id in an `entry-<id>.json` record name.
/// The id in an `entry-<id>.next` replacement record: a newer version of
/// `entry-<id>.json` that a crash left before it was renamed into place.
fn replacement_id(name: &std::ffi::OsStr) -> Option<u64> {
    name.to_str()?
        .strip_prefix("entry-")?
        .strip_suffix(".next")?
        .parse()
        .ok()
}

fn record_id(name: &std::ffi::OsStr) -> Option<u64> {
    name.to_str()?
        .strip_prefix("entry-")?
        .strip_suffix(".json")?
        .parse()
        .ok()
}

/// A record cut short by a crash: empty, or a JSON prefix that ends early,
/// optionally followed by the zero fill some filesystems leave after a
/// crash. A complete record, or bytes that are not a JSON prefix, is not torn.
fn is_torn(bytes: &[u8]) -> bool {
    let end = bytes
        .iter()
        .rposition(|byte| *byte != 0)
        .map_or(0, |i| i + 1);
    let written = &bytes[..end];
    written.is_empty()
        || serde_json::from_slice::<serde_json::Value>(written).is_err_and(|error| error.is_eof())
}

pub(crate) fn dependencies(upstream: &UpstreamConfig) -> Vec<String> {
    let mut ids = upstream.also_requires.clone();
    ids.push(upstream.resource.clone());
    ids.sort();
    ids.dedup();
    ids
}

impl Entry {
    fn is_submitting(&self) -> bool {
        self.completion == Completion::Tracked && self.phase == "forwarding"
    }
}

fn busy(resource: &str, detail: &str) -> Refusal {
    Refusal {
        reason: RefusalReason::ResourceBusy,
        detail: detail.into(),
        resource: Some(resource.into()),
        retry_after_s: 2,
    }
}

impl Ledger {
    pub fn sync_active_metrics(&self, state: &AppState) {
        for upstream in &state.config.upstreams {
            for phase in ["forwarding", "tracking", "held"] {
                let count = self
                    .entries
                    .values()
                    .filter(|entry| entry.upstream == upstream.id && entry.phase == phase)
                    .count();
                state
                    .metrics
                    .active_work
                    .with_label_values(&[upstream.id.as_str(), phase])
                    .set(count as i64);
            }
        }
    }

    /// Open the recovery directory at `path`, holding its lock for the life
    /// of the ledger, and recover the work its records describe as held.
    /// `missing` says whether a directory that does not exist is created.
    pub fn open(
        path: &std::path::Path,
        config: &crate::config::Config,
        missing: Missing,
    ) -> anyhow::Result<Self> {
        use std::io::Read;
        // Whether it exists or is about to be created, the service writes in
        // it as LocalSystem, so it must be inside the protected folder.
        #[cfg(windows)]
        if let Some(reason) = crate::secure_file::replaceable_path(path)? {
            anyhow::bail!(
                "refusing the admission state directory {}: {reason}",
                path.display()
            );
        }
        #[cfg_attr(not(windows), allow(unused_variables))]
        let created = recovery_directory(path, missing, crate::durable::sync_dir)?;
        // A directory created here is private from creation. An existing
        // directory may contain unrelated data, so rewriting its owner and
        // ACL as LocalSystem could damage whatever else lives there. It
        // must already be private; its owner and permissions are never changed.
        #[cfg(windows)]
        if created {
            // Created protected in one call; confirm what is there now.
            crate::windows_acl::verify_directory(path)?;
        } else {
            match crate::windows_acl::verify_directory(path) {
                Ok(()) => {}
                // The directory could be read and is not private. An error
                // from Windows itself (access denied to an unelevated
                // caller, for one) keeps its OS code so the caller can
                // explain it.
                Err(error) if error.raw_os_error().is_none() => anyhow::bail!(
                    "admission state directory {} already exists and is not private to Administrators and LocalSystem; vacancyd does not change permissions it did not set. Restrict this directory as an administrator, or leave server.admission_state_dir empty to use the default location ({error})",
                    path.display()
                ),
                Err(error) => {
                    return Err(anyhow::Error::new(error).context(format!(
                        "reading the permissions of {}",
                        path.display()
                    )))
                }
            }
        }
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let meta = std::fs::metadata(path)?;
            anyhow::ensure!(
                meta.permissions().mode() & 0o077 == 0,
                "admission directory must have private permissions (0700)"
            );
            // SAFETY: geteuid has no preconditions.
            ensure_owned_by(path, &meta, unsafe { libc::geteuid() })?;
        }
        let lock_path = path.join(".lock");
        let mut options = std::fs::OpenOptions::new();
        options.read(true).write(true).create(true).truncate(false);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt;
            options.mode(0o600).custom_flags(libc::O_NOFOLLOW);
        }
        #[cfg(windows)]
        {
            use std::os::windows::fs::OpenOptionsExt;
            options.share_mode(0);
        }
        let lock = options.open(&lock_path)?;
        #[cfg(unix)]
        {
            use std::os::fd::AsRawFd;
            // SAFETY: valid owned fd; flock retains its lock until File drops.
            if unsafe { libc::flock(lock.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) } != 0 {
                return Err(anyhow::anyhow!(
                    "another daemon owns this admission directory"
                ));
            }
        }
        let mut ledger = Self {
            journal: Some(path.to_owned()),
            _journal_lock: Some(lock),
            ..Self::default()
        };
        let mut records = Vec::new();
        let mut replacements = Vec::new();
        for file in std::fs::read_dir(path)? {
            let file = file?;
            if file.file_name() == ".lock"
                || file.file_name() == CONTEXTS_FILE
                || file.file_name() == CONTEXTS_TEMP
            {
                continue;
            }
            anyhow::ensure!(
                file.file_type()?.is_file(),
                "unexpected admission state entry"
            );
            let mut bytes = Vec::new();
            std::fs::File::open(file.path())?
                .take(65537)
                .read_to_end(&mut bytes)?;
            anyhow::ensure!(bytes.len() <= 65536, "oversized admission record");
            match replacement_id(&file.file_name()) {
                Some(id) => replacements.push((id, bytes)),
                None => records.push((file.file_name(), bytes)),
            }
        }
        for (id, bytes) in replacements {
            ledger.resolve_replacement(&mut records, id, &bytes)?;
        }
        let last = records.iter().filter_map(|(name, _)| record_id(name)).max();
        for (name, bytes) in records {
            let work: vacancyd::status::Work = match serde_json::from_slice(&bytes) {
                Ok(work) => work,
                Err(error) => {
                    let id = record_id(&name).filter(|id| Some(*id) == last);
                    let Some(id) = id.filter(|_| is_torn(&bytes)) else {
                        return Err(error.into());
                    };
                    ledger.discard_torn(id, bytes.len())?;
                    continue;
                }
            };
            anyhow::ensure!(
                name == std::ffi::OsString::from(format!("entry-{}.json", work.id)),
                "admission record name/id mismatch"
            );
            let upstream = config.upstreams.iter().find(|u| u.id == work.upstream).ok_or_else(|| anyhow::anyhow!("held work references removed upstream; restore its configuration before recovery"))?;
            anyhow::ensure!(
                work.resources == dependencies(upstream),
                "held work resource mapping changed"
            );
            anyhow::ensure!(
                ledger.entries.len() < config.resources.len(),
                "too many held entries"
            );
            ledger.next = ledger.next.max(work.id);
            ledger.entries.insert(
                work.id,
                Entry {
                    upstream: work.upstream,
                    model: work.model,
                    resources: work.resources,
                    started: Instant::now(),
                    started_unix_s: work.started_unix_s,
                    recovered_age_s: unix_seconds().saturating_sub(work.started_unix_s),
                    completion: Completion::Unknown,
                    // Kept so an operator can find the job at the upstream, and
                    // so owner-return cancellation can still name it. Whether
                    // the upstream acknowledged it is not recorded.
                    tracking_id: work.tracking_id.filter(|id| is_safe_tracking_id(id)),
                    tracking_confirmed: false,
                    cancel_on_submit: false,
                    phase: "held",
                    primary: upstream.resource.clone(),
                    free_before: 0,
                    loaded_at: None,
                    measured: true,
                    observes_models: false,
                    runner: None,
                },
            );
        }
        ledger.load_contexts(config);
        Ok(ledger)
    }

    /// Restore the contexts an earlier run learned, for upstreams that are
    /// still configured. An unreadable file is logged and ignored, never a
    /// reason to refuse to start: it only costs cold-load charges.
    fn load_contexts(&mut self, config: &crate::config::Config) {
        use std::io::Read;
        let Some(dir) = self.journal.clone() else {
            return;
        };
        match std::fs::remove_file(dir.join(CONTEXTS_TEMP)) {
            Ok(()) => tracing::info!("removed a learned-context file left unfinished by a crash"),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
            Err(error) => {
                tracing::warn!(%error, "cannot remove an unfinished learned-context file")
            }
        }
        let path = dir.join(CONTEXTS_FILE);
        let read = std::fs::symlink_metadata(&path).and_then(|meta| {
            if !meta.file_type().is_file() {
                return Err(std::io::Error::other("not a regular file"));
            }
            let mut bytes = Vec::new();
            std::fs::File::open(&path)?
                .take(CONTEXTS_FILE_MAX + 1)
                .read_to_end(&mut bytes)?;
            if bytes.len() as u64 > CONTEXTS_FILE_MAX {
                return Err(std::io::Error::other("larger than 1 MiB"));
            }
            Ok(bytes)
        });
        let bytes = match read {
            Ok(bytes) => bytes,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => return,
            Err(error) => {
                tracing::warn!(%error, "ignoring the learned-context file; resident models are charged cold loads until requests teach their contexts again");
                return;
            }
        };
        let file = match serde_json::from_slice::<ContextsFile>(&bytes) {
            Ok(file) if file.version == CONTEXTS_VERSION => file,
            Ok(file) => {
                tracing::warn!(
                    version = file.version,
                    "ignoring a learned-context file from another vacancyd version"
                );
                return;
            }
            Err(error) => {
                tracing::warn!(%error, "ignoring an unreadable learned-context file; resident models are charged cold loads until requests teach their contexts again");
                return;
            }
        };
        for record in file.contexts {
            let family = match record.route.as_str() {
                "native" => RouteFamily::Native,
                "openai" => RouteFamily::OpenAi,
                _ => continue,
            };
            if !config.upstreams.iter().any(|u| u.id == record.upstream)
                || record.model.is_empty()
                || record.model.len() > 256
                || record.digest.is_empty()
                || record.digest.len() > 128
                || record.context_length == 0
                || record.num_ctx == Some(0)
            {
                continue;
            }
            let key = (
                record.upstream,
                record.model,
                RunnerRequest {
                    family,
                    num_ctx: record.num_ctx,
                },
            );
            let per_model = self
                .contexts
                .keys()
                .filter(|(o, m, _)| *o == key.0 && *m == key.1)
                .count();
            if per_model < CONTEXTS_PER_MODEL && self.contexts.len() < CONTEXTS_MAX {
                self.contexts.insert(
                    key,
                    LearnedContext {
                        context_length: record.context_length,
                        digest: Some(record.digest),
                    },
                );
            }
        }
        if !self.contexts.is_empty() {
            tracing::info!(
                contexts = self.contexts.len(),
                "restored learned model contexts"
            );
        }
    }

    /// Write the learned contexts that carry a digest, replacing the file
    /// in one rename so a crash leaves the old file or the new one. A
    /// failure is logged; the contexts stay in memory for this run.
    fn save_contexts(&self) {
        use std::io::Write;
        let Some(dir) = &self.journal else {
            return;
        };
        let file = ContextsFile {
            version: CONTEXTS_VERSION,
            contexts: self
                .contexts
                .iter()
                .filter_map(|((upstream, model, runner), learned)| {
                    Some(ContextRecord {
                        upstream: upstream.clone(),
                        model: model.clone(),
                        route: match runner.family {
                            RouteFamily::Native => "native",
                            RouteFamily::OpenAi => "openai",
                        }
                        .into(),
                        num_ctx: runner.num_ctx,
                        context_length: learned.context_length,
                        digest: learned.digest.clone()?,
                    })
                })
                .collect(),
        };
        let result = (|| -> anyhow::Result<()> {
            let bytes = serde_json::to_vec(&file)?;
            let temp = dir.join(CONTEXTS_TEMP);
            match std::fs::remove_file(&temp) {
                Err(error) if error.kind() != std::io::ErrorKind::NotFound => {
                    return Err(error.into())
                }
                _ => {}
            }
            let mut out = crate::secure_file::create_new(&temp)?;
            out.write_all(&bytes)?;
            out.sync_all()?;
            drop(out);
            std::fs::rename(&temp, dir.join(CONTEXTS_FILE))?;
            #[cfg(unix)]
            std::fs::File::open(dir)?.sync_all()?;
            Ok(())
        })();
        if let Err(error) = result {
            tracing::warn!(error = %format!("{error:#}"), "cannot save learned model contexts; they are kept for this run only");
        }
    }

    /// `persist` runs before the request is forwarded, so a record that a
    /// crash cut short belongs to work that never reached the upstream. Only
    /// the newest record can be in that state; any other unreadable record
    /// still stops recovery.
    fn discard_torn(&mut self, id: u64, len: usize) -> anyhow::Result<()> {
        let dir = self.journal.as_ref().expect("recovery has a journal");
        tracing::warn!(
            entry = id,
            bytes = len,
            "discarding a partially written admission record; its request was never forwarded"
        );
        std::fs::remove_file(dir.join(format!("entry-{id}.json")))?;
        crate::durable::sync_dir(dir)?;
        self.next = self.next.max(id);
        Ok(())
    }

    /// A crash between writing `entry-<id>.next` and renaming it over
    /// `entry-<id>.json` leaves both. A complete replacement for a record
    /// that still exists is the newer truth and is moved into place; a torn
    /// one, or one whose record was already removed because the work ended,
    /// is discarded.
    fn resolve_replacement(
        &mut self,
        records: &mut [(std::ffi::OsString, Vec<u8>)],
        id: u64,
        bytes: &[u8],
    ) -> anyhow::Result<()> {
        let dir = self
            .journal
            .as_ref()
            .expect("recovery has a journal")
            .clone();
        let name = std::ffi::OsString::from(format!("entry-{id}.json"));
        let replacement = dir.join(format!("entry-{id}.next"));
        let complete =
            serde_json::from_slice::<vacancyd::status::Work>(bytes).is_ok_and(|work| work.id == id);
        match records.iter_mut().find(|(record, _)| *record == name) {
            Some((_, current)) if complete => {
                tracing::warn!(
                    entry = id,
                    "completing an admission record update that a stop interrupted"
                );
                std::fs::rename(&replacement, dir.join(&name))?;
                *current = bytes.to_vec();
            }
            Some(_) => {
                tracing::warn!(
                    entry = id,
                    "discarding a partially written admission record update; the previous record stands"
                );
                std::fs::remove_file(&replacement)?;
            }
            None => {
                tracing::warn!(
                    entry = id,
                    "discarding an admission record update for work that already ended"
                );
                std::fs::remove_file(&replacement)?;
            }
        }
        #[cfg(unix)]
        std::fs::File::open(&dir)?.sync_all()?;
        Ok(())
    }

    /// Replace `id`'s recovery record with its current state. The new record
    /// is written and synced beside the old one, then renamed over it, so a
    /// crash leaves the old record, the new one, or both for recovery to
    /// resolve, never neither and never a mix.
    fn update_record(&self, id: u64) -> anyhow::Result<()> {
        use std::io::Write;
        let Some(dir) = &self.journal else {
            return Ok(());
        };
        let work = self
            .snapshot()
            .into_iter()
            .find(|w| w.id == id)
            .ok_or_else(|| anyhow::anyhow!("no admission entry {id}"))?;
        let bytes = serde_json::to_vec(&work)?;
        anyhow::ensure!(
            bytes.len() <= 65536,
            "admission record exceeds recovery limit"
        );
        let replacement = dir.join(format!("entry-{id}.next"));
        let written = (|| {
            match std::fs::remove_file(&replacement) {
                Ok(()) => {}
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
                Err(error) => return Err(error),
            }
            let mut file = crate::secure_file::create_new(&replacement)?;
            file.write_all(&bytes)?;
            file.sync_all()?;
            drop(file);
            std::fs::rename(&replacement, dir.join(format!("entry-{id}.json")))?;
            #[cfg(unix)]
            std::fs::File::open(dir)?.sync_all()?;
            Ok(())
        })();
        if written.is_err() {
            // Leave no stale replacement for recovery to adopt later.
            let _ = std::fs::remove_file(&replacement);
        }
        Ok(written?)
    }

    fn persist(&self, id: u64) -> anyhow::Result<()> {
        use std::io::Write;
        if let Some(dir) = &self.journal {
            let work = self.snapshot().into_iter().find(|w| w.id == id).unwrap();
            let bytes = serde_json::to_vec(&work)?;
            anyhow::ensure!(
                bytes.len() <= 65536,
                "admission record exceeds recovery limit"
            );
            let mut file = crate::secure_file::create_new(&dir.join(format!("entry-{id}.json")))?;
            file.write_all(&bytes)?;
            // The record's data and, on Windows, its creation; the directory
            // entry on Unix. See the durable module.
            file.sync_all()?;
            crate::durable::sync_dir(dir)?;
        }
        Ok(())
    }

    /// When the last job on `resource` ended or a yield handed it back, if
    /// either has happened. Evidence sampled before then describes memory
    /// that the job or the yield may since have changed.
    pub(crate) fn settled_at(&self, resource: &str) -> Option<Instant> {
        self.after.get(resource).copied()
    }

    pub fn check(&self, state: &AppState, upstream: &UpstreamConfig) -> Result<(), Refusal> {
        let runtime = &state.upstreams[&upstream.id];
        let models = runtime
            .models
            .read()
            .unwrap()
            .view(crate::lifecycle::FRESH_FOR);
        if runtime.adapter.requires_idle_observation()
            && models.quality != crate::state::ObservationQuality::Ready
        {
            return Err(busy(
                &upstream.resource,
                "waiting for a fresh upstream queue/slot observation",
            ));
        }
        if models.value.is_some_and(|m| m.busy) {
            return Err(busy(&upstream.resource, UPSTREAM_BUSY));
        }
        for id in dependencies(upstream) {
            if self.entries.values().any(|e| e.resources.contains(&id)) {
                return Err(busy(&id, "a job still owns this resource"));
            }
            if self.yielding.contains_key(&id) {
                return Err(busy(&id, "the resource is being handed back to its owner"));
            }
            if self.after.get(&id).is_some_and(|at| {
                !state.resources[&id]
                    .capacity
                    .read()
                    .unwrap()
                    .sampled_after(*at)
            }) {
                return Err(busy(&id, POST_JOB_SAMPLE));
            }
        }
        Ok(())
    }

    pub fn observe_models(
        &mut self,
        state: &AppState,
        upstream: &str,
        models: &[vacancyd::status::Model],
    ) {
        self.measurements
            .retain(|e| !e.measured && e.started.elapsed() < Duration::from_secs(30));
        for entry in self
            .entries
            .values_mut()
            .chain(self.measurements.iter_mut())
            .filter(|e| e.upstream == upstream && !e.measured)
        {
            let Some(name) = entry.model.as_ref() else {
                continue;
            };
            if !models.iter().any(|m| {
                m.name.as_ref() == Some(name)
                    && matches!(m.state.as_str(), "loaded_idle" | "running")
            }) {
                continue;
            }
            let at = *entry.loaded_at.get_or_insert_with(Instant::now);
            let cap = state.resources[&entry.primary].capacity.read().unwrap();
            if !cap.sampled_after(at) {
                continue;
            }
            let sample = cap.view(state.capacity_fresh_for());
            if sample.quality != crate::state::ObservationQuality::Ready {
                continue;
            }
            let delta = entry
                .free_before
                .saturating_sub(sample.value.unwrap().free_bytes);
            if delta > 0 {
                let key = (entry.upstream.clone(), name.clone());
                if self.priors.len() < 128 || self.priors.contains_key(&key) {
                    let prior = self.priors.entry(key).or_default();
                    *prior = (*prior).max(delta.div_ceil(1024 * 1024));
                }
            }
            entry.measured = true;
        }
    }

    /// Learn the context each recently finished request left its model
    /// loaded with, from the first model observation sampled after it
    /// finished and before a later request was admitted. Forget what was
    /// learned for a model whose digest has changed.
    pub fn learn_contexts(
        &mut self,
        upstream: &str,
        models: &[vacancyd::status::Model],
        sampled_at: Instant,
    ) {
        let before = self.contexts.len();
        self.contexts.retain(|(owner, model, _), learned| {
            owner != upstream
                || !models.iter().any(|row| {
                    row.name.as_deref() == Some(model.as_str())
                        && row.digest.is_some()
                        && row.digest != learned.digest
                })
        });
        let mut changed = self.contexts.len() != before;
        for pending in std::mem::take(&mut self.pending_contexts) {
            let (owner, model, _) = &pending.key;
            if owner != upstream || sampled_at <= pending.finished {
                if pending.finished.elapsed() < PENDING_CONTEXT_FOR {
                    self.pending_contexts.push(pending);
                }
                continue;
            }
            if pending.superseded.is_some_and(|at| sampled_at >= at) {
                continue;
            }
            let Some(row) = models.iter().find(|row| {
                row.name.as_deref() == Some(model.as_str())
                    && matches!(row.state.as_str(), "loaded_idle" | "running")
            }) else {
                continue;
            };
            let Some(context_length) = row.context_length else {
                continue;
            };
            let known = self.contexts.contains_key(&pending.key);
            let per_model = self
                .contexts
                .keys()
                .filter(|(o, m, _)| o == owner && m == model)
                .count();
            if known || (per_model < CONTEXTS_PER_MODEL && self.contexts.len() < CONTEXTS_MAX) {
                let learned = LearnedContext {
                    context_length,
                    digest: row.digest.clone(),
                };
                changed |= self.contexts.insert(pending.key, learned.clone()) != Some(learned);
            }
        }
        if changed {
            self.save_contexts();
        }
    }

    /// The context `upstream` loaded `model` with for requests like `runner`.
    pub(crate) fn learned_context(
        &self,
        upstream: &str,
        model: &str,
        runner: RunnerRequest,
    ) -> Option<&LearnedContext> {
        self.contexts
            .get(&(upstream.to_owned(), model.to_owned(), runner))
    }

    /// Evidence that `upstream` runs parallel slots: an explicit `num_ctx`
    /// that was seen to load a larger context (Ollama reports the context
    /// times the slot count). Returns that `num_ctx` and context.
    pub(crate) fn parallel_slots(&self, upstream: &str) -> Option<(u64, u64)> {
        self.contexts
            .iter()
            .filter(|((owner, _, _), _)| owner == upstream)
            .find_map(|((_, _, runner), learned)| {
                runner
                    .num_ctx
                    .filter(|asked| learned.context_length > *asked)
                    .map(|asked| (asked, learned.context_length))
            })
    }

    /// Charge the footprint learned from an earlier load of this model. A
    /// resident credit replaces it exactly as it replaces the configured
    /// requirement in `gate::check`. Returns what the credit changed.
    fn check_prior(
        &self,
        state: &AppState,
        upstream: &UpstreamConfig,
        model: Option<&str>,
        credit: Option<&crate::gate::Credit>,
    ) -> Result<Option<String>, Refusal> {
        let Some((model, prior)) = model.and_then(|model| {
            self.priors
                .get(&(upstream.id.clone(), model.into()))
                .map(|prior| (model, *prior))
        }) else {
            return Ok(None);
        };
        let resident_min_free_mib = crate::gate::model_requirement(state, upstream, model)
            .map_or(0, |requirement| requirement.resident_min_free_mib);
        let required = credit.map_or(prior, |credit| {
            credit.charge(&upstream.resource, prior, resident_min_free_mib)
        });
        let capacity = state.resources[&upstream.resource]
            .capacity
            .read()
            .unwrap()
            .view(state.capacity_fresh_for());
        // Like a configured requirement, the prior must fit beside the
        // owner's reserve on a memory resource, not inside it.
        let resource = state
            .config
            .resources
            .iter()
            .find(|resource| resource.id == upstream.resource)
            .expect("configuration validation requires the primary resource");
        let usable = capacity
            .value
            .as_ref()
            .map(|c| crate::gate::usable_mib(c, resource));
        if usable.is_none_or(|usable| usable < required) {
            let (beyond, beyond_it) = crate::gate::reserve_note(resource.owner_reserve_bytes());
            let free = usable.map_or_else(
                || "its capacity is unknown".to_owned(),
                |usable| format!("only {usable} MiB free{beyond_it}"),
            );
            let detail = match credit {
                Some(credit) if required != prior => format!(
                    "{}; it needs {required} MiB free on '{}'{beyond} (measured cold footprint {prior} MiB), {free}",
                    credit.label, upstream.resource
                ),
                _ => format!(
                    "measured model footprint needs {prior} MiB free on '{}'{beyond}, {free}",
                    upstream.resource
                ),
            };
            return Err(Refusal {
                reason: RefusalReason::InsufficientCapacity,
                detail,
                resource: Some(upstream.resource.clone()),
                retry_after_s: 30,
            });
        }
        // Only the charge: the credit's label is already in the gate's note,
        // and the two are logged as one line.
        Ok(credit.filter(|_| required != prior).map(|_| {
            format!(
                "the measured cold footprint of {prior} MiB was charged as {required} MiB on '{}'",
                upstream.resource
            )
        }))
    }

    fn finish(&mut self, id: u64) {
        if !self.entries.contains_key(&id) {
            return;
        }
        if let Some(dir) = &self.journal {
            let result = std::fs::remove_file(dir.join(format!("entry-{id}.json")));
            if result.is_err() {
                self.entries.get_mut(&id).unwrap().phase = "held";
                tracing::error!(
                    entry = id,
                    "cannot remove recovery record; reservation retained"
                );
                return;
            }
            // A crash before directory durability may restore an extra hold,
            // never lose an unfinished one.
            let _ = crate::durable::sync_dir(dir);
        }
        if let Some(entry) = self.entries.remove(&id) {
            let finished = Instant::now();
            for resource in &entry.resources {
                self.after.insert(resource.clone(), finished);
            }
            if let Some((runner, model)) = entry.runner.clone() {
                self.pending_contexts
                    .retain(|p| p.finished.elapsed() < PENDING_CONTEXT_FOR);
                if self.pending_contexts.len() < 32 {
                    self.pending_contexts.push(PendingContext {
                        key: (entry.upstream.clone(), model, runner),
                        finished,
                        superseded: None,
                    });
                }
            }
            if !entry.measured
                && entry.model.is_some()
                && entry.observes_models
                && self.measurements.len() < 128
            {
                self.measurements.push(entry);
            }
        }
    }

    pub fn release(&mut self, id: u64) -> Result<(), &'static str> {
        let entry = self.entries.get(&id).ok_or("unknown entry")?;
        if entry.phase != "held" {
            return Err("work is still being forwarded or tracked; stop it upstream and wait for tracking to end first");
        }
        let entry = self.entries.get_mut(&id).unwrap();
        entry.measured = true;
        // Its outcome is unknown, so it teaches nothing about contexts.
        entry.runner = None;
        self.finish(id);
        if self.entries.contains_key(&id) {
            return Err("cannot update durable recovery record");
        }
        Ok(())
    }

    pub fn snapshot(&self) -> Vec<vacancyd::status::Work> {
        self.entries
            .iter()
            .map(|(id, e)| vacancyd::status::Work {
                id: *id,
                upstream: e.upstream.clone(),
                model: e.model.clone(),
                resources: e.resources.clone(),
                age_s: e
                    .recovered_age_s
                    .saturating_add(e.started.elapsed().as_secs()),
                started_unix_s: e.started_unix_s,
                completion: match e.completion {
                    Completion::ResponseClose => "response_close",
                    Completion::Tracked => "tracked",
                    Completion::Unknown => "unknown",
                }
                .into(),
                phase: e.phase.into(),
                tracking_id: e.tracking_id.clone(),
                model_state: if e.phase == "held" || !e.observes_models || e.model.is_none() {
                    "unknown"
                } else if e.loaded_at.is_some() {
                    "running"
                } else {
                    "loading"
                }
                .into(),
            })
            .collect()
    }

    /// Work on the upstream's resources that something is still finishing:
    /// forwarding a response or tracking a queued job. Held work has lost its
    /// transport or tracker, so waiting for it cannot end.
    pub fn has_live_work_on(&self, upstream: &UpstreamConfig) -> bool {
        let resources = dependencies(upstream);
        self.entries.values().any(|entry| {
            entry.phase != "held" && entry.resources.iter().any(|id| resources.contains(id))
        })
    }

    /// Upstreams that own work something is still finishing, as in
    /// [`Self::has_live_work_on`]. Held work is not counted: nothing shows
    /// whether it still runs.
    pub fn upstreams_with_live_work(&self) -> BTreeSet<String> {
        self.entries
            .values()
            .filter(|entry| entry.phase != "held")
            .map(|entry| entry.upstream.clone())
            .collect()
    }

    /// Entries this upstream owns now. Owner-return cancellation acts only on
    /// these, never on work admitted after the event.
    pub fn entries_of(&self, upstream: &str) -> Vec<u64> {
        self.entries
            .iter()
            .filter(|(_, entry)| entry.upstream == upstream)
            .map(|(id, _)| *id)
            .collect()
    }

    /// Which of `ids` still exist, with their upstream job id when known.
    pub fn remaining(&self, ids: &[u64]) -> Vec<Remaining> {
        ids.iter()
            .filter_map(|id| {
                self.entries.get(id).map(|entry| Remaining {
                    id: *id,
                    tracking_id: entry.tracking_id.clone(),
                    held: entry.phase == "held",
                })
            })
            .collect()
    }

    /// The upstream job ids of every entry admitted through `upstreams`, or
    /// `None` when one of them cannot be named for certain: a submission
    /// still in flight, an id the upstream never confirmed, or no id at all.
    pub fn job_ids_on(&self, upstreams: &[&str]) -> Option<Vec<String>> {
        let mut ids = Vec::new();
        for entry in self
            .entries
            .values()
            .filter(|entry| upstreams.contains(&entry.upstream.as_str()))
        {
            if entry.is_submitting() || !entry.tracking_confirmed {
                return None;
            }
            ids.push(entry.tracking_id.clone()?);
        }
        Some(ids)
    }

    /// Whether any of `ids` is a tracked submission still waiting for its
    /// reply.
    pub fn submitting(&self, ids: &[u64]) -> bool {
        ids.iter()
            .filter_map(|id| self.entries.get(id))
            .any(Entry::is_submitting)
    }

    /// Take `ids` for owner-return cancellation. Entries with a job id are
    /// returned to cancel now. A submission still waiting for its reply is
    /// marked instead, and whoever resolves it cancels it: the reply may
    /// still queue the job, so its absence from the queue proves nothing
    /// yet. Each entry is taken exactly once, by one side or the other.
    pub fn claim_for_cancel(&mut self, ids: &[u64]) -> Vec<CancelClaim> {
        let mut claims = Vec::new();
        for id in ids {
            let Some(entry) = self.entries.get_mut(id) else {
                continue;
            };
            if entry.is_submitting() {
                entry.cancel_on_submit = true;
            } else if let Some(tracking_id) = &entry.tracking_id {
                claims.push(CancelClaim {
                    entry: *id,
                    tracking_id: tracking_id.clone(),
                    confirmed: entry.tracking_confirmed,
                });
            }
        }
        claims
    }

    /// Release work the upstream confirmed stopped after a cancellation. A
    /// cancelled job is not a footprint sample. Returns whether it was released.
    pub fn finish_cancelled(&mut self, id: u64) -> bool {
        let Some(entry) = self.entries.get_mut(&id) else {
            return false;
        };
        entry.measured = true;
        self.finish(id);
        !self.entries.contains_key(&id)
    }

    /// Coalesce owner-return cancellation for an upstream without automatic
    /// unloading. Unlike [`Ledger::begin_yield`] it holds no resource.
    pub fn begin_reclaim(&mut self, upstream: &UpstreamConfig) -> bool {
        self.pending_reclaims.insert(upstream.id.clone())
    }
    pub fn end_reclaim(&mut self, upstream: &UpstreamConfig) {
        self.pending_reclaims.remove(&upstream.id);
    }

    pub fn begin_yield(&mut self, upstream: &UpstreamConfig) -> bool {
        if !self.pending_yields.insert(upstream.id.clone()) {
            return false;
        }
        for id in dependencies(upstream) {
            *self.yielding.entry(id).or_default() += 1;
        }
        true
    }
    pub fn end_yield(&mut self, upstream: &UpstreamConfig) {
        if !self.pending_yields.remove(&upstream.id) {
            return;
        }
        for id in dependencies(upstream) {
            if let Some(count) = self.yielding.get_mut(&id) {
                *count -= 1;
                if *count == 0 {
                    self.yielding.remove(&id);
                }
            }
            self.after.insert(id, Instant::now());
        }
    }
}

/// Longest a stop waits for borrowed requests still being forwarded. It keeps
/// the whole stop inside the Windows service stop-wait hint and the time an
/// installer waits for the service to stop.
pub const SHUTDOWN_DRAIN_MAX: Duration = Duration::from_secs(10);

/// How long a stop may wait for requests in flight, from the upstreams'
/// owner-return policies: `cancel` waits for nothing, `drain` for its
/// `drain_timeout_s`, `finish` as long as allowed. Never more than
/// [`SHUTDOWN_DRAIN_MAX`].
pub fn shutdown_drain_window(config: &crate::config::Config) -> Duration {
    use crate::config::ReclaimMode;
    config
        .upstreams
        .iter()
        .map(|upstream| match upstream.on_owner_return {
            ReclaimMode::Cancel => Duration::ZERO,
            ReclaimMode::Drain => Duration::from_secs(u64::from(upstream.drain_timeout_s)),
            ReclaimMode::Finish => SHUTDOWN_DRAIN_MAX,
        })
        .max()
        .unwrap_or(Duration::ZERO)
        .min(SHUTDOWN_DRAIN_MAX)
}

/// Stop admitting borrowed work, then wait up to `window` for requests still
/// being forwarded to finish, so a service stop does not cut them off.
/// Returns how many were still in flight when the wait ended.
pub async fn drain_for_shutdown(state: &AppState, window: Duration) -> usize {
    state
        .lending_enabled
        .store(false, std::sync::atomic::Ordering::Relaxed);
    let deadline = tokio::time::Instant::now() + window;
    loop {
        let in_flight = state
            .admission
            .lock()
            .unwrap()
            .entries
            .values()
            .filter(|entry| entry.phase == "forwarding")
            .count();
        if in_flight == 0 {
            return 0;
        }
        if tokio::time::Instant::now() >= deadline {
            tracing::warn!(in_flight, "stopping with borrowed requests still in flight");
            return in_flight;
        }
        tracing::info!(in_flight, "waiting for borrowed requests before stopping");
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
}

/// What a tracked submission's exchange showed about the job id it follows.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Acknowledgement {
    /// The upstream's reply named the job, so it was queued under this id.
    Confirmed,
    /// The connection failed after sending and before any reply, and the id
    /// is the one vacancyd assigned. The upstream never received the job,
    /// queued it, or is still validating it. One that keeps assigned ids
    /// queues it under this id.
    LostBeforeReply,
    /// A reply arrived but did not name the job, or could not be read.
    Unconfirmed,
}

/// Whether a tracked job's absence from the upstream's queue and history
/// shows that it can no longer run.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Absence {
    /// It does: the upstream queued the job under this id before replying,
    /// or, once a lost submission's grace has passed, it keeps assigned ids
    /// and so would show the job under this one.
    Counts,
    /// Not before this time, and then only if the upstream keeps the ids
    /// vacancyd assigns. Until then the job may still be queued under it.
    AfterGrace(tokio::time::Instant),
    /// It never does: the upstream may still queue the job, or may know it
    /// under an id of its own.
    ProvesNothing,
}

impl Absence {
    fn of(acknowledgement: Acknowledgement) -> Self {
        match acknowledgement {
            Acknowledgement::Confirmed => Self::Counts,
            Acknowledgement::LostBeforeReply => {
                Self::AfterGrace(tokio::time::Instant::now() + TRACK_LOST_GRACE)
            }
            Acknowledgement::Unconfirmed => Self::ProvesNothing,
        }
    }
}

/// A transport owner may mark work uncertain on Drop, but cannot release it.
/// This includes cancellation while waiting for headers or forwarding bytes.
pub struct Ticket {
    state: Arc<AppState>,
    id: u64,
    completion: Completion,
}

pub fn reserve(
    state: &Arc<AppState>,
    upstream: &UpstreamConfig,
    model: Option<&str>,
    completion: Completion,
) -> Result<Ticket, Refusal> {
    reserve_for(state, upstream, Demand::model(model), completion, None)
}

/// Reserve for a request whose demand, including any runner options that
/// decide whether a resident model can serve it as loaded, is known.
/// `assigned` is the upstream job id vacancyd chose for a tracked
/// submission; it is part of the recovery record from the start.
pub fn reserve_for(
    state: &Arc<AppState>,
    upstream: &UpstreamConfig,
    demand: Demand<'_>,
    completion: Completion,
    assigned: Option<&str>,
) -> Result<Ticket, Refusal> {
    let model = demand.model;
    let mut ledger = state.admission.lock().unwrap();
    let effective = ledger.effective_upstream(upstream);
    let upstream = &effective;
    let admission = crate::gate::check(
        state,
        &ledger,
        &crate::gate::Snapshot::read(state),
        upstream,
        demand,
    )?;
    ledger.check(state, upstream)?;
    let prior_note = ledger.check_prior(state, upstream, model, admission.credit.as_ref())?;
    // A new allocation makes an older unsampled delta ambiguous.
    let resources = dependencies(upstream);
    ledger
        .measurements
        .retain(|e| !e.resources.iter().any(|r| resources.contains(r)));
    let admitted_at = Instant::now();
    for pending in &mut ledger.pending_contexts {
        if pending.key.0 == upstream.id {
            pending.superseded.get_or_insert(admitted_at);
        }
    }
    let host_scoped = upstream.owner_scope == crate::config::OwnerScope::Host;
    let mut insert = || {
        ledger.next = ledger.next.checked_add(1).expect("admission id exhausted");
        let id = ledger.next;
        ledger.entries.insert(
            id,
            Entry {
                upstream: upstream.id.clone(),
                model: model.map(|m| m.chars().take(256).collect()),
                resources: dependencies(upstream),
                started: Instant::now(),
                started_unix_s: unix_seconds(),
                recovered_age_s: 0,
                completion,
                tracking_id: assigned.map(str::to_owned),
                tracking_confirmed: false,
                cancel_on_submit: false,
                phase: "forwarding",
                primary: upstream.resource.clone(),
                free_before: state.resources[&upstream.resource]
                    .capacity
                    .read()
                    .unwrap()
                    .view(state.capacity_fresh_for())
                    .value
                    .unwrap()
                    .free_bytes,
                loaded_at: None,
                measured: false,
                observes_models: state.upstreams[&upstream.id].adapter.observes_models(),
                runner: demand
                    .runner
                    .zip(model.map(|m| state.upstreams[&upstream.id].adapter.canonical_model(m))),
            },
        );
        id
    };
    // A host-scoped entry is inserted under the veto lock, so an owner who
    // returns mid-admission either sees it in the yield or refuses it here.
    // A resource-scoped upstream is not reclaimed by the veto at all.
    let id = if host_scoped {
        state.veto.while_clear(insert)
    } else {
        Some(insert())
    }
    .ok_or_else(|| Refusal {
        reason: RefusalReason::OwnerActive,
        detail: "owner returned during admission".into(),
        resource: None,
        retry_after_s: 30,
    })?;
    if let Err(error) = ledger.persist(id) {
        ledger.entries.get_mut(&id).unwrap().phase = "held";
        ledger.sync_active_metrics(state);
        tracing::error!(entry = id, %error, "cannot persist admission; refusing to forward");
        return Err(busy(
            &upstream.resource,
            "admission recovery record could not be saved; operator recovery required",
        ));
    }
    ledger.sync_active_metrics(state);
    if let Some(note) = residency_log_note(&admission, prior_note.as_deref()) {
        tracing::info!(
            upstream = upstream.id,
            model = model.unwrap_or(""),
            residency = note.as_str(),
            "admitted against a resident model's incremental requirement"
        );
    }
    Ok(Ticket {
        state: state.clone(),
        id,
        completion,
    })
}

/// The single log note for an admission a residency credit shaped, if any.
///
/// The gate's note (configured requirements) and the learned-prior note
/// describe one credit, so they are joined into one line rather than logged
/// once each. The prior note alone still names the credit it used.
fn residency_log_note(admission: &crate::gate::Admission, prior: Option<&str>) -> Option<String> {
    let credit = admission.residency.clone().or_else(|| {
        prior
            .and(admission.credit.as_ref())
            .map(|c| c.label.clone())
    })?;
    Some(match prior {
        Some(prior) => format!("{credit}; {prior}"),
        None => credit,
    })
}

/// Reserve, waiting briefly where the only obstacle is evidence that is
/// already on its way. A job's end asks the collector for an immediate
/// capacity sample, so a borrower's next request waits for that sample
/// (bounded) instead of receiving a 503 for up to one poll interval.
///
/// Two refusals can also turn on a model observation taken a moment too
/// early, and each is retried after one fresh observation, within the same
/// bound as the background observer:
///
/// - A cached busy report is confirmed when it may describe work that has
///   already ended: always for llama.cpp, whose slots can free between the
///   lifecycle poll and the next request, and for any other observed
///   upstream when the report was sampled before vacancyd's own last job on
///   that resource finished (a narrated reply's next window arrives within
///   milliseconds of the previous one, inside the poll interval).
/// - A capacity refusal where residency was not credited only because the
///   model observation was too old or predates the last job or yield (a
///   resident model's follow-up request usually arrives right after the
///   job that loaded it).
///
/// The complete gate and ledger checks then run again. Unknown, failed,
/// still-busy, and post-job stale capacity all continue to refuse.
pub async fn reserve_with_fresh_observations(
    state: &Arc<AppState>,
    upstream: &UpstreamConfig,
    demand: Demand<'_>,
    completion: Completion,
    assigned: Option<&str>,
) -> Result<Ticket, Refusal> {
    let refusal = match reserve_for(state, upstream, demand, completion, assigned) {
        Ok(ticket) => return Ok(ticket),
        Err(refusal) if is_post_job_wait(&refusal) => {
            match reserve_after_post_job_sample(state, upstream, demand, completion, assigned).await
            {
                Ok(ticket) => return Ok(ticket),
                Err(refusal) if residency_evidence_stale(state, upstream, demand, &refusal) => {
                    refusal
                }
                Err(refusal) => return Err(refusal),
            }
        }
        Err(refusal) => refusal,
    };
    let cached_busy = refusal.reason == RefusalReason::ResourceBusy
        && refusal.detail == UPSTREAM_BUSY
        && (state.upstreams[&upstream.id].adapter.kind() == "llamacpp"
            || busy_report_predates_last_job_end(state, upstream));
    if !cached_busy && !residency_evidence_stale(state, upstream, demand, &refusal) {
        return Err(refusal);
    }
    let Some(models) = refresh_models(state, upstream).await else {
        return Err(refusal);
    };
    if cached_busy && models.busy {
        return Err(refusal);
    }
    reserve_after_post_job_sample(state, upstream, demand, completion, assigned).await
}

/// The upstream's model observation was sampled no later than the end of
/// vacancyd's last job on its resource, so its busy report may be that job.
fn busy_report_predates_last_job_end(state: &AppState, upstream: &UpstreamConfig) -> bool {
    let Some(settled) = state
        .admission
        .lock()
        .unwrap()
        .settled_at(&upstream.resource)
    else {
        return false;
    };
    !state.upstreams[&upstream.id]
        .models
        .read()
        .unwrap()
        .sampled_after(settled)
}

/// A capacity refusal that a newer model observation could turn into a
/// resident credit.
fn residency_evidence_stale(
    state: &AppState,
    upstream: &UpstreamConfig,
    demand: Demand<'_>,
    refusal: &Refusal,
) -> bool {
    refusal.reason == RefusalReason::InsufficientCapacity
        && crate::gate::residency(state, &state.admission.lock().unwrap(), upstream, demand)
            .is_err_and(|no_credit| no_credit.is_refreshable())
}

/// Observe the upstream's models now and record the result as the
/// background observer would. The observation is dated from its request,
/// not its reply, so it never claims to be newer than it is.
async fn refresh_models(
    state: &AppState,
    upstream: &UpstreamConfig,
) -> Option<crate::lifecycle::Models> {
    let runtime = &state.upstreams[&upstream.id];
    let probe = runtime
        .adapter
        .observe_models(&runtime.observer_http, upstream.target.as_str())?;
    let sampled_at = Instant::now();
    let models = match tokio::time::timeout(Duration::from_secs(4), probe).await {
        Ok(Ok(models)) => models,
        _ => {
            runtime
                .models
                .write()
                .unwrap()
                .record_failure("model observation unavailable");
            return None;
        }
    };
    runtime
        .models
        .write()
        .unwrap()
        .record_success_at(models.clone(), sampled_at);
    crate::lifecycle::sample_footprints(state, &upstream.id);
    Some(models)
}

fn is_post_job_wait(refusal: &Refusal) -> bool {
    refusal.reason == RefusalReason::ResourceBusy && refusal.detail == POST_JOB_SAMPLE
}

/// Rerun the complete gate and ledger checks until the post-job capacity
/// sample lands, within a bound. Every other refusal returns at once.
async fn reserve_after_post_job_sample(
    state: &Arc<AppState>,
    upstream: &UpstreamConfig,
    demand: Demand<'_>,
    completion: Completion,
    assigned: Option<&str>,
) -> Result<Ticket, Refusal> {
    let deadline = tokio::time::Instant::now() + FRESH_CAPACITY_WAIT;
    loop {
        match reserve_for(state, upstream, demand, completion, assigned) {
            Ok(ticket) => return Ok(ticket),
            Err(next) if is_post_job_wait(&next) && tokio::time::Instant::now() < deadline => {
                tokio::time::sleep(Duration::from_millis(25)).await;
            }
            Err(next) => return Err(next),
        }
    }
}

impl Ticket {
    /// The upstream provably never started this work: it rejected the
    /// submission, or the connection to it was refused.
    pub fn submission_rejected(self) {
        let mut ledger = self.state.admission.lock().unwrap();
        if let Some(entry) = ledger.entries.get_mut(&self.id) {
            // No model was commissioned, so this is not a footprint sample,
            // nor evidence of the context such a request loads.
            entry.measured = true;
            entry.runner = None;
        }
        drop(ledger);
        self.finish();
    }

    /// The ledger entry this ticket holds.
    pub fn id(&self) -> u64 {
        self.id
    }

    /// Whether only the upstream's own end of reply ends this work.
    pub fn ends_at_response_close(&self) -> bool {
        self.completion == Completion::ResponseClose
    }

    pub fn response_complete(self) {
        if self.completion == Completion::ResponseClose {
            self.finish();
        }
    }

    /// End the work and ask the collector for the capacity sample that the
    /// next admission on these resources is waiting for.
    fn finish(&self) {
        let mut ledger = self.state.admission.lock().unwrap();
        ledger.finish(self.id);
        ledger.sync_active_metrics(&self.state);
        drop(ledger);
        self.state.capacity_wake.notify_one();
    }

    /// The ledger entry this ticket owns.
    pub fn entry(&self) -> u64 {
        self.id
    }

    /// Follow the upstream job `id` until its completion is observed.
    /// `acknowledgement` says what the submission's exchange showed about the
    /// id. Returns whether owner-return cancellation reached the work while
    /// it was being submitted, which makes the caller responsible for
    /// cancelling it.
    pub fn track(
        self,
        upstream: UpstreamConfig,
        id: String,
        acknowledgement: Acknowledgement,
    ) -> bool {
        let cancel_requested = {
            let mut ledger = self.state.admission.lock().unwrap();
            let (requested, recorded) = match ledger.entries.get_mut(&self.id) {
                Some(entry) => {
                    let recorded = entry.tracking_id.replace(id.clone());
                    entry.tracking_confirmed = acknowledgement == Acknowledgement::Confirmed;
                    entry.phase = "tracking";
                    (std::mem::take(&mut entry.cancel_on_submit), Some(recorded))
                }
                None => (false, None),
            };
            // An upstream that chose its own job id (ComfyUI before 0.3.45)
            // makes the recovery record name a job that does not exist, so
            // it is rewritten with the id the upstream reported.
            if let Some(recorded) =
                recorded.filter(|recorded| recorded.as_deref() != Some(id.as_str()))
            {
                if let Err(error) = ledger.update_record(self.id) {
                    tracing::error!(
                        entry = self.id,
                        recorded = recorded.as_deref().unwrap_or(""),
                        tracking_id = id.as_str(),
                        error = %format!("{error:#}"),
                        "cannot record the upstream's job id for recovery; tracking continues, but after a crash the recovered entry would name the recorded id instead of this one"
                    );
                }
            }
            ledger.sync_active_metrics(&self.state);
            requested
        };
        tokio::spawn(async move {
            let runtime = &self.state.upstreams[&upstream.id];
            // This bounds the tracker, not the job: expiration keeps the hold.
            let deadline = tokio::time::Instant::now()
                + Duration::from_secs(u64::from(upstream.completion_timeout_s));
            let mut next_queue_check = tokio::time::Instant::now();
            let mut absent_checks = 0u32;
            let mut absence = Absence::of(acknowledgement);
            loop {
                // An owner-return cancellation may already have released it.
                if !self
                    .state
                    .admission
                    .lock()
                    .unwrap()
                    .entries
                    .contains_key(&self.id)
                {
                    break;
                }
                if let Absence::AfterGrace(until) = absence {
                    let now = tokio::time::Instant::now();
                    if now >= until && now >= next_queue_check {
                        match self.lost_absence(runtime, &upstream, &id, deadline).await {
                            Some(settled) => absence = settled,
                            None => next_queue_check = now + TRACK_QUEUE_EVERY,
                        }
                    }
                }
                // A job queued under this id is pending, running, finished, or
                // removed. The queue is read before the history: a job that
                // ends between the two reads is then found in the history.
                let queue_absent = (absence == Absence::Counts
                    && tokio::time::Instant::now() >= next_queue_check)
                    .then(|| {
                        next_queue_check = tokio::time::Instant::now() + TRACK_QUEUE_EVERY;
                        runtime.adapter.queue_absent(
                            &runtime.observer_http,
                            upstream.target.as_str(),
                            &id,
                        )
                    });
                let queue_absent = match queue_absent {
                    Some(check) => Some(tokio::time::timeout_at(deadline, check).await),
                    None => None,
                };
                let check = runtime.adapter.completed(
                    &runtime.observer_http,
                    upstream.target.as_str(),
                    &id,
                );
                let result = tokio::time::timeout_at(deadline, check).await;
                if matches!(result, Ok(Ok(true))) {
                    self.finish();
                    break;
                }
                if let Some(queue_absent) = queue_absent {
                    // Neither queued, running nor finished, twice in a row: the
                    // job was deleted, or its history was cleared or evicted.
                    // A failed read is not evidence and restarts the count.
                    absent_checks = match (queue_absent, &result) {
                        (Ok(Ok(true)), Ok(Ok(false))) => absent_checks + 1,
                        _ => 0,
                    };
                    if absent_checks >= TRACK_ABSENT_CHECKS {
                        self.vanished(&upstream, &id, acknowledgement);
                        break;
                    }
                }
                if tokio::time::Instant::now() >= deadline {
                    break;
                }
                tokio::time::sleep_until((tokio::time::Instant::now() + TRACK_POLL).min(deadline))
                    .await;
            }
        });
        cancel_requested
    }

    /// Whether a lost submission's absence can count, now that its grace has
    /// passed: only on an upstream that keeps the ids vacancyd assigns.
    /// `None` when the upstream could not be asked, which is not evidence
    /// either way.
    async fn lost_absence(
        &self,
        runtime: &crate::state::UpstreamRuntime,
        upstream: &UpstreamConfig,
        id: &str,
        deadline: tokio::time::Instant,
    ) -> Option<Absence> {
        let check = runtime
            .adapter
            .keeps_assigned_ids(&runtime.observer_http, upstream.target.as_str());
        let keeps = tokio::time::timeout_at(deadline, check).await.ok()?.ok()?;
        if !keeps {
            tracing::warn!(
                upstream = upstream.id,
                entry = self.id,
                tracking_id = id,
                "the upstream does not show that it keeps the job id vacancyd assigned, so the lost submission's absence proves nothing; its reservation stays until its history shows it ended or an administrator releases it"
            );
            return Some(Absence::ProvesNothing);
        }
        Some(Absence::Counts)
    }

    /// Release a job that the upstream does not hold anywhere: not queued,
    /// not running and not finished in its history. Nothing of it can run
    /// any more, except that a submission lost before any reply may still be
    /// under validation upstream, which absence cannot rule out. Like a
    /// cancelled job, it teaches no footprint.
    fn vanished(&self, upstream: &UpstreamConfig, id: &str, acknowledgement: Acknowledgement) {
        if let Some(entry) = self
            .state
            .admission
            .lock()
            .unwrap()
            .entries
            .get_mut(&self.id)
        {
            entry.measured = true;
            entry.runner = None;
        }
        let message = if acknowledgement == Acknowledgement::LostBeforeReply {
            "the submission's connection failed before any reply, and two checks after the grace period found no queued, running or completed job under its assigned id; releasing its reservation"
        } else {
            "tracked job is no longer queued, running or in history; releasing its reservation"
        };
        tracing::info!(
            upstream = upstream.id,
            entry = self.id,
            tracking_id = id,
            checks = TRACK_ABSENT_CHECKS,
            "{message}"
        );
        self.finish();
    }

    /// Give up on a submission whose outcome cannot be established: the
    /// reservation is held for an operator. Returns whether owner-return
    /// cancellation reached it while it was being submitted.
    pub fn hold(self) -> bool {
        let requested = self
            .state
            .admission
            .lock()
            .unwrap()
            .entries
            .get_mut(&self.id)
            .is_some_and(|entry| std::mem::take(&mut entry.cancel_on_submit));
        // Dropping the ticket marks the entry held.
        drop(self);
        requested
    }
}

impl Drop for Ticket {
    fn drop(&mut self) {
        let mut ledger = self.state.admission.lock().unwrap();
        if let Some(entry) = ledger.entries.get_mut(&self.id) {
            entry.phase = "held";
        }
        ledger.sync_active_metrics(&self.state);
    }
}

/// Small, time-bounded diagnostic JSON; redirects and credentials are disabled
/// on observer_http. No unbounded history list, prompt or image is fetched.
pub async fn read_json(request: reqwest::RequestBuilder) -> anyhow::Result<serde_json::Value> {
    tokio::time::timeout(Duration::from_secs(3), async {
        let mut response = request.send().await?.error_for_status()?;
        anyhow::ensure!(
            response.status().is_success(),
            "upstream returned HTTP {}",
            response.status()
        );
        let mut bytes = Vec::new();
        while let Some(chunk) = response.chunk().await? {
            anyhow::ensure!(
                bytes.len() + chunk.len() <= 1024 * 1024,
                "observation exceeds 1 MiB"
            );
            bytes.extend_from_slice(&chunk);
        }
        Ok(serde_json::from_slice(&bytes)?)
    })
    .await?
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::plugin::probe::Capacity;

    #[test]
    fn published_capacity_controls_the_next_admission_even_with_a_startup_proxy_copy() {
        let state = state();
        let startup = state.config.upstreams[0].clone();
        let mut changed = state.config.clone();
        changed.upstreams[0].default_min_free_mib = 9000;
        state
            .admission
            .lock()
            .unwrap()
            .publish_capacity(&changed, "high".into());
        let refused = reserve(&state, &startup, None, Completion::ResponseClose)
            .err()
            .unwrap();
        assert_eq!(refused.reason, RefusalReason::InsufficientCapacity);
        changed.upstreams[0].default_min_free_mib = 1000;
        state
            .admission
            .lock()
            .unwrap()
            .publish_capacity(&changed, "low".into());
        let ticket = reserve(&state, &startup, None, Completion::ResponseClose).unwrap();
        assert_eq!(state.admission.lock().unwrap().snapshot().len(), 1);
        drop(ticket);
    }

    #[test]
    fn a_published_model_budget_matches_ollama_aliases_in_real_admission() {
        let mut config = state().config.clone();
        config.upstreams[0].kind = "ollama".into();
        let state = Arc::new(AppState::new(config).unwrap());
        state.mark_presence_sampled();
        sample(&state, 8 * 1024 * 1024 * 1024);
        let startup = state.config.upstreams[0].clone();
        let mut changed = state.config.clone();
        changed.upstreams[0].models.insert(
            "helper:latest".into(),
            crate::config::ModelRequirement {
                load: crate::config::LoadRequirement::Scalar(9000),
                resident_min_free_mib: 0,
            },
        );
        state
            .admission
            .lock()
            .unwrap()
            .publish_capacity(&changed, "alias-high".into());
        assert_eq!(
            reserve(&state, &startup, Some("helper"), Completion::ResponseClose)
                .err()
                .unwrap()
                .reason,
            RefusalReason::InsufficientCapacity
        );
        changed.upstreams[0]
            .models
            .get_mut("helper:latest")
            .unwrap()
            .load = crate::config::LoadRequirement::Scalar(1000);
        state
            .admission
            .lock()
            .unwrap()
            .publish_capacity(&changed, "alias-low".into());
        reserve(&state, &startup, Some("helper"), Completion::ResponseClose)
            .unwrap()
            .response_complete();
    }

    #[test]
    fn published_resident_requirement_is_shared_by_gate_and_learned_prior() {
        let mut config = state().config.clone();
        config.upstreams[0].kind = "llamacpp".into();
        let state = Arc::new(AppState::new(config).unwrap());
        state.mark_presence_sampled();
        sample(&state, 1024 * 1024 * 1024);
        state.upstreams["a"]
            .models
            .write()
            .unwrap()
            .record_success(crate::lifecycle::Models {
                models: vec![vacancyd::status::Model {
                    name: None,
                    state: "loaded_idle".into(),
                    resident_bytes: None,
                    expires_at: None,
                    context_length: None,
                    digest: None,
                    default_context_length: None,
                }],
                busy: false,
            });
        let startup = state.config.upstreams[0].clone();
        let mut changed = state.config.clone();
        changed.upstreams[0].models.insert(
            "m".into(),
            crate::config::ModelRequirement {
                load: crate::config::LoadRequirement::Scalar(9000),
                resident_min_free_mib: 500,
            },
        );
        {
            let mut ledger = state.admission.lock().unwrap();
            ledger.priors.insert(("a".into(), "m".into()), 10000);
            ledger.publish_capacity(&changed, "resident-low".into());
        }
        reserve(&state, &startup, Some("m"), Completion::ResponseClose)
            .unwrap()
            .response_complete();
        sample(&state, 1024 * 1024 * 1024);
        changed.upstreams[0]
            .models
            .get_mut("m")
            .unwrap()
            .resident_min_free_mib = 2000;
        state
            .admission
            .lock()
            .unwrap()
            .publish_capacity(&changed, "resident-high".into());
        assert_eq!(
            reserve(&state, &startup, Some("m"), Completion::ResponseClose)
                .err()
                .unwrap()
                .reason,
            RefusalReason::InsufficientCapacity
        );
    }

    #[test]
    fn capacity_publication_preserves_held_leases_for_increased_and_reduced_requirements() {
        let state = state();
        let startup = state.config.upstreams[0].clone();
        let ticket = reserve(&state, &startup, None, Completion::ResponseClose).unwrap();
        let ticket_id = ticket.id;
        ticket.hold();
        let mut changed = state.config.clone();
        for mib in [9000, 1000] {
            changed.upstreams[0].default_min_free_mib = mib;
            state
                .admission
                .lock()
                .unwrap()
                .publish_capacity(&changed, mib.to_string());
            assert!(reserve(&state, &startup, None, Completion::ResponseClose).is_err());
            let ledger = state.admission.lock().unwrap();
            let held = ledger.snapshot();
            assert_eq!(held.len(), 1);
            assert_eq!(held[0].id, ticket_id);
            assert_eq!(held[0].phase, "held");
            assert_eq!(held[0].resources, vec!["cpu", "mem"]);
        }
    }

    fn state() -> Arc<AppState> {
        let config = toml::from_str(
            r#"
            [server]
            listen = "127.0.0.1:0"
            [owner.gpu_pressure]
            enabled = false
            [[resource]]
            id = "mem"
            kind = "system-memory"
            [[resource]]
            id = "cpu"
            kind = "system-cpu"
            [[upstream]]
            id = "a"
            listen = "127.0.0.1:0"
            target = "http://127.0.0.1:1"
            kind = "openai"
            resource = "mem"
            also_requires = ["cpu"]
            [[upstream]]
            id = "b"
            listen = "127.0.0.1:0"
            target = "http://127.0.0.1:2"
            kind = "openai"
            resource = "cpu"
        "#,
        )
        .unwrap();
        let state = Arc::new(AppState::new(config).unwrap());
        state.mark_presence_sampled();
        sample(&state, 8 * 1024 * 1024 * 1024);
        state
    }

    fn sample(state: &AppState, free_bytes: u64) {
        for resource in state.resources.values() {
            resource.capacity.write().unwrap().record_success(Capacity {
                free_bytes,
                ..Capacity::default()
            });
        }
    }

    async fn llama_state(slots: &'static [u8]) -> (Arc<AppState>, tokio::task::JoinHandle<()>) {
        llama_state_after(slots, 0).await
    }

    async fn llama_state_after(
        slots: &'static [u8],
        delay_ms: u64,
    ) -> (Arc<AppState>, tokio::task::JoinHandle<()>) {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let server = tokio::spawn(async move {
            let (mut socket, _) = listener.accept().await.unwrap();
            let mut request = [0u8; 1024];
            let _ = socket.read(&mut request).await.unwrap();
            tokio::time::sleep(Duration::from_millis(delay_ms)).await;
            socket
                .write_all(
                    format!(
                        "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
                        slots.len()
                    )
                    .as_bytes(),
                )
                .await
                .unwrap();
            socket.write_all(slots).await.unwrap();
        });
        let config = toml::from_str(&format!(
            r#"
            [server]
            listen = "127.0.0.1:0"
            [owner.gpu_pressure]
            enabled = false
            [[resource]]
            id = "mem"
            kind = "system-memory"
            [[upstream]]
            id = "llama"
            listen = "127.0.0.1:0"
            target = "http://{address}"
            kind = "llamacpp"
            resource = "mem"
            "#
        ))
        .unwrap();
        let state = Arc::new(AppState::new(config).unwrap());
        state.mark_presence_sampled();
        sample(&state, 8 * 1024 * 1024 * 1024);
        state.upstreams["llama"]
            .models
            .write()
            .unwrap()
            .record_success(crate::lifecycle::Models {
                models: vec![],
                busy: true,
            });
        (state, server)
    }

    #[tokio::test]
    async fn cached_busy_llama_slot_is_confirmed_fresh_before_admission() {
        let (state, server) = llama_state(br#"[{"is_processing":false}]"#).await;
        state.upstreams["llama"]
            .models
            .write()
            .unwrap()
            .record_success(crate::lifecycle::Models::default());
        reserve(
            &state,
            &state.config.upstreams[0],
            None,
            Completion::ResponseClose,
        )
        .unwrap()
        .response_complete();
        state.upstreams["llama"]
            .models
            .write()
            .unwrap()
            .record_success(crate::lifecycle::Models {
                models: vec![],
                busy: true,
            });
        let sampler = {
            let state = state.clone();
            tokio::spawn(async move {
                tokio::time::sleep(Duration::from_millis(25)).await;
                sample(&state, 8 * 1024 * 1024 * 1024);
            })
        };
        let ticket = reserve_with_fresh_observations(
            &state,
            &state.config.upstreams[0],
            Demand::default(),
            Completion::ResponseClose,
            None,
        )
        .await
        .unwrap();
        assert_eq!(state.admission.lock().unwrap().snapshot().len(), 1);
        ticket.response_complete();
        sampler.await.unwrap();
        server.await.unwrap();
    }

    #[tokio::test]
    async fn fresh_idle_llama_slot_does_not_bypass_post_job_capacity_freshness() {
        let (state, server) = llama_state(br#"[{"is_processing":false}]"#).await;
        // Create the post-job boundary while the cached observation is idle.
        state.upstreams["llama"]
            .models
            .write()
            .unwrap()
            .record_success(crate::lifecycle::Models::default());
        reserve(
            &state,
            &state.config.upstreams[0],
            None,
            Completion::ResponseClose,
        )
        .unwrap()
        .response_complete();
        state.upstreams["llama"]
            .models
            .write()
            .unwrap()
            .record_success(crate::lifecycle::Models {
                models: vec![],
                busy: true,
            });

        let refusal = reserve_with_fresh_observations(
            &state,
            &state.config.upstreams[0],
            Demand::default(),
            Completion::ResponseClose,
            None,
        )
        .await
        .err()
        .expect("stale post-job capacity must refuse");
        assert_eq!(refusal.reason, RefusalReason::ResourceBusy);
        assert!(refusal.detail.contains("capacity sample"));
        server.await.unwrap();
    }

    #[tokio::test]
    async fn fresh_busy_or_unknown_llama_slots_remain_closed() {
        let (state, server) = llama_state(br#"[{"is_processing":true}]"#).await;
        let refusal = reserve_with_fresh_observations(
            &state,
            &state.config.upstreams[0],
            Demand::default(),
            Completion::ResponseClose,
            None,
        )
        .await
        .err()
        .expect("a freshly busy slot must refuse");
        assert_eq!(refusal.reason, RefusalReason::ResourceBusy);
        assert!(state.admission.lock().unwrap().snapshot().is_empty());
        server.await.unwrap();

        let (unknown, unused_server) = llama_state(br#"[]"#).await;
        *unknown.upstreams["llama"].models.write().unwrap() = Default::default();
        let refusal = reserve_with_fresh_observations(
            &unknown,
            &unknown.config.upstreams[0],
            Demand::default(),
            Completion::ResponseClose,
            None,
        )
        .await
        .err()
        .expect("unknown slot state must refuse without probing");
        assert_eq!(refusal.reason, RefusalReason::ResourceBusy);
        assert!(refusal.detail.contains("fresh upstream"));
        unused_server.abort();
    }

    #[test]
    fn one_sample_cannot_admit_concurrent_or_cross_upstream_work() {
        let state = state();
        let barrier = Arc::new(std::sync::Barrier::new(12));
        let workers: Vec<_> = (0..12)
            .map(|n| {
                let state = state.clone();
                let barrier = barrier.clone();
                std::thread::spawn(move || {
                    barrier.wait();
                    reserve(
                        &state,
                        &state.config.upstreams[n % 2],
                        None,
                        Completion::ResponseClose,
                    )
                    .ok()
                })
            })
            .collect();
        let accepted: Vec<_> = workers
            .into_iter()
            .filter_map(|w| w.join().unwrap())
            .collect();
        assert_eq!(accepted.len(), 1);
        assert_eq!(state.admission.lock().unwrap().entries.len(), 1);
    }

    #[test]
    fn active_work_metric_tracks_forwarding_and_held_phases() {
        let state = state();
        let ticket = reserve(
            &state,
            &state.config.upstreams[0],
            None,
            Completion::Unknown,
        )
        .unwrap();
        assert_eq!(
            state
                .metrics
                .active_work
                .with_label_values(&["a", "forwarding"])
                .get(),
            1
        );
        drop(ticket);
        assert_eq!(
            state
                .metrics
                .active_work
                .with_label_values(&["a", "forwarding"])
                .get(),
            0
        );
        assert_eq!(
            state
                .metrics
                .active_work
                .with_label_values(&["a", "held"])
                .get(),
            1
        );
    }

    #[test]
    fn clean_completion_requires_new_samples_for_every_dependency() {
        let state = state();
        let upstream = &state.config.upstreams[0];
        reserve(&state, upstream, None, Completion::ResponseClose)
            .unwrap()
            .response_complete();
        assert!(reserve(&state, upstream, None, Completion::ResponseClose).is_err());
        state.resources["mem"]
            .capacity
            .write()
            .unwrap()
            .record_success(Capacity::default());
        assert!(reserve(&state, upstream, None, Completion::ResponseClose).is_err());
        state.resources["cpu"]
            .capacity
            .write()
            .unwrap()
            .record_success(Capacity::default());
        assert!(reserve(&state, upstream, None, Completion::ResponseClose).is_ok());
    }

    #[test]
    fn disconnect_and_unknown_response_close_cannot_release_work() {
        for completion in [Completion::ResponseClose, Completion::Unknown] {
            let state = state();
            let upstream = &state.config.upstreams[0];
            let ticket = reserve(&state, upstream, None, completion).unwrap();
            let id = ticket.id;
            assert!(state.admission.lock().unwrap().release(id).is_err());
            if completion == Completion::Unknown {
                ticket.response_complete();
            } else {
                drop(ticket);
            }
            sample(&state, u64::MAX);
            assert!(reserve(&state, upstream, None, completion).is_err());
            state.admission.lock().unwrap().release(id).unwrap();
            assert!(reserve(&state, upstream, None, completion).is_err());
            sample(&state, 8 * 1024 * 1024 * 1024);
            assert!(reserve(&state, upstream, None, completion).is_ok());
        }
    }

    #[test]
    fn a_resource_scoped_upstream_is_admitted_while_the_owner_veto_holds() {
        let state = state();
        let mut upstream = state.config.upstreams[0].clone();
        state.veto.observe(Some("owner".into()), 0);
        assert!(reserve(&state, &upstream, None, Completion::ResponseClose).is_err());
        upstream.owner_scope = crate::config::OwnerScope::Resource;
        assert!(reserve(&state, &upstream, None, Completion::ResponseClose).is_ok());
    }

    #[test]
    fn veto_and_scheduled_yield_hold_admission_until_new_evidence() {
        let state = state();
        let upstream = &state.config.upstreams[0];
        state.veto.observe(Some("owner".into()), 0);
        assert!(reserve(&state, upstream, None, Completion::ResponseClose).is_err());
        state.admission.lock().unwrap().begin_yield(upstream);
        state.veto.observe(None, 0);
        assert!(reserve(&state, upstream, None, Completion::ResponseClose).is_err());
        state.admission.lock().unwrap().end_yield(upstream);
        assert!(reserve(&state, upstream, None, Completion::ResponseClose).is_err());
        sample(&state, 8 * 1024 * 1024 * 1024);
        assert!(reserve(&state, upstream, None, Completion::ResponseClose).is_ok());
    }

    #[test]
    fn a_loaded_model_learns_only_from_a_later_capacity_sample() {
        let state = state();
        let upstream = &state.config.upstreams[0];
        let ticket = reserve(&state, upstream, Some("model"), Completion::ResponseClose).unwrap();
        let models = [vacancyd::status::Model {
            name: Some("model".into()),
            state: "loaded_idle".into(),
            resident_bytes: None,
            expires_at: None,
            context_length: None,
            digest: None,
            default_context_length: None,
        }];
        state
            .admission
            .lock()
            .unwrap()
            .observe_models(&state, "a", &models);
        assert!(state.admission.lock().unwrap().priors.is_empty());
        sample(&state, 6 * 1024 * 1024 * 1024);
        state
            .admission
            .lock()
            .unwrap()
            .observe_models(&state, "a", &models);
        assert_eq!(
            state.admission.lock().unwrap().priors[&("a".into(), "model".into())],
            2048
        );
        ticket.response_complete();
        sample(&state, 1024 * 1024 * 1024);
        assert!(reserve(&state, upstream, Some("model"), Completion::ResponseClose).is_err());
        sample(&state, 3 * 1024 * 1024 * 1024);
        assert!(reserve(&state, upstream, Some("model"), Completion::ResponseClose).is_ok());
    }

    /// `path` and any missing directories above it, private whatever the
    /// umask: the recovery directory is created only under a path no other
    /// account can change.
    fn private_dir_all(path: &std::path::Path) {
        let mut builder = std::fs::DirBuilder::new();
        builder.recursive(true);
        #[cfg(unix)]
        {
            use std::os::unix::fs::DirBuilderExt;
            builder.mode(0o700);
        }
        builder.create(path).unwrap();
    }

    fn ledger_dir(tag: &str) -> std::path::PathBuf {
        crate::secure_file::test_root().join(format!(
            "vacancyd-ledger-{tag}-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ))
    }

    /// A journal holding one complete record (entry-1) and `second(entry-1's
    /// bytes)` as entry-2, which is how a crash while persisting leaves it.
    fn journal_with_second_record(
        tag: &str,
        second: impl FnOnce(Vec<u8>) -> Vec<u8>,
    ) -> (Arc<AppState>, std::path::PathBuf) {
        let state = state();
        let upstream = &state.config.upstreams[0];
        let path = ledger_dir(tag);
        *state.admission.lock().unwrap() =
            Ledger::open(&path, &state.config, Missing::Create).unwrap();
        drop(reserve(&state, upstream, None, Completion::ResponseClose).unwrap());
        *state.admission.lock().unwrap() = Ledger::default();
        let first = std::fs::read(path.join("entry-1.json")).unwrap();
        let complete = String::from_utf8(first)
            .unwrap()
            .replacen("\"id\":1,", "\"id\":2,", 1)
            .into_bytes();
        std::fs::write(path.join("entry-2.json"), second(complete)).unwrap();
        (state, path)
    }

    fn remove_journal(path: &std::path::Path) {
        for file in std::fs::read_dir(path).unwrap() {
            std::fs::remove_file(file.unwrap().path()).unwrap();
        }
        std::fs::remove_dir(path).unwrap();
    }

    #[test]
    fn a_torn_final_record_is_discarded_and_complete_records_are_kept() {
        type Cut = fn(Vec<u8>) -> Vec<u8>;
        let torn: [(&str, Cut); 3] = [
            ("empty", |_| Vec::new()),
            ("prefix", |complete| complete[..complete.len() / 2].to_vec()),
            ("zero-fill", |complete| {
                let mut bytes = complete[..complete.len() / 3].to_vec();
                bytes.extend([0u8; 64]);
                bytes
            }),
        ];
        for (tag, cut) in torn {
            let (state, path) = journal_with_second_record(tag, cut);
            let mut recovered = Ledger::open(&path, &state.config, Missing::Create).unwrap();
            let work = recovered.snapshot();
            assert_eq!(work.len(), 1, "{tag}");
            assert_eq!((work[0].id, work[0].phase.as_str()), (1, "held"), "{tag}");
            assert!(!path.join("entry-2.json").exists(), "{tag}");
            // The discarded id is never reissued.
            recovered.release(1).unwrap();
            *state.admission.lock().unwrap() = recovered;
            sample(&state, 8 * 1024 * 1024 * 1024);
            let ticket = reserve(
                &state,
                &state.config.upstreams[0],
                None,
                Completion::Unknown,
            )
            .unwrap();
            assert_eq!(ticket.id, 3, "{tag}");
            drop(ticket);
            *state.admission.lock().unwrap() = Ledger::default();
            remove_journal(&path);
        }

        // A complete final record is recovered, never discarded.
        let (state, path) = journal_with_second_record("complete", |complete| complete);
        let work = Ledger::open(&path, &state.config, Missing::Create)
            .unwrap()
            .snapshot();
        assert_eq!(work.iter().map(|w| w.id).collect::<Vec<_>>(), [1, 2]);
        remove_journal(&path);
    }

    #[test]
    fn unreadable_records_other_than_a_torn_final_one_still_stop_recovery() {
        // Corrupt rather than cut short: not a JSON prefix.
        let (state, path) = journal_with_second_record("garbage", |_| b"{\"id\":2,}".to_vec());
        assert!(Ledger::open(&path, &state.config, Missing::Create).is_err());
        assert!(path.join("entry-2.json").exists());
        remove_journal(&path);

        // Cut short, but an older record: a crash cannot produce this.
        let (state, path) = journal_with_second_record("older", |_| Vec::new());
        std::fs::rename(path.join("entry-2.json"), path.join("entry-0.json")).unwrap();
        assert!(Ledger::open(&path, &state.config, Missing::Create).is_err());
        assert!(path.join("entry-0.json").exists());
        remove_journal(&path);
    }

    #[tokio::test]
    async fn the_next_request_waits_for_the_sample_a_finished_job_requested() {
        let state = state();
        let upstream = state.config.upstreams[1].clone();
        let woken = {
            let state = state.clone();
            tokio::spawn(async move { state.capacity_wake.notified().await })
        };
        tokio::task::yield_now().await;
        reserve(&state, &upstream, None, Completion::ResponseClose)
            .unwrap()
            .response_complete();
        tokio::time::timeout(Duration::from_secs(1), woken)
            .await
            .expect("completion did not request a capacity sample")
            .unwrap();
        // The collector's sample lands while the request is waiting.
        let sampler = {
            let state = state.clone();
            tokio::spawn(async move {
                tokio::time::sleep(Duration::from_millis(50)).await;
                sample(&state, 8 * 1024 * 1024 * 1024);
            })
        };
        let ticket = reserve_with_fresh_observations(
            &state,
            &upstream,
            Demand::default(),
            Completion::ResponseClose,
            None,
        )
        .await
        .expect("admitted once the post-job sample arrived");
        ticket.response_complete();
        sampler.await.unwrap();

        // Without a sample the wait is bounded and the refusal is unchanged.
        let refusal = reserve_with_fresh_observations(
            &state,
            &upstream,
            Demand::default(),
            Completion::ResponseClose,
            None,
        )
        .await
        .err()
        .expect("no post-job sample must still refuse");
        assert_eq!(refusal.detail, POST_JOB_SAMPLE);
    }

    #[tokio::test]
    async fn a_llama_slot_refresh_is_dated_from_its_request_not_its_reply() {
        let (state, server) = llama_state_after(br#"[{"is_processing":false}]"#, 300).await;
        let before = Instant::now();
        reserve_with_fresh_observations(
            &state,
            &state.config.upstreams[0],
            Demand::default(),
            Completion::ResponseClose,
            None,
        )
        .await
        .unwrap()
        .response_complete();
        let age = state.upstreams["llama"]
            .models
            .read()
            .unwrap()
            .view(crate::lifecycle::FRESH_FOR)
            .age
            .unwrap();
        // Dated at the request: at least as old as the slow reply.
        assert!(age >= Duration::from_millis(300), "{age:?}");
        assert!(age <= before.elapsed());
        server.await.unwrap();
    }

    #[test]
    fn restart_retains_uncertain_work_and_clean_finish_removes_the_record() {
        let state = state();
        let upstream = &state.config.upstreams[0];
        let path = crate::secure_file::test_root().join(format!(
            "vacancyd-ledger-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        *state.admission.lock().unwrap() =
            Ledger::open(&path, &state.config, Missing::Create).unwrap();
        let ticket = reserve(&state, upstream, Some("model"), Completion::ResponseClose).unwrap();
        let id = ticket.id;
        assert!(Ledger::open(&path, &state.config, Missing::Create).is_err());
        drop(ticket);
        *state.admission.lock().unwrap() = Ledger::default();
        let mut recovered = Ledger::open(&path, &state.config, Missing::Create).unwrap();
        assert_eq!(recovered.snapshot().len(), 1);
        assert_eq!(recovered.snapshot()[0].phase, "held");
        assert!(recovered.check(&state, upstream).is_err());
        recovered.release(id).unwrap();
        drop(recovered);
        assert!(Ledger::open(&path, &state.config, Missing::Create)
            .unwrap()
            .snapshot()
            .is_empty());
        std::fs::remove_file(path.join(".lock")).unwrap();
        std::fs::remove_dir(&path).unwrap();
    }

    #[test]
    fn shutdown_waits_only_as_long_as_the_reclaim_policies_allow() {
        let window = |policies: &str| {
            let config: crate::config::Config = toml::from_str(&format!(
                r#"
                [server]
                listen = "127.0.0.1:0"
                [[resource]]
                id = "mem"
                kind = "system-memory"
                {policies}
                "#
            ))
            .unwrap();
            shutdown_drain_window(&config)
        };
        let upstream = |id: &str, policy: &str| {
            format!(
                "[[upstream]]\nid = \"{id}\"\nlisten = \"127.0.0.1:0\"\ntarget = \"http://127.0.0.1:1\"\nkind = \"openai\"\nresource = \"mem\"\n{policy}\n"
            )
        };
        assert_eq!(window(""), Duration::ZERO);
        assert_eq!(
            window(&upstream("a", "on_owner_return = \"cancel\"")),
            Duration::ZERO
        );
        assert_eq!(
            window(&format!(
                "{}{}",
                upstream("a", "on_owner_return = \"cancel\""),
                upstream("b", "on_owner_return = \"drain\"\ndrain_timeout_s = 4")
            )),
            Duration::from_secs(4)
        );
        assert_eq!(
            window(&upstream(
                "a",
                "on_owner_return = \"drain\"\ndrain_timeout_s = 3600"
            )),
            SHUTDOWN_DRAIN_MAX
        );
        assert_eq!(
            window(&upstream("a", "on_owner_return = \"finish\"")),
            SHUTDOWN_DRAIN_MAX
        );
    }

    #[tokio::test(start_paused = true)]
    async fn shutdown_refuses_new_work_and_lets_a_request_in_flight_finish() {
        let state = state();
        let upstream = state.config.upstreams[0].clone();
        let ticket = reserve(&state, &upstream, Some("model"), Completion::ResponseClose).unwrap();
        let finisher = tokio::spawn(async move {
            tokio::time::sleep(Duration::from_secs(3)).await;
            ticket.response_complete();
        });
        let started = tokio::time::Instant::now();
        let left = drain_for_shutdown(&state, Duration::from_secs(10)).await;
        assert_eq!(left, 0);
        let waited = started.elapsed();
        assert!(waited >= Duration::from_secs(3) && waited < Duration::from_secs(4));
        assert!(!state
            .lending_enabled
            .load(std::sync::atomic::Ordering::Relaxed));
        assert!(reserve(&state, &upstream, Some("model"), Completion::ResponseClose).is_err());
        finisher.await.unwrap();
    }

    #[tokio::test(start_paused = true)]
    async fn shutdown_stops_waiting_at_the_window() {
        let state = state();
        let upstream = state.config.upstreams[0].clone();
        let _ticket = reserve(&state, &upstream, Some("model"), Completion::ResponseClose).unwrap();
        let started = tokio::time::Instant::now();
        let left = drain_for_shutdown(&state, Duration::from_secs(2)).await;
        assert_eq!(left, 1);
        let waited = started.elapsed();
        assert!(waited >= Duration::from_secs(2) && waited < Duration::from_secs(3));
    }

    /// A second daemon on the same admission records, which is what an
    /// elevated `vacancyd run` meets while the installed service is running:
    /// Windows refuses the exclusive lock with a sharing violation, and the
    /// startup error names the other instance.
    #[cfg(windows)]
    #[test]
    fn a_second_daemon_on_the_same_records_is_told_what_holds_them() {
        let state = state();
        let path = crate::secure_file::test_root().join(format!(
            "vacancyd-ledger-twice-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        let first = Ledger::open(&path, &state.config, Missing::Create).unwrap();
        let second = anyhow::Context::context(
            Ledger::open(&path, &state.config, Missing::Create),
            "open admission recovery records",
        )
        .err()
        .expect("the lock is exclusive");
        let explained = crate::elevation::explain("opening the admission records", second);
        let startup = crate::elevation::startup_error(&explained)
            .unwrap_or_else(|| panic!("not mapped: {explained:#}"));
        assert_eq!(startup.kind, crate::elevation::Kind::InUse);
        assert_eq!(startup.kind.exit_code(), 51);
        drop(first);
        std::fs::remove_dir_all(&path).unwrap();
    }

    #[cfg(windows)]
    #[test]
    fn an_existing_ordinary_directory_is_refused_and_left_unchanged() {
        let state = state();
        let path = crate::secure_file::test_root().join(format!(
            "vacancyd-ledger-existing-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        std::fs::create_dir(&path).unwrap();
        // Inside the protected folder a new directory inherits SYSTEM and
        // Administrators only. Grant ordinary users read access so the
        // directory is ordinary.
        let granted = std::process::Command::new("icacls")
            .arg(&path)
            .args(["/grant", "*S-1-5-32-545:(OI)(CI)(RX)"])
            .output()
            .unwrap();
        assert!(granted.status.success(), "{granted:?}");
        assert!(crate::windows_acl::verify_directory(&path).is_err());
        let refused = Ledger::open(&path, &state.config, Missing::Create)
            .err()
            .unwrap();
        assert!(
            format!("{refused:#}").contains("access is granted to"),
            "{refused:#}"
        );
        // Still the inherited, ordinary permissions: nothing was rewritten.
        assert!(crate::windows_acl::verify_directory(&path).is_err());
        assert!(!path.join(".lock").exists());
        std::fs::remove_dir(&path).unwrap();
    }

    #[cfg(windows)]
    #[test]
    fn a_recovery_directory_outside_the_protected_folder_is_refused_existing_or_not() {
        let state = state();
        let outside = crate::secure_file::protected_directory_outside_test_root();
        let missing = outside.join("vacancyd.admission");
        for (path, missing_rule) in [(&outside, Missing::Refuse), (&missing, Missing::Create)] {
            let refused = Ledger::open(path, &state.config, missing_rule)
                .err()
                .unwrap();
            let message = format!("{refused:#}");
            assert!(
                message.contains("refusing the admission state directory"),
                "{message}"
            );
            assert!(message.contains("is not inside"), "{message}");
        }
        assert!(!missing.exists());
        assert!(!outside.join(".lock").exists());
        std::fs::remove_dir(outside).unwrap();
    }

    #[test]
    fn assigned_job_ids_are_random_canonical_uuids() {
        let ids: Vec<String> = (0..64).map(|_| new_tracking_id().unwrap()).collect();
        for id in &ids {
            let groups: Vec<&str> = id.split('-').collect();
            assert_eq!(
                groups.iter().map(|g| g.len()).collect::<Vec<_>>(),
                [8, 4, 4, 4, 12],
                "{id}"
            );
            assert!(id
                .bytes()
                .all(|b| b == b'-' || b.is_ascii_digit() || (b'a'..=b'f').contains(&b)));
            // Version 4, RFC 4122 variant: what ComfyUI's UUID check accepts.
            assert_eq!(&groups[2][..1], "4", "{id}");
            assert!(matches!(&groups[3][..1], "8" | "9" | "a" | "b"), "{id}");
            assert!(is_safe_tracking_id(id));
        }
        let unique: BTreeSet<&String> = ids.iter().collect();
        assert_eq!(unique.len(), ids.len());
    }

    #[test]
    fn a_recovered_record_keeps_its_assigned_job_id_and_drops_an_unsafe_one() {
        let state = state();
        let upstream = &state.config.upstreams[0];
        let path = ledger_dir("assigned");
        *state.admission.lock().unwrap() =
            Ledger::open(&path, &state.config, Missing::Create).unwrap();
        let assigned = new_tracking_id().unwrap();
        let ticket = reserve_for(
            &state,
            upstream,
            Demand::default(),
            Completion::Tracked,
            Some(&assigned),
        )
        .unwrap();
        let id = ticket.id;
        // The job id is on record before anything is forwarded.
        assert_eq!(
            state.admission.lock().unwrap().snapshot()[0]
                .tracking_id
                .as_deref(),
            Some(assigned.as_str())
        );
        drop(ticket);
        *state.admission.lock().unwrap() = Ledger::default();
        let mut recovered = Ledger::open(&path, &state.config, Missing::Create).unwrap();
        let work = recovered.snapshot();
        assert_eq!(work[0].phase, "held");
        assert_eq!(work[0].tracking_id.as_deref(), Some(assigned.as_str()));
        // Recovery cannot know whether the upstream acknowledged it.
        assert_eq!(
            recovered.claim_for_cancel(&[id]),
            vec![CancelClaim {
                entry: id,
                tracking_id: assigned.clone(),
                confirmed: false
            }]
        );
        drop(recovered);
        // A record edited to carry a path is recovered without it.
        let record = path.join(format!("entry-{id}.json"));
        let text = std::fs::read_to_string(&record)
            .unwrap()
            .replace(&assigned, "../queue");
        std::fs::write(&record, text).unwrap();
        let recovered = Ledger::open(&path, &state.config, Missing::Create).unwrap();
        assert_eq!(recovered.snapshot()[0].tracking_id, None);
        drop(recovered);
        std::fs::remove_file(record).unwrap();
        std::fs::remove_file(path.join(".lock")).unwrap();
        std::fs::remove_dir(&path).unwrap();
    }

    #[tokio::test]
    async fn an_in_flight_submission_is_cancelled_by_exactly_one_side() {
        let state = state();
        let upstream = state.config.upstreams[0].clone();
        let submit = |state: &Arc<AppState>| {
            let assigned = new_tracking_id().unwrap();
            let ticket = reserve_for(
                state,
                &upstream,
                Demand::default(),
                Completion::Tracked,
                Some(&assigned),
            )
            .unwrap();
            (ticket, assigned)
        };
        // Still waiting for its reply: owner-return cancellation marks it and
        // leaves it to whoever settles the submission.
        let (ticket, assigned) = submit(&state);
        let entry = ticket.id;
        assert!(state.admission.lock().unwrap().submitting(&[entry]));
        assert!(state
            .admission
            .lock()
            .unwrap()
            .claim_for_cancel(&[entry])
            .is_empty());
        assert!(ticket.track(
            upstream.clone(),
            assigned.clone(),
            Acknowledgement::Confirmed
        ));
        // The flag is consumed: settling it again cannot cancel it twice.
        let claims = state.admission.lock().unwrap().claim_for_cancel(&[entry]);
        assert_eq!(
            claims,
            vec![CancelClaim {
                entry,
                tracking_id: assigned,
                confirmed: true
            }]
        );
        state.admission.lock().unwrap().finish_cancelled(entry);
        sample(&state, 8 * 1024 * 1024 * 1024);

        // Settled before cancellation reached it: cancellation takes it, and
        // the submission side has nothing to do.
        let (ticket, assigned) = submit(&state);
        let entry = ticket.id;
        assert!(!ticket.track(upstream.clone(), "job-7".into(), Acknowledgement::Confirmed));
        assert!(!state.admission.lock().unwrap().submitting(&[entry]));
        let claims = state.admission.lock().unwrap().claim_for_cancel(&[entry]);
        assert_eq!(claims[0].tracking_id, "job-7");
        assert_ne!(claims[0].tracking_id, assigned);
        state.admission.lock().unwrap().finish_cancelled(entry);
        sample(&state, 8 * 1024 * 1024 * 1024);

        // A submission given up as held still reports a pending cancellation.
        let (ticket, _) = submit(&state);
        let entry = ticket.id;
        state.admission.lock().unwrap().claim_for_cancel(&[entry]);
        assert!(ticket.hold());
        assert_eq!(state.admission.lock().unwrap().snapshot()[0].phase, "held");
    }

    fn remove_journal_dir(path: &std::path::Path) {
        for file in std::fs::read_dir(path).unwrap() {
            let file = file.unwrap().path();
            if file.is_dir() {
                std::fs::remove_dir(&file).unwrap();
            } else {
                std::fs::remove_file(&file).unwrap();
            }
        }
        std::fs::remove_dir(path).unwrap();
    }

    fn record_on_disk(path: &std::path::Path, entry: u64) -> vacancyd::status::Work {
        serde_json::from_slice(&std::fs::read(path.join(format!("entry-{entry}.json"))).unwrap())
            .unwrap()
    }

    /// An upstream that chose its own job id (ComfyUI before 0.3.45) had
    /// the id recorded only in memory: after a crash, recovery named the id
    /// vacancyd had assigned, which does not exist upstream.
    #[tokio::test]
    async fn an_upstream_chosen_job_id_is_recorded_for_recovery() {
        let state = state();
        let upstream = state.config.upstreams[0].clone();
        let path = ledger_dir("acknowledged");
        *state.admission.lock().unwrap() =
            Ledger::open(&path, &state.config, Missing::Create).unwrap();
        let assigned = new_tracking_id().unwrap();
        // A sample of its own: the one `state()` took may have aged out on a
        // busy host, and freshness is not what this test is about.
        sample(&state, 8 * 1024 * 1024 * 1024);
        let ticket = reserve_for(
            &state,
            &upstream,
            Demand::default(),
            Completion::Tracked,
            Some(&assigned),
        )
        .unwrap();
        let entry = ticket.id;
        assert_eq!(
            record_on_disk(&path, entry).tracking_id.as_deref(),
            Some(assigned.as_str())
        );
        ticket.track(
            upstream.clone(),
            "upstream-issued-id".into(),
            Acknowledgement::Confirmed,
        );
        assert_eq!(
            record_on_disk(&path, entry).tracking_id.as_deref(),
            Some("upstream-issued-id")
        );
        assert!(!path.join(format!("entry-{entry}.next")).exists());
        // The daemon stops without the work ending.
        *state.admission.lock().unwrap() = Ledger::default();
        let recovered = Ledger::open(&path, &state.config, Missing::Create).unwrap();
        let work = recovered.snapshot();
        assert_eq!(work[0].phase, "held");
        assert_eq!(work[0].tracking_id.as_deref(), Some("upstream-issued-id"));
        drop(recovered);
        remove_journal_dir(&path);
    }

    /// A crash between writing a replacement record and renaming it into
    /// place leaves `entry-<id>.next` beside the old record.
    #[test]
    fn recovery_resolves_a_replacement_record_a_crash_left_behind() {
        let state = state();
        let upstream = &state.config.upstreams[0];
        for case in ["complete", "torn", "orphan"] {
            let path = ledger_dir(case);
            *state.admission.lock().unwrap() =
                Ledger::open(&path, &state.config, Missing::Create).unwrap();
            sample(&state, 8 * 1024 * 1024 * 1024);
            let ticket = reserve_for(
                &state,
                upstream,
                Demand::default(),
                Completion::Tracked,
                Some("assigned-id"),
            )
            .unwrap();
            let entry = ticket.id;
            drop(ticket);
            *state.admission.lock().unwrap() = Ledger::default();
            let record = std::fs::read_to_string(path.join(format!("entry-{entry}.json"))).unwrap();
            let newer = record.replace("assigned-id", "upstream-issued-id");
            let (name, bytes, expected, entries) = match case {
                "complete" => (entry, newer.clone(), "upstream-issued-id", 1),
                "torn" => (entry, newer[..newer.len() / 2].to_owned(), "assigned-id", 1),
                _ => {
                    // The work ended and its record was removed after a
                    // failed update left a replacement behind.
                    std::fs::remove_file(path.join(format!("entry-{entry}.json"))).unwrap();
                    (entry, newer.clone(), "", 0)
                }
            };
            std::fs::write(path.join(format!("entry-{name}.next")), bytes).unwrap();
            let recovered = Ledger::open(&path, &state.config, Missing::Create).unwrap();
            let work = recovered.snapshot();
            assert_eq!(work.len(), entries, "{case}");
            if entries == 1 {
                assert_eq!(work[0].tracking_id.as_deref(), Some(expected), "{case}");
                assert_eq!(
                    record_on_disk(&path, entry).tracking_id.as_deref(),
                    Some(expected),
                    "{case}"
                );
            }
            assert!(!path.join(format!("entry-{name}.next")).exists(), "{case}");
            drop(recovered);
            // Resolved once, it reopens cleanly again.
            drop(Ledger::open(&path, &state.config, Missing::Create).unwrap());
            remove_journal_dir(&path);
        }
    }

    /// If the record cannot be rewritten, tracking goes on with the real id,
    /// the reservation still survives a crash, and the log names both ids.
    #[tokio::test]
    async fn a_failed_record_update_keeps_tracking_and_names_both_ids() {
        let log = crate::test_log::capture();
        let state = state();
        let upstream = state.config.upstreams[0].clone();
        let path = ledger_dir("update-fails");
        *state.admission.lock().unwrap() =
            Ledger::open(&path, &state.config, Missing::Create).unwrap();
        sample(&state, 8 * 1024 * 1024 * 1024);
        let ticket = reserve_for(
            &state,
            &upstream,
            Demand::default(),
            Completion::Tracked,
            Some("assigned-id"),
        )
        .unwrap();
        let entry = ticket.id;
        // Something occupies the replacement's name, so it cannot be written.
        std::fs::create_dir(path.join(format!("entry-{entry}.next"))).unwrap();
        ticket.track(
            upstream.clone(),
            "upstream-issued-id".into(),
            Acknowledgement::Confirmed,
        );
        let work = state.admission.lock().unwrap().snapshot();
        assert_eq!(work[0].tracking_id.as_deref(), Some("upstream-issued-id"));
        assert_eq!(work[0].phase, "tracking");
        assert_eq!(
            record_on_disk(&path, entry).tracking_id.as_deref(),
            Some("assigned-id")
        );
        let logged = log.contents();
        assert!(
            logged.contains("cannot record the upstream's job id for recovery")
                && logged.contains("assigned-id")
                && logged.contains("upstream-issued-id"),
            "{logged}"
        );
        *state.admission.lock().unwrap() = Ledger::default();
        std::fs::remove_dir(path.join(format!("entry-{entry}.next"))).unwrap();
        let recovered = Ledger::open(&path, &state.config, Missing::Create).unwrap();
        assert_eq!(recovered.snapshot()[0].phase, "held");
        drop(recovered);
        remove_journal_dir(&path);
    }

    fn row(name: &str, context: u64, digest: Option<&str>) -> vacancyd::status::Model {
        vacancyd::status::Model {
            name: Some(name.into()),
            state: "loaded_idle".into(),
            resident_bytes: None,
            expires_at: None,
            context_length: Some(context),
            digest: digest.map(str::to_owned),
            default_context_length: None,
        }
    }

    /// Teach `ledger` that `runner` requests for `model` on upstream `a`
    /// loaded `context`, as a finished request and the next observation do.
    fn teach(
        ledger: &mut Ledger,
        model: &str,
        runner: RunnerRequest,
        row: vacancyd::status::Model,
    ) {
        let finished = Instant::now() - Duration::from_millis(5);
        ledger.pending_contexts.push(PendingContext {
            key: ("a".into(), model.into(), runner),
            finished,
            superseded: None,
        });
        ledger.learn_contexts("a", &[row], Instant::now());
    }

    const UNSET: RunnerRequest = RunnerRequest {
        family: RouteFamily::OpenAi,
        num_ctx: None,
    };

    #[test]
    fn learned_contexts_survive_a_restart_with_their_digest() {
        let state = state();
        let path = ledger_dir("contexts");
        let mut ledger = Ledger::open(&path, &state.config, Missing::Create).unwrap();
        teach(
            &mut ledger,
            "helper",
            UNSET,
            row("helper", 32768, Some("d1")),
        );
        // A context learned without a digest is kept for this run only.
        let pinned = RunnerRequest {
            family: RouteFamily::Native,
            num_ctx: Some(4096),
        };
        teach(&mut ledger, "nodigest", pinned, row("nodigest", 4096, None));
        assert!(ledger.learned_context("a", "nodigest", pinned).is_some());
        drop(ledger);

        let ledger = Ledger::open(&path, &state.config, Missing::Create).unwrap();
        assert_eq!(
            ledger.learned_context("a", "helper", UNSET),
            Some(&LearnedContext {
                context_length: 32768,
                digest: Some("d1".into()),
            })
        );
        assert!(ledger.learned_context("a", "nodigest", pinned).is_none());
        drop(ledger);

        // A new digest forgets the context, and the forgetting is saved too.
        let mut ledger = Ledger::open(&path, &state.config, Missing::Create).unwrap();
        ledger.learn_contexts("a", &[row("helper", 32768, Some("d2"))], Instant::now());
        assert!(ledger.learned_context("a", "helper", UNSET).is_none());
        drop(ledger);
        let ledger = Ledger::open(&path, &state.config, Missing::Create).unwrap();
        assert!(ledger.learned_context("a", "helper", UNSET).is_none());
        drop(ledger);
        std::fs::remove_dir_all(&path).unwrap();
    }

    #[test]
    fn a_damaged_or_foreign_context_file_never_stops_recovery() {
        let state = state();
        let path = ledger_dir("contexts-damaged");
        let mut ledger = Ledger::open(&path, &state.config, Missing::Create).unwrap();
        teach(
            &mut ledger,
            "helper",
            UNSET,
            row("helper", 32768, Some("d1")),
        );
        drop(ledger);
        let file = path.join(CONTEXTS_FILE);
        let good = std::fs::read(&file).unwrap();

        let reopen = |bytes: &[u8]| {
            std::fs::write(&file, bytes).unwrap();
            Ledger::open(&path, &state.config, Missing::Create).unwrap()
        };
        for bytes in [
            &b""[..],
            b"{",
            b"[]",
            br#"{"version":2,"contexts":[]}"#,
            br#"{"version":1,"contexts":[],"extra":1}"#,
        ] {
            let ledger = reopen(bytes);
            assert!(
                ledger.contexts.is_empty(),
                "{}",
                String::from_utf8_lossy(bytes)
            );
        }
        // Records for a removed upstream or with impossible values are dropped.
        let ledger = reopen(
            br#"{"version":1,"contexts":[
                {"upstream":"gone","model":"m","route":"openai","num_ctx":null,"context_length":1,"digest":"d"},
                {"upstream":"a","model":"m","route":"other","num_ctx":null,"context_length":1,"digest":"d"},
                {"upstream":"a","model":"m","route":"native","num_ctx":0,"context_length":1,"digest":"d"},
                {"upstream":"a","model":"m","route":"native","num_ctx":null,"context_length":0,"digest":"d"},
                {"upstream":"a","model":"m","route":"native","num_ctx":null,"context_length":1,"digest":""},
                {"upstream":"a","model":"kept","route":"native","num_ctx":null,"context_length":8192,"digest":"d"}]}"#,
        );
        assert_eq!(ledger.contexts.len(), 1);
        assert!(ledger
            .learned_context(
                "a",
                "kept",
                RunnerRequest {
                    family: RouteFamily::Native,
                    num_ctx: None
                }
            )
            .is_some());
        drop(ledger);
        // An unfinished temporary file from a crash is removed, and the
        // intact file beside it still loads.
        std::fs::write(&file, &good).unwrap();
        std::fs::write(path.join(CONTEXTS_TEMP), b"{\"vers").unwrap();
        let ledger = Ledger::open(&path, &state.config, Missing::Create).unwrap();
        assert!(!path.join(CONTEXTS_TEMP).exists());
        assert!(ledger.learned_context("a", "helper", UNSET).is_some());
        drop(ledger);
        std::fs::remove_dir_all(&path).unwrap();
    }

    #[test]
    fn a_learned_prior_must_fit_beside_the_owner_memory_reserve() {
        let config = toml::from_str(
            r#"
            [server]
            listen = "127.0.0.1:0"
            [owner.gpu_pressure]
            enabled = false
            [[resource]]
            id = "mem"
            kind = "system-memory"
            reserve = 4294967296
            [[upstream]]
            id = "a"
            listen = "127.0.0.1:0"
            target = "http://127.0.0.1:1"
            kind = "openai"
            resource = "mem"
            "#,
        )
        .unwrap();
        let state = Arc::new(AppState::new(config).unwrap());
        state.mark_presence_sampled();
        state
            .admission
            .lock()
            .unwrap()
            .priors
            .insert(("a".into(), "m".into()), 1000);
        let upstream = &state.config.upstreams[0];
        sample(&state, (4096 + 999) * 1024 * 1024);
        let refusal = reserve(&state, upstream, Some("m"), Completion::ResponseClose)
            .err()
            .unwrap();
        assert!(
            refusal.detail.contains(
                "measured model footprint needs 1000 MiB free on 'mem' beyond the owner's 4096 MiB reserve, only 999 MiB free beyond it"
            ),
            "{}",
            refusal.detail
        );
        sample(&state, (4096 + 1000) * 1024 * 1024);
        reserve(&state, upstream, Some("m"), Completion::ResponseClose)
            .unwrap()
            .response_complete();
    }

    fn with_admission_dir(state: &AppState, value: &str) -> crate::config::Config {
        let mut config = state.config.clone();
        config.server.admission_state_dir = value.into();
        config
    }

    #[test]
    fn only_the_default_recovery_directory_is_created_when_missing() {
        let state = state();
        let root = std::env::temp_dir().join("vacancyd-resolve");
        let config_path = root.join("etc").join("vacancyd.toml");
        let state_dir = root.join("state");
        let resolve = |value: &str, state_directory: Option<&std::path::Path>| {
            RecoveryDir::resolve(
                &with_admission_dir(&state, value),
                &config_path,
                state_directory.map(|d| d.as_os_str()),
            )
        };
        let dir = |path: std::path::PathBuf, missing| RecoveryDir { path, missing };
        // Empty: beside the configuration, or under the service's state
        // directory.
        assert_eq!(
            resolve("", None),
            dir(root.join("etc").join("vacancyd.admission"), Missing::Create)
        );
        assert_eq!(
            resolve("", Some(&state_dir)),
            dir(state_dir.join("admission"), Missing::Create)
        );
        // Naming the default location, as the packaged Linux instructions
        // do, still creates it on first start under the service manager.
        let named = state_dir.join("admission");
        assert_eq!(
            resolve(named.to_str().unwrap(), Some(&state_dir)),
            dir(named.clone(), Missing::Create)
        );
        // Started any other way, the same setting names a directory that
        // must already exist.
        assert_eq!(
            resolve(named.to_str().unwrap(), None),
            dir(named, Missing::Refuse)
        );
        // A relative value follows the configuration, so a moved
        // configuration names somewhere new: it is never created.
        assert_eq!(
            resolve("records", Some(&state_dir)),
            dir(root.join("etc").join("records"), Missing::Refuse)
        );
        let elsewhere = root.join("srv").join("admission");
        assert_eq!(
            resolve(elsewhere.to_str().unwrap(), Some(&state_dir)),
            dir(elsewhere, Missing::Refuse)
        );
    }

    /// A configuration moved away from a relative recovery directory used to
    /// start with a new, empty ledger, forgetting the held work in the old one.
    #[test]
    fn a_moved_configuration_with_held_work_refuses_to_start_instead_of_forgetting_it() {
        let state = state();
        let upstream = &state.config.upstreams[0];
        let root = ledger_dir("moved");
        let before = root.join("before");
        let after = root.join("after");
        private_dir_all(&before);
        private_dir_all(&after);
        let config = with_admission_dir(&state, "records");
        let original = RecoveryDir::resolve(&config, &before.join("vacancyd.toml"), None);
        *state.admission.lock().unwrap() =
            Ledger::open(&original.path, &config, Missing::Create).unwrap();
        drop(reserve(&state, upstream, None, Completion::ResponseClose).unwrap());
        *state.admission.lock().unwrap() = Ledger::default();
        assert!(holds_records(&original.path).unwrap());

        let moved = RecoveryDir::resolve(&config, &after.join("vacancyd.toml"), None);
        assert_ne!(moved.path, original.path);
        let refused = Ledger::open(&moved.path, &config, moved.missing)
            .err()
            .expect("a missing named directory is refused");
        let message = format!("{refused:#}");
        assert!(message.contains("does not exist"), "{message}");
        assert!(
            message.contains("server.admission_state_dir names it"),
            "{message}"
        );
        assert!(!moved.path.exists(), "nothing is created");
        // The held work is where it was, and still opens there.
        let recovered = Ledger::open(&original.path, &config, original.missing).unwrap();
        assert_eq!(recovered.snapshot()[0].phase, "held");
        drop(recovered);
        remove_journal(&original.path);
        std::fs::remove_dir(&before).unwrap();
        std::fs::remove_dir(&after).unwrap();
        std::fs::remove_dir(&root).unwrap();
    }

    /// An install that left `admission_state_dir` empty, with the service's
    /// records in `service` and a run started by hand from `config_dir`.
    fn manual_run_beside(
        state: &AppState,
        service: &std::path::Path,
        config_dir: &std::path::Path,
    ) -> anyhow::Result<Option<String>> {
        let recovery = RecoveryDir::resolve(&state.config, &config_dir.join("vacancyd.toml"), None);
        check_beside_service(&state.config, &recovery, None, service)
    }

    #[test]
    fn a_manual_run_beside_a_service_ledger_holding_work_refuses_to_start() {
        let state = state();
        let root = ledger_dir("beside-held");
        let service = root.join("var-lib").join("admission");
        let etc = root.join("etc");
        private_dir_all(service.parent().unwrap());
        private_dir_all(&etc);
        *state.admission.lock().unwrap() =
            Ledger::open(&service, &state.config, Missing::Create).unwrap();
        drop(
            reserve(
                &state,
                &state.config.upstreams[0],
                None,
                Completion::ResponseClose,
            )
            .unwrap(),
        );
        *state.admission.lock().unwrap() = Ledger::default();

        let refused = manual_run_beside(&state, &service, &etc).unwrap_err();

        let message = format!("{refused:#}");
        assert!(
            message.contains("hold unfinished or held work"),
            "{message}"
        );
        assert!(
            message.contains(&format!("admission_state_dir = '{}'", service.display())),
            "{message}"
        );
        assert!(
            message.contains(&etc.join("vacancyd.admission").display().to_string()),
            "{message}"
        );
        remove_journal(&service);
        std::fs::remove_dir(service.parent().unwrap()).unwrap();
        std::fs::remove_dir(&etc).unwrap();
        std::fs::remove_dir(&root).unwrap();
    }

    #[test]
    fn a_manual_run_beside_an_empty_service_ledger_warns_and_starts() {
        let state = state();
        let root = ledger_dir("beside-empty");
        private_dir_all(&root);
        let service = root.join("admission");
        let ledger = Ledger::open(&service, &state.config, Missing::Create).unwrap();
        drop(ledger);

        let warning = manual_run_beside(&state, &service, &root).unwrap().unwrap();

        assert!(warning.contains("keeps its own in"), "{warning}");
        assert!(
            warning.contains(&format!("admission_state_dir = '{}'", service.display())),
            "{warning}"
        );
        remove_journal(&service);
        std::fs::remove_dir(&root).unwrap();
    }

    #[test]
    fn a_service_ledger_that_cannot_be_read_warns_and_starts() {
        let state = state();
        let root = ledger_dir("beside-unreadable");
        std::fs::create_dir(&root).unwrap();
        // Not a directory: reading it fails as an unreadable one does.
        let service = root.join("admission");
        std::fs::write(&service, b"").unwrap();

        let warning = manual_run_beside(&state, &service, &root).unwrap().unwrap();

        assert!(warning.starts_with("could not read"), "{warning}");
        std::fs::remove_file(&service).unwrap();
        std::fs::remove_dir(&root).unwrap();
    }

    #[test]
    fn only_a_manual_run_with_the_default_location_is_checked_against_the_service() {
        let state = state();
        let root = ledger_dir("beside-unchecked");
        private_dir_all(&root);
        let service = root.join("admission");
        *state.admission.lock().unwrap() =
            Ledger::open(&service, &state.config, Missing::Create).unwrap();
        drop(
            reserve(
                &state,
                &state.config.upstreams[0],
                None,
                Completion::ResponseClose,
            )
            .unwrap(),
        );
        *state.admission.lock().unwrap() = Ledger::default();
        let config_path = root.join("vacancyd.toml");

        // No service ledger: nothing to compare with.
        assert!(manual_run_beside(&state, &root.join("absent"), &root)
            .unwrap()
            .is_none());
        // The service itself, started with its state directory.
        let recovery = RecoveryDir::resolve(&state.config, &config_path, Some(root.as_os_str()));
        assert!(
            check_beside_service(&state.config, &recovery, Some(root.as_os_str()), &service)
                .unwrap()
                .is_none()
        );
        // A named location is the same however vacancyd starts.
        let named = with_admission_dir(&state, service.to_str().unwrap());
        let recovery = RecoveryDir::resolve(&named, &config_path, None);
        assert!(check_beside_service(&named, &recovery, None, &service)
            .unwrap()
            .is_none());
        remove_journal(&service);
        std::fs::remove_dir(&root).unwrap();
    }

    #[test]
    fn the_default_recovery_directory_is_created_on_first_start() {
        let state = state();
        let root = ledger_dir("first-start");
        private_dir_all(&root);
        let default = RecoveryDir::resolve(&state.config, &root.join("vacancyd.toml"), None);
        assert_eq!(default.missing, Missing::Create);
        let ledger = Ledger::open(&default.path, &state.config, default.missing).unwrap();
        assert!(ledger.snapshot().is_empty());
        assert!(default.path.is_dir());
        assert!(
            !holds_records(&default.path).unwrap(),
            "the lock is not a record"
        );
        drop(ledger);
        remove_journal(&default.path);
        std::fs::remove_dir(&root).unwrap();
    }

    #[test]
    fn a_new_recovery_directory_is_made_durable_in_its_parent_before_use() {
        let root = ledger_dir("durable-parent");
        private_dir_all(&root);
        let path = root.join("admission");
        let mut synced = Vec::new();
        create_recovery_directory(&path, |directory| {
            synced.push(directory.to_path_buf());
            Ok(())
        })
        .unwrap();
        assert_eq!(synced, vec![root.clone()]);
        assert!(path.is_dir());
        std::fs::remove_dir(&path).unwrap();

        let failed = create_recovery_directory(&path, |_| {
            Err(std::io::Error::other("injected sync failure"))
        })
        .unwrap_err();
        assert!(
            format!("{failed:#}").contains("injected sync failure"),
            "{failed:#}"
        );
        std::fs::remove_dir(&path).unwrap();
        std::fs::remove_dir(&root).unwrap();
    }

    #[test]
    fn a_restart_after_a_failed_parent_sync_syncs_the_parent_before_use() {
        let root = ledger_dir("durable-restart");
        private_dir_all(&root);
        let path = root.join("admission");
        // The first start creates the directory, then cannot sync its parent.
        let failed = recovery_directory(&path, Missing::Create, |_| {
            Err(std::io::Error::other("injected sync failure"))
        })
        .unwrap_err();
        assert!(format!("{failed:#}").contains("injected sync failure"));
        assert!(
            path.is_dir(),
            "the failed start leaves the directory behind"
        );

        // The next start finds it and must still sync the parent.
        let mut synced = Vec::new();
        let created = recovery_directory(&path, Missing::Create, |directory| {
            synced.push(directory.to_path_buf());
            Ok(())
        })
        .unwrap();
        assert!(!created);
        assert_eq!(synced, vec![root.clone()]);
        // And a failure on that start stops it too.
        assert!(recovery_directory(&path, Missing::Create, |_| {
            Err(std::io::Error::other("injected sync failure"))
        })
        .is_err());
        std::fs::remove_dir(&path).unwrap();
        std::fs::remove_dir(&root).unwrap();
    }

    #[cfg(unix)]
    #[test]
    fn a_recovery_directory_is_never_created_under_a_parent_others_can_change() {
        use std::os::unix::fs::PermissionsExt;
        // Another account could swap the new directory for one of its own
        // before the ledger writes held-work records into it.
        let state = state();
        let root = ledger_dir("open-parent");
        private_dir_all(&root);
        std::fs::set_permissions(&root, std::fs::Permissions::from_mode(0o777)).unwrap();
        let path = root.join("admission");

        let refused = Ledger::open(&path, &state.config, Missing::Create)
            .err()
            .unwrap();

        assert!(
            format!("{refused:#}").contains("writable by group or others"),
            "{refused:#}"
        );
        assert!(!path.exists());
        std::fs::remove_dir(&root).unwrap();
    }

    #[cfg(unix)]
    #[test]
    fn a_recovery_directory_another_account_owns_is_refused() {
        use std::os::unix::fs::{DirBuilderExt, MetadataExt};
        let path = ledger_dir("owner");
        std::fs::DirBuilder::new()
            .mode(0o700)
            .create(&path)
            .unwrap();
        let meta = std::fs::metadata(&path).unwrap();
        assert!(ensure_owned_by(&path, &meta, meta.uid()).is_ok());
        let refused = ensure_owned_by(&path, &meta, meta.uid().wrapping_add(1))
            .unwrap_err()
            .to_string();
        assert!(
            refused.contains(&format!("owned by uid {}", meta.uid())),
            "{refused}"
        );
        // Through the ledger itself, where the test can hand the directory
        // to another account.
        // SAFETY: geteuid has no preconditions.
        if unsafe { libc::geteuid() } == 0 {
            std::os::unix::fs::chown(&path, Some(65534), None).unwrap();
            let state = state();
            let refused = Ledger::open(&path, &state.config, Missing::Create)
                .err()
                .expect("another account's directory is refused");
            assert!(format!("{refused:#}").contains("owned by uid 65534"));
            assert!(!path.join(".lock").exists(), "nothing is written there");
        }
        std::fs::remove_dir(&path).unwrap();
    }
}
