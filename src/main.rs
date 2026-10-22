//! vacancyd gates borrowed AI requests on owner policy and available resources.
//!
//! Host-scoped upstreams apply owner presence; resource-scoped upstreams can
//! keep lending while the owner is present. Accepted work follows the configured
//! cancel, drain or finish policy. Closing a proxy connection does not prove that
//! the upstream stopped computing or released memory.

#[cfg(any(windows, test))]
mod acl_policy;
mod adapters;
#[cfg(target_os = "linux")]
mod admin_socket;
mod admission;
mod agent;
mod api;
mod auth;
mod child_process;
mod collect;
mod config;
mod diagnostics;
mod durable;
mod elevation;
mod gate;
mod lifecycle;
mod listener;
mod log_control;
mod metrics;
mod plugin;
mod presence_impl;
mod pressure_history;
mod probes;
#[cfg(any(windows, test))]
mod protected_root;
mod proxy;
mod reclaim_log;
#[cfg(target_os = "linux")]
mod sandbox;
mod secure_file;
#[cfg(any(windows, test))]
mod service_log;
mod setup;
mod state;
#[cfg(test)]
mod test_log;
mod upstream_credential;
#[cfg(windows)]
mod windows_acl;
mod windows_runtime;
#[cfg(windows)]
mod windows_service_host;
mod work_cancellation;
mod yield_control;

use std::future::Future;
use std::path::PathBuf;
use std::sync::Arc;

use anyhow::{anyhow, Context, Result};
use clap::{Parser, Subcommand};

#[derive(Parser)]
#[command(name = "vacancyd", version, about)]
struct Cli {
    /// Keep the console open after completion (for interactive shortcuts).
    #[arg(long, global = true)]
    pause: bool,
    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand)]
enum Command {
    /// Run the daemon.
    Run {
        /// Path to the TOML config file.
        #[arg(short, long, default_value = "vacancyd.toml")]
        config: PathBuf,
    },
    /// Validate a config file and exit.
    CheckConfig {
        #[arg(short, long, default_value = "vacancyd.toml")]
        config: PathBuf,
    },
    /// Protect installed configuration for Administrators and LocalSystem.
    #[cfg(windows)]
    ProtectConfig {
        #[arg(long)]
        config: PathBuf,
        /// Installer mode: refuse a configuration not already owned by
        /// Administrators or LocalSystem instead of taking ownership of it.
        #[arg(long)]
        require_admin_owner: bool,
    },
    /// Migrate config bytes and sibling credential files into a protected directory.
    ///
    /// Missing destination directories and parents are created private only
    /// after the path checks pass. A failed check stops migration. On Windows
    /// the destination directory must be named vacancyd and must be
    /// ProgramData\vacancyd itself or a folder inside it. The protected root
    /// must already exist.
    MigrateConfig {
        #[arg(long)]
        source: PathBuf,
        #[arg(long)]
        config: PathBuf,
        #[arg(long)]
        replace_starter: bool,
    },
    /// Guided first-run configuration (does not restart or install services).
    ///
    /// Missing destination directories and parents are created private only
    /// after the path checks pass. A failed check stops setup. On Windows
    /// the destination must be inside ProgramData\vacancyd.
    Setup {
        #[arg(short, long, default_value = "vacancyd.toml")]
        config: PathBuf,
        /// Allow replacement of the exact, unmodified MSI starter only.
        #[arg(long)]
        replace_starter: bool,
    },
    /// Discover candidate NVIDIA GPUs and local inference services; change nothing.
    Discover,
    /// Run the user-session agent (Windows).
    ///
    /// One per interactive session. Reports idle time and the foreground
    /// application to the service, which cannot see either from session 0.
    /// Safe to run anywhere: if the service is not up it retries quietly.
    #[cfg(windows)]
    Agent {
        /// Seconds between reports.
        #[arg(short, long, default_value_t = 3)]
        interval: u64,
    },
    /// Send one local admin request to the running service.
    ///
    /// Reads a JSON request from a file and sends it to the service as the
    /// account running this command (over the admin pipe on Windows, the
    /// admin socket on Linux), then exits with the result code documented in
    /// docs/authentication.md. Anyone may pause and resume their own pause.
    /// Saving settings, switching a resource and releasing held work need an
    /// elevated prompt on Windows, and root or the admin group on Linux. The
    /// Windows owner console runs this through the approval prompt.
    #[cfg(any(windows, target_os = "linux"))]
    AdminRequest {
        /// The request file.
        #[arg(long)]
        request: PathBuf,
        /// The service's admin socket, when its configuration sets
        /// `server.admin_socket`.
        #[cfg(target_os = "linux")]
        #[arg(long, default_value = vacancyd::local_admin::DEFAULT_SOCKET_PATH)]
        socket: PathBuf,
    },
    /// Generate a token suitable for [[auth.token]].
    ///
    /// 256 bits from the OS random source, URL-safe so it survives shells,
    /// YAML and HTTP headers without quoting. Printed once and never
    /// stored - put it straight into a secret file or environment variable.
    GenToken,
    /// Query a running daemon and print its status.
    Status {
        /// Address of the daemon's API listener.
        #[arg(short, long, default_value = "127.0.0.1:9811")]
        addr: String,
    },
    /// Save what a running daemon saw and decided, for a report.
    Diagnostics {
        #[command(subcommand)]
        command: DiagnosticsCommand,
    },
    /// Exit 0 if the daemon configured by this file answers `/healthz`.
    ///
    /// For container and supervisor health checks. The API address comes
    /// from `server.listen`; an unspecified bind (0.0.0.0 or [::]) is probed
    /// on loopback. `/healthz` needs no token. Any failure exits non-zero.
    Healthcheck {
        #[arg(short, long, default_value = "vacancyd.toml")]
        config: PathBuf,
    },
    /// Run under the Windows Service Control Manager.
    ///
    /// This entry point is installed by the MSI. Use `run` from an interactive
    /// terminal; starting this command outside the SCM returns a Windows error.
    #[cfg(windows)]
    #[command(hide = true)]
    Service {
        /// Path to the service's TOML configuration.
        #[arg(
            short,
            long,
            default_value = "C:\\ProgramData\\vacancyd\\vacancyd.toml"
        )]
        config: PathBuf,
        /// Append service logs here, inside ProgramData\vacancyd; missing parent
        /// directories are created protected only after the path checks pass.
        /// A failed check stops startup.
        #[arg(long, default_value = "C:\\ProgramData\\vacancyd\\vacancyd.log")]
        log_file: PathBuf,
    },
}

#[derive(Subcommand)]
enum DiagnosticsCommand {
    /// Write the daemon's diagnostics export as JSON: status, running
    /// settings with credentials redacted, log level, saved pressure
    /// calibration and recent pressure samples.
    ///
    /// Reads `GET /admin/diagnostics` over loopback, which any local user
    /// may do, like `status`.
    Export {
        /// Address of the daemon's API listener.
        #[arg(short, long, default_value = "127.0.0.1:9811")]
        addr: String,
        /// Write here instead of standard output. An existing file is
        /// replaced.
        #[arg(short, long)]
        out: Option<PathBuf>,
    },
}

fn main() -> std::process::ExitCode {
    let cli = Cli::parse();
    let pause = cli.pause;
    let Err(error) = start(cli.command) else {
        return std::process::ExitCode::SUCCESS;
    };
    if pause {
        eprintln!("{error:#}");
        eprintln!("Press Enter to close.");
        let _ = std::io::stdin().read_line(&mut String::new());
    }
    eprintln!("Error: {error:?}");
    std::process::ExitCode::from(exit_code(&error))
}

/// 1 for an ordinary failure; a startup failure the operator can fix has its
/// own code (see `elevation`).
fn exit_code(error: &anyhow::Error) -> u8 {
    elevation::startup_error(error).map_or(1, |startup| startup.kind.exit_code() as u8)
}

/// Only the daemon logs to standard output, where foreground runs and the
/// systemd journal have always read it. Every other command keeps standard
/// output for its result, such as a diagnostics export.
fn logs_to_stdout(command: &Command) -> bool {
    matches!(command, Command::Run { .. })
}

fn start(command: Command) -> Result<()> {
    #[cfg(windows)]
    if let Command::Service {
        ref config,
        ref log_file,
    } = command
    {
        init_file_logging(log_file).map_err(|error| {
            elevation::explain(
                format!("opening the service log {}", log_file.display()),
                error,
            )
        })?;
        return windows_service_host::dispatch(config.clone())
            .map_err(|error| elevation::explain("starting the service", error));
    }

    if logs_to_stdout(&command) {
        log_control::install_stdout();
    } else {
        log_control::install_stderr();
    }
    let runtime = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
        .context("building async runtime")?;
    let result = runtime.block_on(execute(command));
    // Dropping a runtime waits for every blocking task, and a presence sample
    // stuck in an operating-system call would hold exit forever. Wait as long
    // as the Windows service host does for probes to end, then exit anyway.
    runtime.shutdown_timeout(FOREGROUND_SHUTDOWN);
    result
}

/// Every probe subprocess has a deadline of at most 5 s and the in-process
/// GPU counter sample returns within 8 s, so this lets a sample in progress
/// end; it matches the service host's `RUNTIME_SHUTDOWN`.
const FOREGROUND_SHUTDOWN: std::time::Duration = std::time::Duration::from_secs(9);

#[cfg(windows)]
fn init_file_logging(path: &std::path::Path) -> Result<()> {
    let file = service_log::ServiceLog::open(path)
        .with_context(|| format!("opening bounded service log {}", path.display()))?;
    log_control::install_service_log(file);
    Ok(())
}

async fn execute(command: Command) -> Result<()> {
    match command {
        #[cfg(windows)]
        Command::ProtectConfig {
            config,
            require_admin_owner,
        } => {
            windows_acl::protect_config(&config, require_admin_owner)
                .context("protecting configuration permissions")
                .map_err(|error| {
                    elevation::explain(format!("protecting {}", config.display()), error)
                })?;
            println!("Configuration permissions verified; bytes unchanged.");
            Ok(())
        }
        Command::MigrateConfig {
            source,
            config,
            replace_starter,
        } => {
            setup::migrate(&source, &config, replace_starter).map_err(|error| {
                elevation::explain(format!("migrating into {}", config.display()), error)
            })?;
            println!("Migration complete; source bytes preserved and service not restarted.");
            Ok(())
        }
        Command::Setup {
            config,
            replace_starter,
        } => setup::run(&config, replace_starter)
            .await
            .map_err(|error| elevation::explain(format!("setting up {}", config.display()), error)),
        Command::Discover => {
            println!(
                "{}",
                serde_json::to_string_pretty(&setup::discover().await?)?
            );
            Ok(())
        }
        Command::Run { config } => run(config).await,
        Command::CheckConfig { config } => {
            let reading = format!("reading {}", config.display());
            let cfg = config::Config::load(&config)
                .map_err(|error| elevation::explain(reading.clone(), error))?;
            // Resolve credentials too. Without this, a config naming a
            // secret file that does not exist validates cleanly and then
            // fails at service start — which is exactly the moment a deploy
            // script has already stopped the old process.
            let credentials = cfg.resolve_credentials().map_err(|error| {
                elevation::explain("reading the credentials the configuration names", error)
            })?;
            #[cfg(target_os = "linux")]
            sandbox::warn_blocked_paths(&cfg, &config);
            cfg.log_owner_scopes();
            cfg.log_setting_warnings();
            collect::pressure::warn_shared_cache_directory(&cfg.owner.gpu_pressure, &config);
            let tokens = credentials.auth.token_count();
            let upstreams = credentials.upstreams.len();
            println!(
                "{} OK ({tokens} auth token{}, {upstreams} upstream credential{})",
                config.display(),
                if tokens == 1 { "" } else { "s" },
                if upstreams == 1 { "" } else { "s" }
            );
            Ok(())
        }
        #[cfg(windows)]
        Command::Agent { interval } => {
            agent::client::run(std::time::Duration::from_secs(interval), true).await
        }
        #[cfg(windows)]
        Command::AdminRequest { request } => {
            std::process::exit(vacancyd::local_admin::client::run_helper(&request).await)
        }
        #[cfg(target_os = "linux")]
        Command::AdminRequest { request, socket } => std::process::exit(
            vacancyd::local_admin::socket_client::run_helper(&request, &socket).await,
        ),
        Command::GenToken => {
            println!("{}", auth::generate_token()?);
            Ok(())
        }
        Command::Status { addr } => status(&addr).await,
        Command::Diagnostics {
            command: DiagnosticsCommand::Export { addr, out },
        } => export_diagnostics(&addr, out.as_deref()).await,
        Command::Healthcheck { config } => {
            let reading = format!("reading {}", config.display());
            let config = config::Config::load(&config)
                .map_err(|error| elevation::explain(reading, error))?;
            healthcheck(health_probe_addr(&config.server.listen)?).await
        }
        #[cfg(windows)]
        Command::Service { .. } => unreachable!("service command is dispatched before runtime"),
    }
}

async fn run(config_path: PathBuf) -> Result<()> {
    run_until(config_path, shutdown_signal(), || Ok(())).await
}

async fn run_until<S, F>(config_path: PathBuf, shutdown: S, on_started: F) -> Result<()>
where
    S: Future<Output = ()>,
    F: FnOnce() -> Result<()>,
{
    // On Windows the daemon's state is private to Administrators and
    // LocalSystem, and an unelevated start cannot get through it. Say so
    // before touching anything.
    #[cfg(windows)]
    elevation::require_elevated("running the vacancyd daemon")?;
    let config = config::Config::load(&config_path)
        .map_err(|error| elevation::explain(format!("reading {}", config_path.display()), error))?;
    config.log_owner_scopes();
    config.log_setting_warnings();
    #[cfg(windows)]
    if !config
        .server
        .listen
        .parse::<std::net::SocketAddr>()?
        .ip()
        .is_loopback()
        || config.upstreams.iter().any(|u| {
            u.listen
                .parse::<std::net::SocketAddr>()
                .is_ok_and(|a| !a.ip().is_loopback())
        })
    {
        windows_acl::verify_config(&config_path)
            .context("refusing LAN lending with insecure configuration permissions")
            .map_err(|error| {
                elevation::explain(
                    format!("checking the permissions of {}", config_path.display()),
                    error,
                )
            })?;
    }
    let state_directory = std::env::var_os("STATE_DIRECTORY");
    let recovery =
        admission::RecoveryDir::resolve(&config, &config_path, state_directory.as_deref());
    #[cfg(target_os = "linux")]
    if let Some(warning) = admission::check_beside_service(
        &config,
        &recovery,
        state_directory.as_deref(),
        std::path::Path::new(admission::SERVICE_RECOVERY_DIR),
    )? {
        tracing::warn!("{warning}");
    }
    let ledger = admission::Ledger::open(&recovery.path, &config, recovery.missing)
        .context("open admission recovery records")
        .map_err(|error| {
            elevation::explain(
                format!(
                    "opening the admission recovery records in {}",
                    recovery.path.display()
                ),
                error,
            )
        })?;
    let state = Arc::new(
        state::AppState::new_with_config_path(config, &config_path).map_err(|error| {
            elevation::explain("reading the credentials the configuration names", error)
        })?,
    );
    *state.admission.lock().unwrap() = ledger;
    state.log.configure(state.config.log.level);
    let tracked_gpus = if state.config.owner.gpu_pressure.enabled {
        collect::pressure::lendable_gpu_ids(&state.config).len()
    } else {
        0
    };
    state
        .log
        .limit_debug_window(log_control::debug_window_limit(
            state.config.owner.gpu_pressure.poll_ms,
            tracked_gpus,
        ));
    // Not an essential task: a status read also closes a window that is
    // due, so losing this would only delay the filter change.
    let log_supervisor = tokio::spawn(state.log.clone().supervise());

    let mut tasks: tokio::task::JoinSet<Result<()>> = tokio::task::JoinSet::new();
    let collector_state = state.clone();
    let collector_config = config_path.clone();
    tasks.spawn(async move {
        collect::run(collector_state, collector_config).await;
        Err(anyhow!("collector exited unexpectedly"))
    });
    for upstream in state.config.upstreams.clone() {
        let st = state.clone();
        let id = upstream.id.clone();
        tasks.spawn(async move {
            proxy::serve(st, upstream)
                .await
                .with_context(|| format!("proxy '{id}' exited"))
        });
    }
    {
        let st = state.clone();
        tasks.spawn(async move { api::serve(st).await.context("API server exited") });
    }
    // The agent channel is optional: if the pipe cannot be created the
    // daemon carries on with its own conservative view rather than failing
    // to start, and keeps retrying the channel with backoff.
    #[cfg(windows)]
    let agent_task = tokio::spawn(agent::server::serve(state.clone()));
    // Local admin actions that need a verified account arrive here. Without
    // it, a local owner can still read status and pause over loopback.
    #[cfg(windows)]
    let admin_task = tokio::spawn(agent::admin_server::serve(state.clone()));
    #[cfg(target_os = "linux")]
    let admin_task = tokio::spawn(admin_socket::serve(state.clone()));

    on_started()?;

    tokio::select! {
        _ = shutdown => {
            tracing::info!("shutting down");
            let window = admission::shutdown_drain_window(&state.config);
            admission::drain_for_shutdown(&state, window).await;
            tasks.abort_all();
            log_supervisor.abort();
            #[cfg(windows)]
            agent_task.abort();
            #[cfg(any(windows, target_os = "linux"))]
            {
                admin_task.abort();
                let _ = admin_task.await;
            }
            Ok(())
        }
        failure = essential_failure(&mut tasks) => {
            tasks.abort_all();
            log_supervisor.abort();
            #[cfg(windows)]
            agent_task.abort();
            #[cfg(any(windows, target_os = "linux"))]
            {
                admin_task.abort();
                let _ = admin_task.await;
            }
            Err(failure)
        }
    }
}

async fn essential_failure(tasks: &mut tokio::task::JoinSet<Result<()>>) -> anyhow::Error {
    match tasks.join_next().await {
        Some(Ok(Err(error))) => error,
        Some(Ok(Ok(()))) => anyhow!("essential task exited unexpectedly"),
        Some(Err(error)) => anyhow!("essential task failed: {error}"),
        None => anyhow!("all essential tasks exited unexpectedly"),
    }
}

/// Wait for any shutdown request.
///
/// A console run can receive more than Ctrl-C: close, shutdown, and break
/// events should all take the same graceful path. The native SCM entry point
/// supplies its own stop future instead of calling this function.
async fn shutdown_signal() {
    #[cfg(windows)]
    {
        use tokio::signal::windows;
        let mut ctrl_c = windows::ctrl_c().expect("ctrl_c handler");
        let mut close = windows::ctrl_close().expect("ctrl_close handler");
        let mut shutdown = windows::ctrl_shutdown().expect("ctrl_shutdown handler");
        let mut brk = windows::ctrl_break().expect("ctrl_break handler");
        tokio::select! {
            _ = ctrl_c.recv() => {}
            _ = close.recv() => {}
            _ = shutdown.recv() => {}
            _ = brk.recv() => {}
        }
    }
    #[cfg(not(windows))]
    {
        use tokio::signal::unix::{signal, SignalKind};
        let mut term = signal(SignalKind::terminate()).expect("SIGTERM handler");
        tokio::select! {
            _ = tokio::signal::ctrl_c() => {}
            _ = term.recv() => {}
        }
    }
}

/// Read `/status` the way the daemon's other local clients do: straight to
/// the address given, never through an `HTTP_PROXY`-style setting meant for
/// web browsing, and without following a redirect elsewhere.
async fn fetch_status(addr: &str) -> Result<serde_json::Value> {
    let client = reqwest::Client::builder()
        .no_proxy()
        .redirect(reqwest::redirect::Policy::none())
        .timeout(std::time::Duration::from_secs(10))
        .build()?;
    Ok(client
        .get(format!("http://{addr}/status"))
        .send()
        .await?
        .error_for_status()?
        .json()
        .await?)
}

/// `vacancyd diagnostics export`: fetch the export the way `status` reads
/// `/status`, then write it where asked.
async fn export_diagnostics(addr: &str, out: Option<&std::path::Path>) -> Result<()> {
    let client = reqwest::Client::builder()
        .no_proxy()
        .redirect(reqwest::redirect::Policy::none())
        .build()?;
    let url = reqwest::Url::parse(&format!("http://{addr}/admin/diagnostics"))
        .with_context(|| format!("{addr:?} is not a host:port address"))?;
    let document = vacancyd::status_client::fetch_diagnostics(&client, url)
        .await
        .map_err(|reason| anyhow!("{reason}"))?;
    match out {
        Some(path) => {
            std::fs::write(path, &document)
                .with_context(|| format!("writing {}", path.display()))?;
            eprintln!(
                "Wrote diagnostics ({} KiB) to {}",
                document.len().div_ceil(1024),
                path.display()
            );
        }
        None => {
            use std::io::Write;
            std::io::stdout().write_all(&document)?;
        }
    }
    Ok(())
}

/// Where a local health probe reaches the API listener bound at `listen`.
/// A wildcard bind accepts loopback connections of its own address family.
fn health_probe_addr(listen: &str) -> Result<std::net::SocketAddr> {
    use std::net::{IpAddr, Ipv4Addr, Ipv6Addr};
    let mut addr: std::net::SocketAddr = listen
        .parse()
        .with_context(|| format!("server.listen {listen:?} is not an IP socket address"))?;
    if addr.ip().is_unspecified() {
        addr.set_ip(match addr.ip() {
            IpAddr::V4(_) => IpAddr::V4(Ipv4Addr::LOCALHOST),
            IpAddr::V6(_) => IpAddr::V6(Ipv6Addr::LOCALHOST),
        });
    }
    Ok(addr)
}

/// Liveness only. `/healthz` answers while the API task serves; readiness is
/// deliberately not required, so a daemon that is up but refusing to lend
/// (no GPU visible, presence unknown) stays healthy and explains itself in
/// `/status` instead of being restarted in a loop.
async fn healthcheck(addr: std::net::SocketAddr) -> Result<()> {
    let client = reqwest::Client::builder()
        .no_proxy()
        .redirect(reqwest::redirect::Policy::none())
        .timeout(std::time::Duration::from_secs(3))
        .build()?;
    let response = client
        .get(format!("http://{addr}/healthz"))
        .send()
        .await
        .with_context(|| format!("no answer from http://{addr}/healthz"))?;
    let code = response.status();
    let body = response.text().await.unwrap_or_default();
    if code != reqwest::StatusCode::OK || body != "ok" {
        return Err(anyhow!("http://{addr}/healthz answered {code}"));
    }
    Ok(())
}

async fn status(addr: &str) -> Result<()> {
    let body = fetch_status(addr).await?;
    for line in format_lending_header(&body) {
        println!("{line}");
    }
    if let Some(line) = format_logging(&body["logging"]) {
        println!("{line}");
    }
    if let Some(line) = format_admin_channel(&body["admin_channel"]) {
        println!("{line}");
    }
    for line in format_held_work(&body["work"]) {
        println!("{line}");
    }
    if let Some(error) = body["presence"]["media_error"].as_str() {
        println!("  media detection unavailable: {error}");
    }
    for r in body["resources"].as_array().into_iter().flatten() {
        let quality = r["capacity"]["quality"].as_str().unwrap_or("unknown");
        if quality != "ready" {
            let age = r["capacity"]["age_s"]
                .as_u64()
                .map(|seconds| format!(", last success {seconds}s ago"))
                .unwrap_or_default();
            let error = r["capacity"]["last_error"]
                .as_str()
                .map(|message| format!(": {message}"))
                .unwrap_or_default();
            println!(
                "  {:<6} capacity {quality}{age}{error}   {}",
                r["id"].as_str().unwrap_or("?"),
                if r["enabled"].as_bool().unwrap_or(false) {
                    "lending requested"
                } else {
                    "disabled"
                },
            );
        } else {
            println!("{}", format_ready_resource(r));
        }

        if !r["adapter_mapping"].is_null() {
            let mapping = &r["adapter_mapping"];
            let quality = mapping["quality"].as_str().unwrap_or("unknown");
            if quality == "ready" {
                println!(
                    "         adapter {} -> {}   source {}",
                    mapping["pci"].as_str().unwrap_or("?"),
                    mapping["luid"].as_str().unwrap_or("?"),
                    mapping["source"].as_str().unwrap_or("?"),
                );
            } else {
                let error = mapping["last_error"]
                    .as_str()
                    .map(|message| format!(": {message}"))
                    .unwrap_or_default();
                println!("         adapter mapping {quality}{error}");
            }
        }

        if !r["pressure"].is_null() {
            let pressure = &r["pressure"];
            let pressure_quality = pressure["quality"].as_str().unwrap_or("unknown");
            if pressure_quality == "ready" {
                println!(
                    "         pressure {:>5.1}% / {:>5.1}% threshold   {}",
                    pressure["non_lender_pct"].as_f64().unwrap_or(0.0),
                    pressure["threshold_pct"].as_f64().unwrap_or(0.0),
                    if !pressure["calibrated"].as_bool().unwrap_or(false) {
                        "CALIBRATING (lending refused)"
                    } else if pressure["engaged"].as_bool().unwrap_or(false) {
                        "REFUSING"
                    } else {
                        "clear"
                    },
                );
                if let (Some(threshold), Some(target)) = (
                    pressure["threshold_pct"].as_f64(),
                    pressure["target_pct"].as_f64(),
                ) {
                    if target > threshold {
                        println!(
                            "         threshold rising toward {target:.1}% (learned from {:.0} h)",
                            pressure["learned_s"].as_f64().unwrap_or(0.0) / 3600.0
                        );
                    }
                }
                if let Some(hint) = pressure["calibration_hint"].as_str() {
                    println!(
                        "         calibration {}/{}: {hint}",
                        pressure["calibration_samples"], pressure["calibration_required"]
                    );
                }
            } else {
                let age = pressure["age_s"]
                    .as_u64()
                    .map(|seconds| format!(", last success {seconds}s ago"))
                    .unwrap_or_default();
                let error = pressure["last_error"]
                    .as_str()
                    .map(|message| format!(": {message}"))
                    .unwrap_or_default();
                println!("         pressure {pressure_quality}{age}{error}");
            }
        }
    }
    for u in body["upstreams"].as_array().into_iter().flatten() {
        println!("{}", format_upstream_status(u));
        if let Some(line) = format_unseen_gpu_work(&u["unseen_gpu_work"]) {
            println!("{line}");
        }
    }
    Ok(())
}

/// The first lines of `vacancyd status`: the lending switch, the owner veto,
/// and the upstreams the veto does not apply to. While the veto is active an
/// exempt upstream can still be lending, so the header says which.
fn format_lending_header(body: &serde_json::Value) -> Vec<String> {
    let veto = &body["owner_veto"];
    let active = veto["active"].as_bool().unwrap_or(false);
    let mut lines = vec![format!(
        "lending: {}   owner veto: {}{}",
        if body["lending_enabled"].as_bool().unwrap_or(false) {
            "ON"
        } else {
            "OFF"
        },
        if active { "ACTIVE" } else { "clear" },
        veto["cause"]
            .as_str()
            .map(|c| format!(" ({c})"))
            .unwrap_or_default(),
    )];
    let exempt: Vec<&str> = veto["exempt_upstreams"]
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(serde_json::Value::as_str)
        .collect();
    if exempt.is_empty() {
        return lines;
    }
    if !active {
        lines.push(format!(
            "  exempt from the owner veto (owner_scope = \"resource\"): {}",
            exempt.join(", ")
        ));
        return lines;
    }
    // Serving means admitting new work or still holding accepted work.
    let serving: Vec<&str> = exempt
        .iter()
        .copied()
        .filter(|id| {
            body["upstreams"]
                .as_array()
                .into_iter()
                .flatten()
                .any(|u| u["id"].as_str() == Some(id) && u["vacancy"].as_bool() == Some(true))
                || body["work"]
                    .as_array()
                    .into_iter()
                    .flatten()
                    .any(|w| w["upstream"].as_str() == Some(id))
        })
        .collect();
    lines.push(if serving.is_empty() {
        format!(
            "  paused for host-scoped upstreams; exempt from the veto but not lending now: {}",
            exempt.join(", ")
        )
    } else {
        format!(
            "  paused for host-scoped upstreams only; {} exempt upstream{} still lending: {}",
            serving.len(),
            if serving.len() == 1 { "" } else { "s" },
            serving.join(", ")
        )
    });
    lines
}

/// A line about logging, only when it differs from the default `info` from
/// the configuration.
fn format_logging(logging: &serde_json::Value) -> Option<String> {
    let level = logging["level"].as_str()?;
    if let Some(until) = logging["debug_until"].as_str() {
        let minutes = logging["debug_remaining_s"]
            .as_u64()
            .map(|seconds| format!(", {} min left", seconds.div_ceil(60)))
            .unwrap_or_default();
        return Some(format!(
            "  logging: {level} until {until}{minutes} (debug window)"
        ));
    }
    if logging["environment_override"].as_bool() == Some(true) {
        return Some("  logging: RUST_LOG filter in force".into());
    }
    (level != "info").then(|| format!("  logging: {level}"))
}

/// A line only while the local admin channel is down: the service keeps
/// running without it and says why only in its log.
fn format_admin_channel(channel: &serde_json::Value) -> Option<String> {
    (channel["state"].as_str() == Some("unavailable")).then(|| {
        "  local admin channel unavailable: loopback reads and pauses still work, and HTTP administration remains available with an admin token; the service log says why".into()
    })
}

/// A line for each held reservation. Held work keeps its resources from
/// every upstream that shares them until an administrator releases it, and
/// the upstream lines only say that a job owns the resource.
fn format_held_work(work: &serde_json::Value) -> Vec<String> {
    work.as_array()
        .into_iter()
        .flatten()
        .filter(|entry| entry["phase"].as_str() == Some("held"))
        .map(|entry| {
            let resources: Vec<&str> = entry["resources"]
                .as_array()
                .into_iter()
                .flatten()
                .filter_map(|resource| resource.as_str())
                .collect();
            format!(
                "  work {} held {}s: {} on {}; completion unconfirmed. Stop or verify it at the upstream, then release it with release_work",
                entry["id"],
                entry["age_s"].as_u64().unwrap_or(0),
                entry["upstream"].as_str().unwrap_or("?"),
                if resources.is_empty() { "?".to_owned() } else { resources.join(", ") },
            )
        })
        .collect()
}

/// One resource line for a ready capacity sample. A value the service did
/// not publish reads as unknown, never as zero, and a CPU has no byte line.
fn format_ready_resource(r: &serde_json::Value) -> String {
    let utilization = r["utilization_pct"]
        .as_u64()
        .map(|percent| format!("{percent}%"))
        .unwrap_or_else(|| "unknown".into());
    let mib = |field: &str| {
        r[field]
            .as_u64()
            .map(|mib| mib.to_string())
            .unwrap_or_else(|| "unknown".into())
    };
    let capacity = if r["kind"].as_str() == Some("system-cpu") {
        String::new()
    } else {
        format!(
            "{:>8} MiB free / {:>8} MiB   ",
            mib("free_mib"),
            mib("total_mib")
        )
    };
    format!(
        "  {:<6} {:<28} {capacity}util {:>7}   {}",
        r["id"].as_str().unwrap_or("?"),
        r["label"].as_str().unwrap_or(""),
        utilization,
        if r["enabled"].as_bool().unwrap_or(false) {
            "lending"
        } else {
            "disabled"
        },
    )
}

/// A line under an upstream whose borrowed GPU work pressure may not see.
fn format_unseen_gpu_work(unseen: &serde_json::Value) -> Option<String> {
    let resources: Vec<&str> = unseen["resources"]
        .as_array()?
        .iter()
        .filter_map(serde_json::Value::as_str)
        .collect();
    let on = if resources.is_empty() {
        String::new()
    } else {
        format!(" on {}", resources.join(", "))
    };
    Some(format!(
        "               GPU pressure{on} may not see this upstream's work and may count it as owner load; the service log says why"
    ))
}

fn format_upstream_status(upstream: &serde_json::Value) -> String {
    let availability = if upstream["vacancy"].as_bool().unwrap_or(false) {
        "VACANCY".to_owned()
    } else {
        let refusal = &upstream["refusal"];
        let detail = refusal["detail"].as_str().unwrap_or("reason unavailable");
        let retry = refusal["retry_after_s"]
            .as_u64()
            .map(|seconds| format!(", retry in {seconds}s"))
            .unwrap_or_default();
        format!("NO VACANCY: {detail}{retry}")
    };
    format!(
        "  {:<12} {} -> {}   {}{}",
        upstream["id"].as_str().unwrap_or("?"),
        upstream["listen"].as_str().unwrap_or(""),
        // The daemon already redacts it; a status from anything else is not
        // trusted to have done so.
        upstream["target"]
            .as_str()
            .map(|target| vacancyd::status::RedactedUrl(target).to_string())
            .unwrap_or_default(),
        availability,
        if upstream["owner_scope"].as_str() == Some(vacancyd::status::OWNER_SCOPE_RESOURCE) {
            "   [exempt from owner veto]"
        } else {
            ""
        },
    )
}

#[cfg(test)]
mod task_tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn status_cli_explains_a_structured_upstream_refusal() {
        let upstream = json!({
            "id": "ollama",
            "listen": "127.0.0.1:11435",
            "target": "http://127.0.0.1:11434",
            "vacancy": false,
            "refusal": {
                "reason": "resource_unmeasured",
                "detail": "resource 'gpu0' capacity is unknown",
                "retry_after_s": 30
            }
        });

        let rendered = format_upstream_status(&upstream);

        assert!(rendered.contains("NO VACANCY: resource 'gpu0' capacity is unknown"));
        assert!(rendered.contains("retry in 30s"));
    }

    fn exempt_status(veto: bool, chatterbox_vacancy: bool) -> serde_json::Value {
        json!({
            "lending_enabled": true,
            "owner_veto": {
                "active": veto,
                "cause": veto.then_some("owner active"),
                "exempt_upstreams": ["chatterbox"]
            },
            "upstreams": [
                {"id": "ollama", "owner_scope": "host", "vacancy": false},
                {"id": "chatterbox", "owner_scope": "resource", "vacancy": chatterbox_vacancy}
            ],
            "work": []
        })
    }

    #[test]
    fn status_cli_says_the_veto_spares_an_exempt_upstream_that_is_lending() {
        let lines = format_lending_header(&exempt_status(true, true));
        assert_eq!(lines[0], "lending: ON   owner veto: ACTIVE (owner active)");
        assert_eq!(
            lines[1],
            "  paused for host-scoped upstreams only; 1 exempt upstream still lending: chatterbox"
        );

        // Holding accepted work counts as lending even without a vacancy.
        let mut busy = exempt_status(true, false);
        busy["work"] = json!([{"upstream": "chatterbox"}]);
        assert!(format_lending_header(&busy)[1].contains("still lending: chatterbox"));

        let idle = format_lending_header(&exempt_status(true, false));
        assert_eq!(
            idle[1],
            "  paused for host-scoped upstreams; exempt from the veto but not lending now: chatterbox"
        );

        let clear = format_lending_header(&exempt_status(false, true));
        assert_eq!(clear[0], "lending: ON   owner veto: clear");
        assert!(clear[1].contains("exempt from the owner veto"), "{clear:?}");

        let upstream = &exempt_status(true, true)["upstreams"][1];
        assert!(format_upstream_status(upstream).ends_with("[exempt from owner veto]"));
        let host = &exempt_status(true, true)["upstreams"][0];
        assert!(!format_upstream_status(host).contains("exempt"));
    }

    #[test]
    fn status_cli_header_is_one_line_without_exemptions() {
        let body = json!({"lending_enabled": false, "owner_veto": {"active": true, "cause": null}});
        assert_eq!(
            format_lending_header(&body),
            ["lending: OFF   owner veto: ACTIVE"]
        );
    }

    #[test]
    fn status_cli_warns_under_an_upstream_whose_gpu_work_pressure_may_not_see() {
        let unseen = |value: serde_json::Value| format_unseen_gpu_work(&value);
        assert_eq!(unseen(json!(null)), None, "absent while the work is seen");
        assert_eq!(
            unseen(json!({"resources": ["gpu0"]})).unwrap(),
            "               GPU pressure on gpu0 may not see this upstream's work and may count it as owner load; the service log says why"
        );
        assert!(unseen(json!({"resources": ["gpu0", "gpu1"]}))
            .unwrap()
            .contains("GPU pressure on gpu0, gpu1 may not see"));
    }

    #[test]
    fn status_cli_names_each_held_reservation_and_how_to_release_it() {
        assert!(
            format_held_work(&json!(null)).is_empty(),
            "a daemon without work"
        );
        let work = json!([
            {"id": 44, "upstream": "comfyui", "resources": ["gpu0", "mem"], "age_s": 4500,
             "completion": "tracked", "phase": "held", "tracking_id": "f03745eb", "model_state": "unknown"},
            {"id": 45, "upstream": "ollama", "resources": ["gpu0"], "age_s": 3,
             "completion": "response_close", "phase": "forwarding", "tracking_id": null},
            {"id": 46, "upstream": "comfyui", "resources": ["gpu0"], "age_s": 20,
             "completion": "tracked", "phase": "tracking", "tracking_id": "a1"},
        ]);
        assert_eq!(
            format_held_work(&work),
            vec!["  work 44 held 4500s: comfyui on gpu0, mem; completion unconfirmed. Stop or verify it at the upstream, then release it with release_work"]
        );
    }

    #[test]
    fn status_cli_reports_a_local_admin_channel_that_is_down() {
        let channel = |value: serde_json::Value| format_admin_channel(&value);
        assert_eq!(
            channel(json!(null)),
            None,
            "a daemon that predates the field"
        );
        assert_eq!(channel(json!({"state": "listening"})), None);
        assert_eq!(channel(json!({"state": "not_supported"})), None);
        assert!(channel(json!({"state": "unavailable"}))
            .unwrap()
            .starts_with("  local admin channel unavailable:"));
    }

    #[test]
    fn status_cli_mentions_logging_only_when_it_is_not_the_default() {
        let logging = |value: serde_json::Value| format_logging(&value);
        assert_eq!(logging(json!(null)), None);
        assert_eq!(
            logging(json!({"level": "info", "source": "config", "environment_override": false})),
            None
        );
        assert_eq!(
            logging(json!({"level": "warn", "source": "config", "environment_override": false}))
                .as_deref(),
            Some("  logging: warn")
        );
        assert_eq!(
            logging(
                json!({"level": "custom", "source": "environment", "environment_override": true})
            )
            .as_deref(),
            Some("  logging: RUST_LOG filter in force")
        );
        assert_eq!(
            logging(json!({
                "level": "debug", "source": "debug_window", "environment_override": false,
                "debug_until": "2026-10-07T05:00:00Z", "debug_remaining_s": 1741
            }))
            .as_deref(),
            Some("  logging: debug until 2026-10-07T05:00:00Z, 30 min left (debug window)")
        );
    }

    #[test]
    fn status_cli_never_prints_url_credentials_it_was_sent() {
        let upstream = json!({
            "id": "ollama",
            "listen": "127.0.0.1:11435",
            "target": "http://demo:fake-secret@127.0.0.1:11434/?key=fake-secret",
            "vacancy": true
        });

        let rendered = format_upstream_status(&upstream);

        assert!(!rendered.contains("fake-secret"), "{rendered}");
        assert!(
            rendered.contains("http://***@127.0.0.1:11434/?***"),
            "{rendered}"
        );
    }

    #[test]
    fn status_cli_never_prints_a_missing_reading_as_zero() {
        let gpu = json!({
            "id": "gpu0", "kind": "nvidia-gpu", "label": "Example GPU", "enabled": true,
            "free_mib": 4096, "total_mib": 8192, "utilization_pct": null
        });
        let line = format_ready_resource(&gpu);
        assert!(line.contains("4096 MiB free /     8192 MiB"), "{line}");
        assert!(line.contains("util unknown"), "{line}");

        let partial = json!({
            "id": "mem", "kind": "system-memory", "label": "system memory", "enabled": true,
            "free_mib": null, "total_mib": null, "utilization_pct": 0
        });
        let line = format_ready_resource(&partial);
        assert!(line.contains("unknown MiB free /  unknown MiB"), "{line}");
        assert!(line.contains("util      0%"), "{line}");
        assert!(!line.contains(" 0 MiB"), "{line}");

        let cpu = json!({
            "id": "cpu", "kind": "system-cpu", "label": "8 logical cpus", "enabled": false,
            "free_mib": null, "total_mib": null, "utilization_pct": 12
        });
        let line = format_ready_resource(&cpu);
        assert!(!line.contains("MiB"), "{line}");
        assert!(line.contains("util     12%"), "{line}");
        assert!(line.ends_with("disabled"), "{line}");
    }

    /// Runs `run_status_through_environment_proxy` in a child process,
    /// because proxy environment variables are process-wide.
    #[test]
    fn status_ignores_proxy_environment_variables() {
        let status = std::process::Command::new(std::env::current_exe().unwrap())
            .args([
                "--exact",
                "task_tests::run_status_through_environment_proxy",
                "--ignored",
                "--nocapture",
            ])
            .env("VACANCYD_TEST_CHILD", "1")
            .status()
            .unwrap();
        assert!(status.success());
    }

    /// Answers every connection with `reply` and counts the connections.
    async fn counting_server(
        reply: &'static str,
    ) -> (
        std::net::SocketAddr,
        std::sync::Arc<std::sync::atomic::AtomicUsize>,
    ) {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let connections = std::sync::Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let counted = connections.clone();
        tokio::spawn(async move {
            loop {
                let (mut stream, _) = listener.accept().await.unwrap();
                counted.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
                tokio::spawn(async move {
                    let mut head = Vec::new();
                    let mut byte = [0u8; 1];
                    while !head.ends_with(b"\r\n\r\n") {
                        if stream.read_exact(&mut byte).await.is_err() {
                            return;
                        }
                        head.push(byte[0]);
                    }
                    let _ = stream.write_all(reply.as_bytes()).await;
                });
            }
        });
        (address, connections)
    }

    #[tokio::test]
    #[ignore = "run in a child process by status_ignores_proxy_environment_variables"]
    async fn run_status_through_environment_proxy() {
        assert_eq!(std::env::var("VACANCYD_TEST_CHILD").as_deref(), Ok("1"));
        let (system_proxy, proxied) = counting_server(
            "HTTP/1.1 418 I'm a teapot\r\nContent-Length: 0\r\nConnection: close\r\n\r\n",
        )
        .await;
        let (daemon, direct) = counting_server(
            "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: 2\r\nConnection: close\r\n\r\n{}",
        )
        .await;
        for name in ["HTTP_PROXY", "http_proxy", "ALL_PROXY", "all_proxy"] {
            std::env::set_var(name, format!("http://{system_proxy}"));
        }
        for name in ["NO_PROXY", "no_proxy"] {
            std::env::remove_var(name);
        }
        let body = fetch_status(&daemon.to_string()).await.unwrap();
        assert_eq!(body, json!({}));
        assert_eq!(direct.load(std::sync::atomic::Ordering::SeqCst), 1);
        assert_eq!(proxied.load(std::sync::atomic::Ordering::SeqCst), 0);
    }

    #[tokio::test]
    async fn status_does_not_follow_a_redirect() {
        let (target, followed) =
            counting_server("HTTP/1.1 200 OK\r\nContent-Length: 2\r\nConnection: close\r\n\r\n{}")
                .await;
        let redirect: &'static str = Box::leak(
            format!(
                "HTTP/1.1 302 Found\r\nLocation: http://{target}/status\r\nContent-Length: 0\r\nConnection: close\r\n\r\n"
            )
            .into_boxed_str(),
        );
        let (daemon, _) = counting_server(redirect).await;
        assert!(fetch_status(&daemon.to_string()).await.is_err());
        assert_eq!(followed.load(std::sync::atomic::Ordering::SeqCst), 0);
    }

    #[test]
    fn only_the_daemon_logs_to_standard_output() {
        let parse = |args: &[&str]| {
            Cli::try_parse_from(std::iter::once("vacancyd").chain(args.iter().copied()))
                .unwrap()
                .command
        };
        assert!(logs_to_stdout(&parse(&["run"])));
        for args in [
            &["diagnostics", "export"][..],
            &["status"],
            &["check-config"],
            &["gen-token"],
        ] {
            assert!(!logs_to_stdout(&parse(args)), "{args:?}");
        }
    }

    #[tokio::test]
    async fn diagnostics_export_writes_the_document_to_the_named_file() {
        let (daemon, connections) = counting_server(
            "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: 20\r\nConnection: close\r\n\r\n{\"schema_version\":1}",
        )
        .await;
        let out = std::env::temp_dir().join(format!(
            "vacancyd-diagnostics-cli-test-{}.json",
            std::process::id()
        ));
        export_diagnostics(&daemon.to_string(), Some(&out))
            .await
            .unwrap();
        let written = std::fs::read_to_string(&out).unwrap();
        std::fs::remove_file(&out).unwrap();
        assert_eq!(written, "{\"schema_version\":1}\n");
        assert_eq!(connections.load(std::sync::atomic::Ordering::SeqCst), 1);

        let (refusing, _) = counting_server(
            "HTTP/1.1 403 Forbidden\r\nContent-Length: 2\r\nConnection: close\r\n\r\n{}",
        )
        .await;
        let error = export_diagnostics(&refusing.to_string(), Some(&out))
            .await
            .unwrap_err();
        assert!(error.to_string().contains("refused"), "{error}");
        assert!(!out.exists());
    }

    #[test]
    fn health_probe_uses_loopback_for_a_wildcard_bind() {
        for (listen, probe) in [
            ("0.0.0.0:9811", "127.0.0.1:9811"),
            ("[::]:9811", "[::1]:9811"),
            ("127.0.0.1:9811", "127.0.0.1:9811"),
            ("192.0.2.10:9811", "192.0.2.10:9811"),
            ("[2001:db8::1]:9811", "[2001:db8::1]:9811"),
        ] {
            assert_eq!(health_probe_addr(listen).unwrap().to_string(), probe);
        }
        assert!(health_probe_addr("localhost:9811").is_err());
    }

    #[tokio::test]
    async fn healthcheck_accepts_only_an_ok_healthz_answer() {
        let (healthy, _) =
            counting_server("HTTP/1.1 200 OK\r\nContent-Length: 2\r\nConnection: close\r\n\r\nok")
                .await;
        healthcheck(healthy).await.unwrap();

        let (failing, _) = counting_server(
            "HTTP/1.1 503 Service Unavailable\r\nContent-Length: 2\r\nConnection: close\r\n\r\nok",
        )
        .await;
        assert!(healthcheck(failing).await.is_err());

        let (other_body, _) = counting_server(
            "HTTP/1.1 200 OK\r\nContent-Length: 4\r\nConnection: close\r\n\r\nnope",
        )
        .await;
        assert!(healthcheck(other_body).await.is_err());

        let (target, followed) =
            counting_server("HTTP/1.1 200 OK\r\nContent-Length: 2\r\nConnection: close\r\n\r\nok")
                .await;
        let redirect: &'static str = Box::leak(
            format!(
                "HTTP/1.1 302 Found\r\nLocation: http://{target}/healthz\r\nContent-Length: 0\r\nConnection: close\r\n\r\n"
            )
            .into_boxed_str(),
        );
        let (redirecting, _) = counting_server(redirect).await;
        assert!(healthcheck(redirecting).await.is_err());
        assert_eq!(followed.load(std::sync::atomic::Ordering::SeqCst), 0);
    }

    #[tokio::test]
    async fn healthcheck_fails_when_nothing_listens() {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let closed = listener.local_addr().unwrap();
        drop(listener);
        let error = healthcheck(closed).await.unwrap_err();
        assert!(format!("{error:#}").contains("no answer"));
    }

    #[tokio::test]
    async fn an_essential_task_error_is_returned_to_main() {
        let mut tasks = tokio::task::JoinSet::new();
        tasks.spawn(async { Err(anyhow!("listener bind failed")) });

        let error = essential_failure(&mut tasks).await;

        assert!(error.to_string().contains("listener bind failed"));
    }

    #[tokio::test]
    async fn an_essential_task_panic_is_returned_to_main() {
        let mut tasks: tokio::task::JoinSet<Result<()>> = tokio::task::JoinSet::new();
        tasks.spawn(async { panic!("collector panic") });

        let error = essential_failure(&mut tasks).await;

        assert!(error.to_string().contains("essential task failed"));
    }
}
