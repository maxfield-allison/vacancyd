//! Configuration loading and validation.
//!
//! vacancyd is configured by a single TOML file. See `vacancyd.example.toml`
//! at the repository root for a documented example.

use std::collections::{BTreeMap, BTreeSet};
use std::net::{IpAddr, SocketAddr};
use std::path::{Path, PathBuf};

use anyhow::{bail, Context, Result};
use serde::Deserialize;

#[derive(Debug, Clone, PartialEq, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Config {
    pub server: ServerConfig,
    #[serde(default)]
    pub poll: PollConfig,
    #[serde(default)]
    pub owner: OwnerConfig,
    #[serde(default)]
    pub auth: crate::auth::AuthConfig,
    #[serde(default, rename = "resource")]
    pub resources: Vec<ResourceConfig>,
    #[serde(default, rename = "upstream")]
    pub upstreams: Vec<UpstreamConfig>,
    #[serde(default)]
    pub log: LogConfig,
}

/// `[log]`: how much the daemon writes to its log.
#[derive(Debug, Clone, Default, PartialEq, Deserialize)]
#[serde(deny_unknown_fields, default)]
pub struct LogConfig {
    /// The level vacancyd's own events are logged at. `debug` and `trace`
    /// apply to vacancyd only; the libraries it uses stay at `info`, so the
    /// extra detail is vacancyd's reasoning rather than HTTP internals.
    /// `RUST_LOG`, when set, replaces this setting entirely.
    pub level: LogLevel,
}

/// `log.level`.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, PartialOrd, Ord, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum LogLevel {
    Error,
    Warn,
    #[default]
    Info,
    Debug,
    Trace,
}

impl LogLevel {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Error => "error",
            Self::Warn => "warn",
            Self::Info => "info",
            Self::Debug => "debug",
            Self::Trace => "trace",
        }
    }
}

#[derive(Debug, Clone, PartialEq, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ServerConfig {
    /// IP socket for the status / metrics / admin API, e.g. "0.0.0.0:9811".
    pub listen: String,
    /// Durable admission recovery directory. Empty selects
    /// `$STATE_DIRECTORY/admission` when set, otherwise a sibling of config.
    #[serde(default)]
    pub admission_state_dir: String,
    /// Connection limits applied to the API and to every proxy listener.
    #[serde(default)]
    pub limits: ListenerLimits,
    /// Linux: the Unix socket on which local administration is decided by
    /// the caller's account. Empty selects
    /// [`vacancyd::local_admin::DEFAULT_SOCKET_PATH`]. Ignored elsewhere.
    #[serde(default)]
    pub admin_socket: String,
    /// Linux: the group whose members are administrators, beside root.
    /// Empty makes root the only local administrator. Ignored elsewhere.
    #[serde(default = "default_admin_group")]
    pub admin_group: String,
}

fn default_admin_group() -> String {
    vacancyd::local_admin::DEFAULT_ADMIN_GROUP.to_owned()
}

impl ServerConfig {
    /// The admin socket path this configuration selects.
    #[cfg_attr(not(target_os = "linux"), allow(dead_code))]
    pub fn admin_socket_path(&self) -> PathBuf {
        if self.admin_socket.is_empty() {
            PathBuf::from(vacancyd::local_admin::DEFAULT_SOCKET_PATH)
        } else {
            PathBuf::from(&self.admin_socket)
        }
    }

    fn validate_local_admin(&self) -> Result<()> {
        if !self.admin_socket.is_empty() {
            // A client connects with the full path, which must fit a Unix
            // socket address (108 bytes including the terminating NUL).
            // Checked on the text: `Path::components` silently drops `.`
            // and repeated separators.
            let segments: Vec<&str> = self
                .admin_socket
                .strip_prefix('/')
                .unwrap_or_default()
                .split('/')
                .collect();
            if !self.admin_socket.starts_with('/')
                || segments
                    .iter()
                    .any(|segment| segment.is_empty() || *segment == "." || *segment == "..")
                || segments.len() < 2
                || self.admin_socket.len() > 107
                || self.admin_socket.chars().any(char::is_control)
                || segments.last().is_none_or(|name| name.len() > 64)
            {
                bail!("server.admin_socket must be an absolute path of at most 107 bytes naming a file (at most 64 bytes) inside a directory, without '.' or '..' components");
            }
        }
        // A group name as the user and group databases accept it. Rejecting
        // separators and control characters keeps a typo from matching
        // something unintended in an NSS backend.
        if self.admin_group.len() > 256
            || self.admin_group.starts_with('-')
            || self.admin_group.chars().any(|character| {
                character.is_control()
                    || character.is_whitespace()
                    || matches!(character, ':' | ',' | '/' | '\\')
            })
        {
            bail!("server.admin_group must be a group name, or empty to make root the only local administrator");
        }
        Ok(())
    }
}

/// `[server.limits]`: how long, how much and how many connections a client
/// may hold on each listener.
///
/// The header and body timeouts cover waiting for a request and idle
/// connections. None of them applies while a response is streaming, so a
/// long generation is never cut off by these settings; only a client that
/// stops accepting the response is.
#[derive(Debug, Clone, PartialEq, Deserialize)]
#[serde(deny_unknown_fields, default)]
pub struct ListenerLimits {
    /// Seconds a client has to send complete request headers. The same
    /// timeout closes a keep-alive connection that sits idle between
    /// requests.
    pub header_read_timeout_s: u64,
    /// Seconds a client has, once its headers are complete, to send the
    /// whole request body. A slower body is refused with `408`.
    pub body_read_timeout_s: u64,
    /// Seconds a request body may make no progress before it is refused
    /// with `408`, however much of `body_read_timeout_s` remains.
    pub body_idle_timeout_s: u64,
    /// Seconds a client may accept no response bytes, with data waiting to
    /// be sent, before its connection is closed. A client that stops
    /// reading would otherwise hold its connection, and any borrowed work
    /// streaming to it, indefinitely.
    pub write_stall_timeout_s: u64,
    /// Open connections per listener. At the limit, new connections wait
    /// until one closes. WebSocket tunnels on a proxy count against a
    /// separate limit of the same size.
    pub max_connections: usize,
    /// Largest request body a proxy accepts, in MiB. Larger bodies are
    /// refused with `413`. A connection sends one request at a time, so this
    /// is also the most one connection can make a proxy hold.
    pub max_request_body_mib: usize,
    /// Request-body memory each proxy may hold at once, in MiB, across all
    /// its connections. A body is held in memory from its first byte until
    /// it has been forwarded; one that would take a proxy past this limit
    /// is refused with `503` and `Retry-After`. Must be at least
    /// `max_request_body_mib`. Unset, it is 256 MiB, or
    /// `max_request_body_mib` where that is larger, so a configuration that
    /// raised the request limit without knowing this setting stays valid.
    pub max_buffered_body_mib: Option<usize>,
    /// Seconds a proxied WebSocket tunnel may pass no data in either
    /// direction before it is closed.
    pub tunnel_idle_timeout_s: u64,
}

/// The per-proxy request-body budget when `max_buffered_body_mib` is unset
/// and `max_request_body_mib` is no larger.
pub const DEFAULT_BUFFERED_BODY_MIB: usize = 256;

impl Default for ListenerLimits {
    fn default() -> Self {
        Self {
            header_read_timeout_s: 30,
            body_read_timeout_s: 120,
            body_idle_timeout_s: 15,
            write_stall_timeout_s: 60,
            max_connections: 128,
            max_request_body_mib: 64,
            max_buffered_body_mib: None,
            tunnel_idle_timeout_s: 3600,
        }
    }
}

impl ListenerLimits {
    pub fn max_request_body_bytes(&self) -> usize {
        self.max_request_body_mib.saturating_mul(1024 * 1024)
    }

    /// The request-body budget of one proxy in MiB: the configured value,
    /// or the default of [`DEFAULT_BUFFERED_BODY_MIB`] raised to
    /// `max_request_body_mib`.
    pub fn buffered_body_mib(&self) -> usize {
        self.max_buffered_body_mib
            .unwrap_or_else(|| DEFAULT_BUFFERED_BODY_MIB.max(self.max_request_body_mib))
    }

    /// The request-body budget of one proxy in KiB, the unit it is
    /// reserved in.
    pub fn buffered_body_kib(&self) -> usize {
        self.buffered_body_mib().saturating_mul(1024)
    }

    fn validate(&self) -> Result<()> {
        if !(1..=300).contains(&self.header_read_timeout_s) {
            bail!("server.limits.header_read_timeout_s must be from 1 to 300");
        }
        if !(1..=3600).contains(&self.body_read_timeout_s) {
            bail!("server.limits.body_read_timeout_s must be from 1 to 3600");
        }
        if !(1..=300).contains(&self.body_idle_timeout_s) {
            bail!("server.limits.body_idle_timeout_s must be from 1 to 300");
        }
        if self.body_idle_timeout_s > self.body_read_timeout_s {
            bail!(
                "server.limits.body_idle_timeout_s ({}) must not exceed body_read_timeout_s ({})",
                self.body_idle_timeout_s,
                self.body_read_timeout_s
            );
        }
        if !(1..=3600).contains(&self.write_stall_timeout_s) {
            bail!("server.limits.write_stall_timeout_s must be from 1 to 3600");
        }
        if !(1..=4096).contains(&self.max_connections) {
            bail!("server.limits.max_connections must be from 1 to 4096");
        }
        if !(1..=1024).contains(&self.max_request_body_mib) {
            bail!("server.limits.max_request_body_mib must be from 1 to 1024");
        }
        if let Some(buffered) = self.max_buffered_body_mib {
            if !(1..=65536).contains(&buffered) {
                bail!("server.limits.max_buffered_body_mib must be from 1 to 65536");
            }
            if buffered < self.max_request_body_mib {
                bail!(
                    "server.limits.max_buffered_body_mib ({buffered}) must be at least \
                     max_request_body_mib ({}), or a body of the permitted size could never \
                     be accepted",
                    self.max_request_body_mib
                );
            }
        }
        if self.tunnel_idle_timeout_s == 0 {
            bail!("server.limits.tunnel_idle_timeout_s must be greater than zero");
        }
        Ok(())
    }
}

#[derive(Debug, Clone, PartialEq, Deserialize)]
#[serde(deny_unknown_fields, default)]
pub struct PollConfig {
    /// How often collectors sample the hardware, in milliseconds.
    pub interval_ms: u64,
}

impl Default for PollConfig {
    fn default() -> Self {
        Self { interval_ms: 3000 }
    }
}

/// Detection of "the owner is using this machine" — the veto signal.
///
/// The primary signal is **session state**: a locked, disconnected or
/// logged-out machine is lendable, an unlocked in-use desktop is not. That
/// is readable from a session-0 service, so the daemon can be always-on.
/// When it happens to run inside the user's session, the foreground process
/// list below refines the signal further.
///
/// The veto engages on the first matching sample and releases only after
/// `release_after_s` of consecutive non-matching samples, so unlocking to
/// check something does not flap lending off and on.
#[derive(Debug, Clone, PartialEq, Deserialize)]
#[serde(deny_unknown_fields, default)]
pub struct OwnerConfig {
    /// Process names (without `.exe`, case-insensitive) that count as owner
    /// activity when observable. Only consulted in a user-session run.
    pub processes: Vec<String>,
    /// Seconds the veto persists after the last matching sample, whatever
    /// ended it: an owner process leaving, a lock, a sign-out or idle time.
    pub release_after_s: u64,
    /// Treat an unlocked, logged-in desktop as owner presence even when no
    /// listed process is in the foreground. This is the safe default: if
    /// the owner is sitting there, the machine is theirs.
    pub unlocked_session_is_presence: bool,
    /// Seconds of no keyboard/mouse input after which an *unlocked* session
    /// stops counting as presence — the owner walked away without locking.
    /// 0 disables it. Only ever acts on a real idle reading; an unknown idle
    /// time is never treated as idle.
    pub idle_lends_after_s: u64,
    /// Treat media playback as owner presence. A film takes no keyboard
    /// input for two hours, so without this `idle_lends_after_s` would hand
    /// the GPU away mid-scene. Detected from the display-wake request the
    /// player itself asserts, so no list of players needs maintaining.
    /// Requires elevation to read; when it cannot be read, idle lending is
    /// suspended rather than assumed safe.
    pub media_is_presence: bool,
    /// How often to re-read the media signal, in milliseconds. Separate from
    /// `poll.interval_ms` because the read costs a process spawn; the floor
    /// is 1000.
    pub media_poll_ms: u64,
    /// Holders to disregard, for the occasional application that asserts a
    /// display-wake request and never releases it. Matched case-insensitively
    /// as a substring.
    pub media_ignore: Vec<String>,
    /// Treat the sign-in screen as owner presence, so a PC waiting at
    /// sign-in after a reboot, a sign-out, or Switch user keeps refusing. On
    /// Linux this is a display-manager greeter in the foreground of its seat.
    /// Off by default: nobody signed in is the logged-out tier and lends once
    /// two consecutive session reads agree. A session state that cannot be
    /// read refuses either way.
    pub sign_in_screen_is_presence: bool,
    /// Where session presence comes from. `auto` (the default) uses the
    /// platform backend: WTS on Windows, systemd-logind on Linux. `headless`
    /// declares that nobody sits at this machine: session state, idle, media
    /// and the owner-process list are not consulted, and the host is treated
    /// as logged out. Use it only on a host without a local desktop, such as
    /// a Linux server or container without systemd-logind.
    pub presence: PresenceMode,
    /// Presence inferred from GPU work that vacancyd did not admit — the
    /// signal that covers every application rather than a list of them.
    pub gpu_pressure: GpuPressureConfig,
}

/// The longest `learn_half_life_h` accepted: thirty days. Longer, and the
/// threshold would answer to habits the machine left behind months ago.
pub const MAX_LEARN_HALF_LIFE_H: f64 = 24.0 * 30.0;

/// `[owner.gpu_pressure]` — per-resource refusal from attributed GPU load.
///
/// Unlike the explicit process policy, this is an inferred measurement. It can
/// refuse only the resource whose attributed load crossed its threshold; it
/// never becomes a machine-wide presence verdict.
///
/// `processes` remains useful and is not deprecated: a named process is a
/// faster and more certain signal than inferring from load, so it stays as the
/// fast path for applications the operator already knows about.
#[derive(Debug, Clone, PartialEq, Deserialize)]
#[serde(deny_unknown_fields, default)]
pub struct GpuPressureConfig {
    pub enabled: bool,
    /// Calibrated by default; fixed is an override. The retired `adaptive`
    /// still parses and runs as calibrated, with a warning at startup.
    pub mode: crate::plugin::pressure::PressureMode,
    /// The highest engage threshold calibration may learn, in percent of
    /// one device.
    pub cap_pct: f64,
    /// Fixed engage threshold, used only with mode = "fixed".
    pub threshold_pct: f64,
    /// Added to the learned quantile to get the target threshold.
    pub margin_pct: f64,
    /// The engage threshold is never allowed below this.
    pub floor_pct: f64,
    /// Seconds of sustained load before presence engages. Stops a thumbnail
    /// render from evicting a model that takes half a minute to reload.
    pub sustain_s: u64,
    /// Quantile of the learned histogram the target threshold is built on.
    /// High on purpose: the threshold must clear the desktop's ordinary
    /// busy hours, and the cap keeps heavy work from raising it further.
    pub learn_quantile: f64,
    /// Hours of observed time after which a reading counts half as much.
    pub learn_half_life_h: f64,
    /// Percentage points per observed hour the threshold may rise.
    pub raise_pct_per_h: f64,
    /// Samples before a profile's first threshold is set.
    pub warmup_samples: usize,
    /// How often to sample. Separate from `poll.interval_ms` because the read
    /// costs a process spawn over roughly a thousand counter instances.
    pub poll_ms: u64,
    /// Processes whose GPU use never counts as pressure — a wallpaper engine,
    /// a monitoring overlay, a second inference server run outside vacancyd.
    /// Matched case-insensitively as a substring, like `media_ignore`.
    pub ignore: Vec<String>,
    /// Where learned calibration is kept across restarts. Empty means
    /// `vacancyd-pressure.json` beside the config file. The file is a cache,
    /// not state of record: losing it costs a warm start, nothing more.
    pub state_file: String,
    /// Deprecated and ignored: the frozen-baseline estimator's window. Still
    /// accepted so an older configuration loads; a warning names it.
    pub baseline_window_s: Option<u64>,
    /// Deprecated and ignored: the frozen-baseline estimator's quantile.
    /// Every earlier console save wrote it, so its value must not reach the
    /// learned threshold.
    pub baseline_quantile: Option<f64>,
}

impl Default for GpuPressureConfig {
    fn default() -> Self {
        let d = crate::plugin::pressure::PressureConfig::default();
        Self {
            enabled: true,
            mode: d.mode,
            cap_pct: d.cap_pct,
            threshold_pct: d.threshold_pct,
            margin_pct: d.margin_pct,
            floor_pct: d.floor_pct,
            sustain_s: d.sustain_s,
            learn_quantile: d.learn_quantile,
            learn_half_life_h: d.learn_half_life_h,
            raise_pct_per_h: d.raise_pct_per_h,
            warmup_samples: d.warmup_samples,
            poll_ms: 5_000,
            ignore: Vec::new(),
            state_file: String::new(),
            baseline_window_s: None,
            baseline_quantile: None,
        }
    }
}

impl GpuPressureConfig {
    pub fn tracker_config(&self, release_after_s: u64) -> crate::plugin::pressure::PressureConfig {
        crate::plugin::pressure::PressureConfig {
            mode: self.mode.effective(),
            cap_pct: self.effective_cap_pct(),
            threshold_pct: self.threshold_pct,
            margin_pct: self.margin_pct,
            floor_pct: self.floor_pct,
            sustain_s: self.sustain_s,
            release_after_s,
            learn_quantile: self.learn_quantile,
            learn_half_life_h: self.learn_half_life_h,
            raise_pct_per_h: self.raise_pct_per_h,
            warmup_samples: self.warmup_samples,
            // The gap between valid samples must tolerate exactly one missed
            // sample. A sample lands every poll_ms plus the attribution
            // read's own duration, up to about 5 s at the default poll, so
            // one miss spans nearly two of those intervals.
            max_gap_ms: self.poll_ms.saturating_mul(3).max(20_000),
        }
    }

    /// The cap a tracker runs with. A configured cap above
    /// [`crate::plugin::pressure::MAX_CAP_PCT`] runs at that bound, and one
    /// below the floor runs at the floor, keeping the earlier meaning that the
    /// threshold is never below the floor. Both load with a warning rather
    /// than failing, because earlier versions accepted them.
    pub fn effective_cap_pct(&self) -> f64 {
        self.cap_pct
            .min(crate::plugin::pressure::MAX_CAP_PCT)
            .max(self.floor_pct)
    }

    /// Resolve [`Self::state_file`] against the directory holding the config.
    pub fn state_path(&self, config_path: &Path) -> PathBuf {
        if !self.state_file.is_empty() {
            let configured = PathBuf::from(&self.state_file);
            if configured.is_absolute() {
                return configured;
            }
            return config_path
                .parent()
                .unwrap_or_else(|| Path::new("."))
                .join(configured);
        }
        config_path
            .parent()
            .unwrap_or_else(|| Path::new("."))
            .join("vacancyd-pressure.json")
    }
}

impl Default for OwnerConfig {
    fn default() -> Self {
        Self {
            processes: Vec::new(),
            release_after_s: 120,
            unlocked_session_is_presence: true,
            idle_lends_after_s: 0,
            media_is_presence: true,
            media_poll_ms: 5_000,
            media_ignore: Vec::new(),
            sign_in_screen_is_presence: false,
            presence: PresenceMode::Auto,
            gpu_pressure: GpuPressureConfig::default(),
        }
    }
}

/// `owner.presence`.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum PresenceMode {
    #[default]
    Auto,
    Headless,
}

impl OwnerConfig {
    /// The subset a presence backend needs.
    pub fn presence_options(&self) -> crate::plugin::presence::PresenceOptions {
        crate::plugin::presence::PresenceOptions {
            media_is_presence: self.media_is_presence,
            media_poll_ms: self.media_poll_ms,
            media_ignore: self.media_ignore.clone(),
            sign_in_screen_is_presence: self.sign_in_screen_is_presence,
            headless: self.presence == PresenceMode::Headless,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ResourceConfig {
    /// Stable identifier used in gates, metrics labels, and the admin API.
    pub id: String,
    /// Resource kind, resolved against the probe registry (e.g.
    /// "nvidia-gpu", "system-memory", "system-cpu").
    pub kind: String,
    /// Kind-specific selector. For `nvidia-gpu`: a GPU UUID
    /// ("GPU-xxxx...") or an nvidia-smi index ("0"). Unused for system
    /// resources.
    #[serde(default)]
    pub selector: Option<String>,
    /// Stable PCI address used to join a vendor GPU to the operating system's
    /// per-process accounting identity, e.g. "0000:0e:00.0". Normally learned
    /// from the vendor probe; set this to make the identity explicit.
    #[serde(default)]
    pub adapter: Option<crate::probes::gpu_id::PciAddress>,
    /// Whether this resource participates in lending at startup. Can be
    /// flipped at runtime through the admin API.
    #[serde(default = "default_true")]
    pub enabled: bool,
    /// For `system-memory`: bytes that must remain free for the owner. A
    /// job's memory requirement on this resource is charged on top of it,
    /// so a job is admitted only while free memory covers both.
    /// For `system-cpu`: load percentage (0-100) above which the resource
    /// is not lendable. Ignored for GPUs (upstream thresholds govern).
    #[serde(default)]
    pub reserve: Option<u64>,
}

impl ResourceConfig {
    /// Bytes admission must leave free for the owner on top of any job's
    /// requirement: the `reserve` of a memory resource. Other kinds keep
    /// nothing back in bytes (a CPU reserve is a load ceiling).
    pub fn owner_reserve_bytes(&self) -> u64 {
        if self.kind == "system-memory" {
            self.reserve.unwrap_or(0)
        } else {
            0
        }
    }
}

#[derive(Debug, Clone, PartialEq, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct UpstreamConfig {
    /// Stable identifier used in metrics labels and logs.
    pub id: String,
    /// IP socket this upstream's gated proxy listens on, e.g. "0.0.0.0:11435".
    pub listen: String,
    /// HTTP base URL of the real service, e.g. "http://127.0.0.1:11434".
    /// It may not carry credentials; an upstream that authenticates its
    /// callers is given [`Self::credential`] instead. Upstreams whose targets
    /// reach the same server must borrow the same resources, because a
    /// reclaim may affect work or resident models across that server.
    pub target: UpstreamTarget,
    /// Upstream dialect, resolved against the adapter registry (e.g.
    /// "ollama", "openai", "comfyui").
    pub kind: String,
    /// The resource this upstream's per-model thresholds apply to
    /// (must match a `resource.id`). A scalar requirement and
    /// `default_min_free_mib` apply to it when it is a GPU or system memory,
    /// and to every NVIDIA GPU in `also_requires`.
    pub resource: String,
    /// Additional resources that must also be lendable before work is
    /// admitted — e.g. require RAM headroom and a quiet CPU alongside the
    /// GPU. Each must match a `resource.id`.
    #[serde(default)]
    pub also_requires: Vec<String>,
    /// When the owner returns, actively release the hardware (adapters that
    /// support it) instead of letting the OS page it out.
    #[serde(default)]
    pub yield_on_veto: bool,
    /// What the owner veto applies to. `host` (the default) refuses and
    /// reclaims this upstream whenever the owner is using the machine.
    /// `resource` exempts it from the owner veto and session evidence: it
    /// lends whenever its own resources are lendable, and is reclaimed only
    /// by GPU pressure on its GPUs. Memory and CPU dependencies are checked
    /// at admission and never reclaim running work. Use it for an always-on
    /// lane on its own server, on a card the owner's workload does not use,
    /// such as a helper model on a display GPU while a game runs on another.
    #[serde(default)]
    pub owner_scope: OwnerScope,
    /// Existing work on owner return: cancel now, drain for a bounded time,
    /// or finish without a time limit. Also applies to affected-resource GPU
    /// pressure reclaim. New work is refused in every mode.
    #[serde(default)]
    pub on_owner_return: ReclaimMode,
    #[serde(default = "default_drain_timeout_s")]
    pub drain_timeout_s: u32,
    /// Maximum tracking time; expiry retains the reservation for operator recovery.
    #[serde(default = "default_completion_timeout_s")]
    pub completion_timeout_s: u32,
    /// Pass the client's `Authorization` header through to the real service.
    /// Off by default: the header carries *vacancyd's* credential, and
    /// forwarding it hands that secret to an upstream with no need for it.
    /// Turn on only when the upstream authenticates callers itself.
    #[serde(default)]
    pub forward_authorization: bool,
    /// The credential vacancyd itself presents to the real service, for an
    /// upstream that authenticates its callers. Borrowers never see it.
    #[serde(default)]
    pub credential: Option<crate::upstream_credential::UpstreamCredential>,
    /// Whether borrowers may manage the upstream through the gate: change
    /// its model store (Ollama pull, create, copy, push, delete and blob
    /// uploads), change its state or files (ComfyUI uploads, settings, user
    /// data, `/free`, `/interrupt`, queue and history deletion), read the
    /// owner's data on it (llama.cpp slots, metrics and properties, Ollama's
    /// running models, ComfyUI history lists, user data, settings and logs),
    /// or make any read the adapter does not list for borrowers. `deny` (the
    /// default) refuses these with 403 before forwarding.
    #[serde(default)]
    pub management: ManagementPolicy,
    /// Free MiB required for a model not listed below, and for dialects
    /// that do not report a model at all, on every resource a scalar
    /// requirement applies to (see [`UpstreamConfig::scalar_applies_to`]).
    #[serde(default)]
    pub default_min_free_mib: u64,
    /// Per-model free-MiB requirements — measured footprint plus headroom,
    /// keyed by the model name as it appears in request bodies. A scalar
    /// applies to the primary resource when it is system memory and to every
    /// NVIDIA GPU dependency. A table declares an explicit
    /// requirement for each named dependency, for example
    /// `"split-model" = { gpu0 = 12000, gpu1 = 16000 }`. Either form can
    /// also set `resident_min_free_mib`, the smaller amount a request needs
    /// while the model is already loaded; the scalar form then becomes
    /// `{ min_free_mib = 8200, resident_min_free_mib = 300 }`.
    #[serde(default)]
    pub models: BTreeMap<String, ModelRequirement>,
}

/// An upstream's base URL as configured.
///
/// It formats, in `Display` and `Debug` alike, only through
/// [`vacancyd::status::RedactedUrl`], so neither a log line nor a `{:?}` of
/// the configuration can carry URL credentials or a query string even if
/// validation is ever bypassed. Code that sends a request reads the URL with
/// [`Self::as_str`].
#[derive(Clone, Deserialize, PartialEq, Eq)]
#[serde(transparent)]
pub struct UpstreamTarget(String);

impl UpstreamTarget {
    /// The URL to send requests to. Never format this for a reader; format
    /// the `UpstreamTarget` itself.
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl std::fmt::Display for UpstreamTarget {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        vacancyd::status::RedactedUrl(&self.0).fmt(f)
    }
}

impl std::fmt::Debug for UpstreamTarget {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        std::fmt::Debug::fmt(&vacancyd::status::RedactedUrl(&self.0), f)
    }
}

impl From<&str> for UpstreamTarget {
    fn from(url: &str) -> Self {
        Self(url.to_owned())
    }
}

impl From<String> for UpstreamTarget {
    fn from(url: String) -> Self {
        Self(url)
    }
}

/// Table keys inside a model requirement that are not resource ids.
pub const MODEL_REQUIREMENT_KEYS: [&str; 2] = ["min_free_mib", "resident_min_free_mib"];

/// What one model needs free before a request is admitted.
#[derive(Debug, Clone, Deserialize, PartialEq, Eq)]
#[serde(try_from = "RawModelRequirement")]
pub struct ModelRequirement {
    /// Free MiB a request needs when it may have to load the model.
    pub load: LoadRequirement,
    /// Free MiB a request needs on each budgeted dependency once admission
    /// has proved the model resident and reusable as loaded. Defaults to 0,
    /// and never exceeds the load requirement for the same dependency.
    pub resident_min_free_mib: u64,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum LoadRequirement {
    Scalar(u64),
    PerResource(BTreeMap<String, u64>),
}

/// The TOML shapes a model requirement may take. Values are read signed so
/// a negative number is reported as such instead of as an unmatched shape.
#[derive(Deserialize)]
#[serde(untagged)]
enum RawModelRequirement {
    Scalar(i64),
    Table(BTreeMap<String, i64>),
}

fn non_negative_mib(key: &str, value: i64) -> Result<u64, String> {
    u64::try_from(value).map_err(|_| format!("{key} must not be negative (found {value})"))
}

impl TryFrom<RawModelRequirement> for ModelRequirement {
    type Error = String;

    fn try_from(raw: RawModelRequirement) -> Result<Self, Self::Error> {
        match raw {
            RawModelRequirement::Scalar(mib) => Ok(Self {
                load: LoadRequirement::Scalar(non_negative_mib("model requirement", mib)?),
                resident_min_free_mib: 0,
            }),
            RawModelRequirement::Table(mut table) => {
                let resident_min_free_mib = table
                    .remove("resident_min_free_mib")
                    .map(|mib| non_negative_mib("resident_min_free_mib", mib))
                    .transpose()?
                    .unwrap_or(0);
                let load = match table.remove("min_free_mib") {
                    Some(mib) if table.is_empty() => {
                        LoadRequirement::Scalar(non_negative_mib("min_free_mib", mib)?)
                    }
                    Some(_) => {
                        return Err(
                            "min_free_mib applies to every NVIDIA GPU and cannot be combined with per-resource requirements"
                                .into(),
                        )
                    }
                    None => LoadRequirement::PerResource(
                        table
                            .into_iter()
                            .map(|(id, mib)| non_negative_mib(&id, mib).map(|mib| (id, mib)))
                            .collect::<Result<_, _>>()?,
                    ),
                };
                Ok(Self {
                    load,
                    resident_min_free_mib,
                })
            }
        }
    }
}

impl ModelRequirement {
    /// The requirement for a request that may load the model. `scalar_applies`
    /// says whether a scalar requirement budgets this resource; see
    /// [`UpstreamConfig::scalar_applies_to`].
    pub fn for_resource(&self, resource: &str, scalar_applies: bool) -> Option<u64> {
        match &self.load {
            LoadRequirement::Scalar(mib) => scalar_applies.then_some(*mib),
            LoadRequirement::PerResource(resources) => resources.get(resource).copied(),
        }
    }
}

impl UpstreamConfig {
    /// Whether a scalar model requirement and `default_min_free_mib` budget
    /// `resource`: every NVIDIA GPU dependency, and the primary resource
    /// when it is system memory, as on a CPU-only upstream. A CPU is never
    /// budgeted in MiB, and a memory resource named only in `also_requires`
    /// keeps its owner reserve but is budgeted only by a per-resource table.
    pub fn scalar_applies_to(&self, resource: &ResourceConfig) -> bool {
        resource.kind == "nvidia-gpu"
            || (resource.id == self.resource && resource.kind == "system-memory")
    }
}

/// What a borrower may do to an upstream beyond using it for inference.
#[derive(Debug, Clone, Copy, Default, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum ManagementPolicy {
    /// Refuse management and owner-data routes with 403.
    #[default]
    Deny,
    /// Forward them without admission, as for discovery routes.
    Allow,
}

#[derive(Debug, Clone, Copy, Default, Deserialize, serde::Serialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum OwnerScope {
    #[default]
    Host,
    Resource,
}

/// Upstreams the owner veto does not apply to (`owner_scope = "resource"`),
/// in configuration order.
pub fn veto_exempt_upstreams(config: &Config) -> impl Iterator<Item = &UpstreamConfig> {
    config
        .upstreams
        .iter()
        .filter(|upstream| upstream.owner_scope == OwnerScope::Resource)
}

impl Config {
    /// Dependencies of `upstream` that no pressure measurement watches.
    /// Pressure covers NVIDIA GPUs while `owner.gpu_pressure` is enabled;
    /// memory and CPU have only a capacity reserve, which refuses new work
    /// but never reclaims accepted work.
    pub fn unwatched_dependencies<'a>(&'a self, upstream: &'a UpstreamConfig) -> Vec<&'a str> {
        std::iter::once(&upstream.resource)
            .chain(upstream.also_requires.iter())
            .filter(|id| {
                !(self.owner.gpu_pressure.enabled
                    && self
                        .resources
                        .iter()
                        .any(|resource| &resource.id == *id && resource.kind == "nvidia-gpu"))
            })
            .map(String::as_str)
            .collect()
    }

    /// Say once, at startup, which upstreams keep lending while the owner is
    /// present, and warn about any of them whose dependencies no pressure
    /// measurement watches. Logs nothing when every upstream is host-scoped.
    pub fn log_owner_scopes(&self) {
        let exempt: Vec<&str> = veto_exempt_upstreams(self)
            .map(|upstream| upstream.id.as_str())
            .collect();
        if exempt.is_empty() {
            return;
        }
        tracing::info!(
            upstreams = exempt.join(", "),
            "owner veto exemptions: these upstreams have owner_scope = \"resource\" and keep lending while the owner is present, gated only by their own resources"
        );
        for upstream in veto_exempt_upstreams(self) {
            let unwatched = self.unwatched_dependencies(upstream);
            if !unwatched.is_empty() {
                tracing::warn!(
                    upstream = upstream.id,
                    resources = unwatched.join(", "),
                    "exempt from the owner veto, and no pressure measurement watches these resources: they refuse new work only on capacity, and nothing reclaims this upstream's accepted work while the owner is present"
                );
            }
        }
    }

    /// Warn about each retired or deprecated setting the file still names,
    /// and each pressure bound that runs at a different value than written.
    /// They load, so nothing else would say so.
    pub fn log_setting_warnings(&self) {
        let pressure = &self.owner.gpu_pressure;
        let max_cap = crate::plugin::pressure::MAX_CAP_PCT;
        if pressure.mode.effective() != crate::plugin::pressure::PressureMode::Fixed {
            if pressure.floor_pct > pressure.cap_pct {
                tracing::warn!(
                    setting = "owner.gpu_pressure.cap_pct",
                    cap_pct = pressure.cap_pct,
                    floor_pct = pressure.floor_pct,
                    effective_cap_pct = pressure.effective_cap_pct(),
                    "cap_pct is below floor_pct; the threshold is never below the floor, so the cap runs at the floor; set floor_pct <= cap_pct"
                );
            } else if pressure.cap_pct > max_cap {
                tracing::warn!(
                    setting = "owner.gpu_pressure.cap_pct",
                    cap_pct = pressure.cap_pct,
                    effective_cap_pct = pressure.effective_cap_pct(),
                    "cap_pct above {max_cap} runs at {max_cap}: a higher cap lets long work at full load raise the learned threshold past itself; set cap_pct <= {max_cap}"
                );
            }
        }
        if pressure.mode == crate::plugin::pressure::PressureMode::Adaptive {
            tracing::warn!(
                setting = "owner.gpu_pressure.mode",
                "mode = \"adaptive\" is retired and runs as \"calibrated\", whose learned threshold now follows the machine; set mode = \"calibrated\""
            );
        }
        for (setting, present) in [
            (
                "owner.gpu_pressure.baseline_quantile",
                pressure.baseline_quantile.is_some(),
            ),
            (
                "owner.gpu_pressure.baseline_window_s",
                pressure.baseline_window_s.is_some(),
            ),
        ] {
            if present {
                tracing::warn!(
                    setting,
                    "deprecated setting is ignored: calibration learns from a decaying histogram (see learn_quantile and learn_half_life_h); remove it, or save settings from the owner console, which removes it"
                );
            }
        }
    }
}

#[derive(Debug, Clone, Copy, Default, Deserialize, serde::Serialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum ReclaimMode {
    Cancel,
    #[default]
    Drain,
    Finish,
}

#[cfg(test)]
mod owner_scope_tests {
    use super::*;

    #[test]
    fn owner_scope_defaults_to_host_and_parses_resource() {
        let base = r#"
            id = "a"
            listen = "127.0.0.1:0"
            target = "http://127.0.0.1:1"
            kind = "ollama"
            resource = "gpu1"
        "#;
        let host: UpstreamConfig = toml::from_str(base).unwrap();
        assert_eq!(host.owner_scope, OwnerScope::Host);
        let scoped: UpstreamConfig = toml::from_str(&format!(
            "{base}
owner_scope = \"resource\"
"
        ))
        .unwrap();
        assert_eq!(scoped.owner_scope, OwnerScope::Resource);
        assert!(toml::from_str::<UpstreamConfig>(&format!(
            "{base}
owner_scope = \"gpu\"
"
        ))
        .is_err());
    }

    fn scoped_config(pressure: bool) -> Config {
        toml::from_str(&format!(
            r#"
            [server]
            listen = "127.0.0.1:9811"

            [owner.gpu_pressure]
            enabled = {pressure}

            [[resource]]
            id = "gpu0"
            kind = "nvidia-gpu"
            selector = "GPU-0"

            [[resource]]
            id = "gpu1"
            kind = "nvidia-gpu"
            selector = "GPU-1"

            [[resource]]
            id = "memory"
            kind = "system-memory"

            [[upstream]]
            id = "ollama"
            listen = "127.0.0.1:11435"
            target = "http://127.0.0.1:1"
            kind = "ollama"
            resource = "gpu0"

            [[upstream]]
            id = "helper"
            listen = "127.0.0.1:11437"
            target = "http://127.0.0.1:2"
            kind = "ollama"
            resource = "gpu1"
            owner_scope = "resource"

            [[upstream]]
            id = "tts"
            listen = "127.0.0.1:8859"
            target = "http://127.0.0.1:3"
            kind = "openai"
            resource = "gpu1"
            also_requires = ["memory"]
            owner_scope = "resource"
            "#
        ))
        .unwrap()
    }

    #[test]
    fn an_exempt_upstream_needs_a_server_no_host_scoped_upstream_uses() {
        let config = scoped_config(true);
        config.validate().unwrap();
        for (target, shared) in [
            ("http://127.0.0.1:1", true),
            ("http://localhost:1/", true),
            ("http://[::1]:1", true),
            ("http://127.0.0.1:1/v1", true),
            ("http://0.0.0.0:1", true),
            ("http://127.0.0.1:4", false),
            ("http://192.0.2.5:1", false),
        ] {
            let mut config = scoped_config(true);
            // "helper" is resource-scoped; "ollama" (host) serves port 1.
            config.upstreams[1].target = target.into();
            let result = config.validate();
            assert_eq!(result.is_err(), shared, "{target}: {result:?}");
            if let Err(error) = result {
                let message = error.to_string();
                // Different resources: the shared-server rule answers first.
                assert!(message.contains("'ollama' and 'helper'"), "{message}");
            }
        }
        // Two exempt upstreams may share a server, as may two host-scoped
        // ones, when they borrow the same resources (the shared-server rule).
        let mut config = scoped_config(true);
        config.upstreams[2].also_requires.clear();
        config.upstreams[2].target = config.upstreams[1].target.clone();
        config.validate().unwrap();
        let mut config = scoped_config(true);
        config.upstreams[1].owner_scope = OwnerScope::Host;
        config.upstreams[1].resource = "gpu0".into();
        config.upstreams[1].target = config.upstreams[0].target.clone();
        config.validate().unwrap();
        // With the same resources, the host-scoped pairing is still refused:
        // owner return would unload the exempt upstream's models.
        let mut config = scoped_config(true);
        config.upstreams[1].resource = "gpu0".into();
        config.upstreams[1].target = config.upstreams[0].target.clone();
        assert!(config
            .validate()
            .unwrap_err()
            .to_string()
            .contains("host-scoped upstream 'ollama'"));
    }

    #[test]
    fn exempt_upstreams_and_their_unwatched_dependencies() {
        let config = scoped_config(true);
        let exempt: Vec<_> = veto_exempt_upstreams(&config)
            .map(|upstream| upstream.id.as_str())
            .collect();
        assert_eq!(exempt, ["helper", "tts"]);
        assert!(config
            .unwatched_dependencies(&config.upstreams[1])
            .is_empty());
        assert_eq!(
            config.unwatched_dependencies(&config.upstreams[2]),
            ["memory"]
        );
        // Without GPU pressure nothing watches a GPU either.
        let config = scoped_config(false);
        assert_eq!(
            config.unwatched_dependencies(&config.upstreams[1]),
            ["gpu1"]
        );
    }

    #[test]
    fn startup_names_exempt_upstreams_once_and_warns_about_unwatched_ones() {
        let log = crate::test_log::capture();
        scoped_config(true).log_owner_scopes();
        let lines = log.contents();
        assert_eq!(lines.matches("owner veto exemptions").count(), 1, "{lines}");
        assert!(lines.contains("upstreams=\"helper, tts\""), "{lines}");
        assert_eq!(
            lines.matches("no pressure measurement watches").count(),
            1,
            "{lines}"
        );
        assert!(lines.contains("upstream=\"tts\""), "{lines}");
        assert!(lines.contains("resources=\"memory\""), "{lines}");
        drop(log);

        let mut host_only = scoped_config(true);
        for upstream in &mut host_only.upstreams {
            upstream.owner_scope = OwnerScope::Host;
        }
        let log = crate::test_log::capture();
        host_only.log_owner_scopes();
        assert_eq!(log.contents(), "");
    }
}

fn default_completion_timeout_s() -> u32 {
    3600
}

fn default_drain_timeout_s() -> u32 {
    30
}

fn default_true() -> bool {
    true
}

/// Every credential a configuration names, resolved.
#[derive(Debug)]
pub struct Credentials {
    /// What callers present to vacancyd.
    pub auth: crate::auth::Authenticator,
    /// What vacancyd presents to each upstream that has a credential, as an
    /// `Authorization` value marked sensitive, keyed by upstream id.
    pub upstreams: BTreeMap<String, reqwest::header::HeaderValue>,
}

/// The rules for an upstream's base URL. Error messages name the upstream,
/// never the URL, which may be carrying a secret.
fn validate_target(u: &UpstreamConfig) -> Result<()> {
    let target = reqwest::Url::parse(u.target.as_str())
        .with_context(|| format!("upstream '{}': invalid target URL", u.id))?;
    if target.scheme() != "http" {
        bail!(
            "upstream '{}': target scheme must be http (found '{}')",
            u.id,
            target.scheme()
        );
    }
    if target.host().is_none() {
        bail!("upstream '{}': target URL has no host", u.id);
    }
    if !target.username().is_empty() || target.password().is_some() {
        bail!(
            "upstream '{}': target must not contain credentials (user:password@); configure [upstream.credential] instead",
            u.id
        );
    }
    if target.query().is_some() || target.fragment().is_some() {
        bail!(
            "upstream '{}': target must not contain a query or fragment",
            u.id
        );
    }
    Ok(())
}

impl Config {
    pub fn load(path: &Path) -> Result<Self> {
        let raw = std::fs::read_to_string(path)
            .with_context(|| format!("reading config file {}", path.display()))?;
        let mut cfg: Config = toml::from_str(&raw)
            .map_err(|error| anyhow::anyhow!(describe_parse_error(&raw, &error)))
            .with_context(|| format!("parsing {}", path.display()))?;
        cfg.resolve_paths(path);
        cfg.validate()?;
        Ok(cfg)
    }

    pub(crate) fn validate(&self) -> Result<()> {
        if self.resources.is_empty() {
            bail!("no [[resource]] defined; nothing to lend");
        }

        if self.poll.interval_ms == 0 {
            bail!("poll.interval_ms must be greater than zero");
        }
        if self.owner.media_poll_ms < 1_000 {
            bail!("owner.media_poll_ms must be at least 1000");
        }
        self.validate_pressure()?;
        reject_empty_entries("owner.processes", &self.owner.processes)?;
        if self.owner.presence == PresenceMode::Headless && !self.owner.processes.is_empty() {
            bail!("owner.processes has no effect with owner.presence = \"headless\"; remove the processes or use presence = \"auto\"");
        }
        reject_empty_entries("owner.media_ignore", &self.owner.media_ignore)?;
        reject_empty_entries("owner.gpu_pressure.ignore", &self.owner.gpu_pressure.ignore)?;
        if !self.auth.trust_loopback && self.auth.tokens.is_empty() {
            bail!("auth.trust_loopback = false requires at least one auth token");
        }

        self.server.limits.validate()?;
        self.server.validate_local_admin()?;
        let server_listen = parse_listen("server.listen", &self.server.listen)?;
        let mut listeners = vec![("server.listen".to_string(), server_listen)];

        let mut resource_ids = BTreeSet::new();
        let mut selectors = BTreeMap::new();
        let nvidia_resources = self
            .resources
            .iter()
            .filter(|resource| resource.kind == "nvidia-gpu")
            .count();
        let probes = crate::plugin::probe::ProbeRegistry::with_builtins();
        for resource in &self.resources {
            validate_id("resource", &resource.id)?;
            if MODEL_REQUIREMENT_KEYS.contains(&resource.id.as_str()) {
                bail!(
                    "resource id '{}' is reserved: it is a key in [upstream.models] tables",
                    resource.id
                );
            }
            if !resource_ids.insert(resource.id.as_str()) {
                bail!("duplicate resource id '{}'", resource.id);
            }
            probes
                .build(resource)
                .map_err(|error| anyhow::anyhow!("resource '{}': {error}", resource.id))?;

            if resource.kind != "nvidia-gpu" {
                if resource.selector.is_some() {
                    bail!(
                        "resource '{}': selector is valid only for kind 'nvidia-gpu'",
                        resource.id
                    );
                }
                if resource.adapter.is_some() {
                    bail!(
                        "resource '{}': adapter is valid only for kind 'nvidia-gpu'",
                        resource.id
                    );
                }
            }
            if resource.kind == "nvidia-gpu" {
                if nvidia_resources > 1 && resource.selector.is_none() {
                    bail!(
                        "resource '{}': selector is required when more than one NVIDIA GPU resource is defined",
                        resource.id
                    );
                }
                if let Some(selector) = resource.selector.as_deref() {
                    let normalized = normalize_nvidia_selector(selector).with_context(|| {
                        format!("resource '{}': invalid NVIDIA selector", resource.id)
                    })?;
                    if let Some(first) = selectors.insert(normalized, resource.id.as_str()) {
                        bail!(
                            "resources '{}' and '{}' declare the same NVIDIA selector '{}'",
                            first,
                            resource.id,
                            selector
                        );
                    }
                }
                if resource.reserve.is_some() {
                    bail!(
                        "resource '{}': reserve is not used by kind 'nvidia-gpu'",
                        resource.id
                    );
                }
            }
            if resource.kind == "system-cpu" && resource.reserve.is_some_and(|value| value > 100) {
                bail!(
                    "resource '{}': CPU reserve must be a utilization percentage from 0 to 100",
                    resource.id
                );
            }
        }

        let mut adapters: Vec<_> = self
            .resources
            .iter()
            .filter_map(|resource| resource.adapter.map(|adapter| (adapter, &resource.id)))
            .collect();
        adapters.sort_unstable_by_key(|(adapter, _)| *adapter);
        for duplicate in adapters.windows(2) {
            if duplicate[0].0 == duplicate[1].0 {
                bail!(
                    "resources '{}' and '{}' declare the same adapter '{}'",
                    duplicate[0].1,
                    duplicate[1].1,
                    duplicate[0].0
                );
            }
        }

        let adapters = crate::plugin::adapter::AdapterRegistry::with_builtins();
        let mut upstream_ids = BTreeSet::new();
        for u in &self.upstreams {
            validate_id("upstream", &u.id)?;
            if !upstream_ids.insert(u.id.as_str()) {
                bail!("duplicate upstream id '{}'", u.id);
            }
            let adapter = adapters
                .build(&u.kind)
                .map_err(|error| anyhow::anyhow!("upstream '{}': {error}", u.id))?;
            // Two spellings of one model must not declare different budgets,
            // or the one that applies would depend on how a client spelled it.
            let mut canonical: BTreeMap<String, (&str, &ModelRequirement)> = BTreeMap::new();
            for (model, requirement) in &u.models {
                match canonical.entry(adapter.canonical_model(model)) {
                    std::collections::btree_map::Entry::Vacant(slot) => {
                        slot.insert((model.as_str(), requirement));
                    }
                    std::collections::btree_map::Entry::Occupied(slot) => {
                        let (first, first_requirement) = *slot.get();
                        if first_requirement != requirement {
                            bail!(
                                "upstream '{}': models '{}' and '{}' name the same model with different requirements",
                                u.id,
                                first,
                                model
                            );
                        }
                    }
                }
            }

            let listen = parse_listen(&format!("upstream '{}'.listen", u.id), &u.listen)?;
            for (other_name, other) in &listeners {
                if listeners_overlap(*other, listen) {
                    bail!(
                        "upstream '{}'.listen conflicts with {} on {}",
                        u.id,
                        other_name,
                        listen
                    );
                }
            }
            listeners.push((format!("upstream '{}'.listen", u.id), listen));

            let mut dependencies = BTreeSet::new();
            for id in std::iter::once(&u.resource).chain(u.also_requires.iter()) {
                if !resource_ids.contains(id.as_str()) {
                    bail!(
                        "upstream '{}' references resource '{}', which is not defined",
                        u.id,
                        id
                    );
                }
                if !dependencies.insert(id.as_str()) {
                    bail!("upstream '{}' names resource '{}' more than once", u.id, id);
                }
            }
            validate_target(u)?;
            if let Some(credential) = &u.credential {
                credential.validate(&u.id)?;
                if u.forward_authorization {
                    bail!(
                        "upstream '{}': credential and forward_authorization both set the Authorization header; choose one",
                        u.id
                    );
                }
            }
            if u.models.keys().any(|name| name.trim().is_empty()) {
                bail!("upstream '{}': model names must not be empty", u.id);
            }
            // A scalar requirement budgets only GPUs and a memory primary.
            // An upstream with neither would silently admit whatever the
            // operator meant to require, so such a budget is refused here.
            let scalar_targets = self
                .resources
                .iter()
                .filter(|resource| {
                    dependencies.contains(resource.id.as_str()) && u.scalar_applies_to(resource)
                })
                .count();
            let primary_kind = self
                .resources
                .iter()
                .find(|resource| resource.id == u.resource)
                .map(|resource| resource.kind.as_str());
            if scalar_targets == 0 {
                if u.default_min_free_mib > 0 {
                    bail!(
                        "upstream '{}': default_min_free_mib applies to an NVIDIA GPU or a system-memory primary resource, and this upstream has neither (its primary resource '{}' is {}); remove it, or make system memory the primary resource",
                        u.id,
                        u.resource,
                        primary_kind.unwrap_or("unknown")
                    );
                }
                if let Some((model, _)) = u.models.iter().find(|(_, requirement)| {
                    matches!(requirement.load, LoadRequirement::Scalar(mib) if mib > 0)
                }) {
                    bail!(
                        "upstream '{}': model '{}' has a single MiB requirement, which applies to an NVIDIA GPU or a system-memory primary resource, and this upstream has neither; declare it per resource, for example {{ memory = 8000 }}",
                        u.id,
                        model
                    );
                }
            }
            for (model, requirement) in &u.models {
                if let LoadRequirement::PerResource(requirements) = &requirement.load {
                    if primary_kind == Some("system-memory")
                        && !requirements.contains_key(&u.resource)
                    {
                        bail!(
                            "upstream '{}': model '{}' must declare its primary memory resource '{}'",
                            u.id,
                            model,
                            u.resource
                        );
                    }
                    if requirements.is_empty() {
                        bail!(
                            "upstream '{}': model '{}' has no per-resource requirements",
                            u.id,
                            model
                        );
                    }
                    for id in requirements.keys() {
                        if !dependencies.contains(id.as_str()) {
                            bail!(
                                "upstream '{}': model '{}' declares resource '{}', which is not a dependency",
                                u.id,
                                model,
                                id
                            );
                        }
                        if self
                            .resources
                            .iter()
                            .any(|resource| resource.id == *id && resource.kind == "system-cpu")
                        {
                            bail!(
                                "upstream '{}': model '{}' cannot declare MiB for CPU resource '{}'",
                                u.id,
                                model,
                                id
                            );
                        }
                    }
                    for id in &dependencies {
                        let is_gpu = self
                            .resources
                            .iter()
                            .any(|resource| resource.id == *id && resource.kind == "nvidia-gpu");
                        if is_gpu && !requirements.contains_key(*id) {
                            bail!(
                                "upstream '{}': model '{}' must declare every NVIDIA GPU dependency (missing '{}')",
                                u.id,
                                model,
                                id
                            );
                        }
                    }
                }
            }
        }

        self.validate_shared_servers()?;
        self.validate_owner_scopes()?;

        if self.auth.tokens.is_empty() && !self.auth.allow_unauthenticated_remote {
            if let Some((name, address)) = listeners.iter().find(|(_, addr)| !is_loopback(*addr)) {
                bail!(
                    "{name} binds to non-loopback address {address} but no auth tokens are configured; add credentials or set auth.allow_unauthenticated_remote = true"
                );
            }
        }
        Ok(())
    }

    /// An upstream exempt from the owner veto must not share its server with
    /// a host-scoped one. Owner-return reclaim can affect both routes on that
    /// server, depending on adapter and policy. A later settings change can
    /// enable server-wide unloading, so validation keeps them separate.
    fn validate_owner_scopes(&self) -> Result<()> {
        for exempt in veto_exempt_upstreams(self) {
            let Some(exempt_server) = UpstreamServer::of(exempt.target.as_str()) else {
                continue;
            };
            if let Some(host) = self.upstreams.iter().find(|upstream| {
                upstream.owner_scope == OwnerScope::Host
                    && UpstreamServer::of(upstream.target.as_str()).as_ref() == Some(&exempt_server)
            }) {
                bail!(
                    "upstream '{}' has owner_scope = \"resource\" but shares its server ({exempt_server}) with host-scoped upstream '{}': owner-return reclaim may affect both routes on that server, depending on adapter and policy. Run the exempt upstream on its own server instance",
                    exempt.id,
                    host.id
                );
            }
        }
        Ok(())
    }

    /// Two upstreams that reach the same server must borrow the same
    /// resources. Server-wide unloading, such as Ollama model unloads or
    /// ComfyUI's `/free`, can affect another route's work or resident models.
    /// Validation conservatively protects that shared boundary even when
    /// the current reclaim policy does not unload. A multi-GPU server is one
    /// upstream whose `also_requires` names the others.
    fn validate_shared_servers(&self) -> Result<()> {
        let mut seen: Vec<(UpstreamServer, &UpstreamConfig)> = Vec::new();
        for upstream in &self.upstreams {
            let Some(server) = UpstreamServer::of(upstream.target.as_str()) else {
                continue;
            };
            if let Some((_, first)) = seen.iter().find(|(other, _)| *other == server) {
                let (theirs, ours) = (dependency_set(first), dependency_set(upstream));
                if theirs != ours {
                    bail!(
                        "upstreams '{}' and '{}' reach the same server ({server}) but borrow different resources ({} and {}). Reclaim may affect work or resident models across that server, including the other's. Front a server that spans several GPUs with one upstream whose also_requires names the other resources, or run a separate server for each",
                        first.id,
                        upstream.id,
                        theirs.join(", "),
                        ours.join(", ")
                    );
                }
            }
            seen.push((server, upstream));
        }
        Ok(())
    }

    fn resolve_paths(&mut self, config_path: &Path) {
        let base = config_path.parent().unwrap_or_else(|| Path::new("."));
        let tokens = self.auth.tokens.iter_mut().map(|t| &mut t.value_file);
        let upstreams = self
            .upstreams
            .iter_mut()
            .filter_map(|u| u.credential.as_mut())
            .map(|c| &mut c.value_file);
        for path in tokens.chain(upstreams).flatten() {
            if path.is_relative() {
                *path = base.join(&*path);
            }
        }
    }

    /// Every file a credential is read from: auth tokens and upstream
    /// credentials alike.
    pub fn credential_files(&self) -> impl Iterator<Item = &PathBuf> {
        let tokens = self.auth.tokens.iter().map(|t| &t.value_file);
        let upstreams = self
            .upstreams
            .iter()
            .filter_map(|u| u.credential.as_ref())
            .map(|c| &c.value_file);
        tokens.chain(upstreams).flatten()
    }

    /// Resolve every credential the configuration names, as the daemon does
    /// at startup. A missing environment variable or unreadable secret file
    /// is an error here, so `check-config`, setup and a settings save catch
    /// it before a service restart does.
    pub fn resolve_credentials(&self) -> Result<Credentials> {
        let auth = crate::auth::Authenticator::new(&self.auth)?;
        let mut upstreams = BTreeMap::new();
        for upstream in &self.upstreams {
            if let Some(credential) = &upstream.credential {
                upstreams.insert(upstream.id.clone(), credential.resolve(&upstream.id)?);
            }
        }
        Ok(Credentials { auth, upstreams })
    }

    fn validate_pressure(&self) -> Result<()> {
        let pressure = &self.owner.gpu_pressure;
        validate_percentage("owner.gpu_pressure.threshold_pct", pressure.threshold_pct)?;
        validate_percentage("owner.gpu_pressure.margin_pct", pressure.margin_pct)?;
        validate_percentage("owner.gpu_pressure.floor_pct", pressure.floor_pct)?;
        validate_percentage("owner.gpu_pressure.cap_pct", pressure.cap_pct)?;
        // The margin may be anything from 0 to 100: the cap bounds what it
        // produces. A floor above the cap, or a cap above the tracker's
        // bound, loads and runs adjusted (see `effective_cap_pct`), because
        // earlier versions accepted both.
        if pressure.floor_pct == 0.0 || pressure.floor_pct >= 100.0 {
            bail!("GPU pressure needs 0 < floor_pct < 100");
        }
        if pressure.mode == crate::plugin::pressure::PressureMode::Fixed {
            if pressure.threshold_pct == 0.0 {
                bail!("mode = 'fixed' requires threshold_pct > 0");
            }
        } else if pressure.threshold_pct != 0.0 {
            bail!("threshold_pct requires mode = 'fixed'; use 0 for calibrated mode");
        }
        if !pressure.learn_quantile.is_finite()
            || pressure.learn_quantile <= 0.0
            || pressure.learn_quantile >= 1.0
        {
            bail!(
                "owner.gpu_pressure.learn_quantile must be finite and between 0 and 1, exclusive"
            );
        }
        if !pressure.learn_half_life_h.is_finite()
            || pressure.learn_half_life_h <= 0.0
            || pressure.learn_half_life_h > MAX_LEARN_HALF_LIFE_H
        {
            bail!("owner.gpu_pressure.learn_half_life_h must be greater than 0 and at most {MAX_LEARN_HALF_LIFE_H} (30 days)");
        }
        if !pressure.raise_pct_per_h.is_finite()
            || pressure.raise_pct_per_h <= 0.0
            || pressure.raise_pct_per_h > 100.0
        {
            bail!("owner.gpu_pressure.raise_pct_per_h must be greater than 0 and at most 100");
        }
        if pressure.poll_ms < 1_000 {
            bail!("owner.gpu_pressure.poll_ms must be at least 1000");
        }
        if pressure.warmup_samples == 0 {
            bail!("owner.gpu_pressure.warmup_samples must be greater than zero");
        }
        if !pressure.state_file.is_empty() && pressure.state_file.trim().is_empty() {
            bail!("owner.gpu_pressure.state_file must be empty or a non-blank path");
        }
        Ok(())
    }
}

/// A TOML error without the source excerpt the parser attaches.
///
/// The excerpt quotes the offending line, which can be an inline token
/// `value = "..."`; this message reaches the terminal and the Windows service
/// log. Line and column are enough to find the problem.
fn describe_parse_error(raw: &str, error: &toml::de::Error) -> String {
    let Some(span) = error.span() else {
        return error.message().to_string();
    };
    let before = &raw[..span.start.min(raw.len())];
    let line = before.matches('\n').count() + 1;
    let column = before.len() - before.rfind('\n').map_or(0, |at| at + 1) + 1;
    format!("line {line}, column {column}: {}", error.message())
}

fn validate_percentage(name: &str, value: f64) -> Result<()> {
    if !value.is_finite() || !(0.0..=100.0).contains(&value) {
        bail!("{name} must be finite and from 0 to 100");
    }
    Ok(())
}

fn reject_empty_entries(name: &str, values: &[String]) -> Result<()> {
    if values.iter().any(|value| value.trim().is_empty()) {
        bail!("{name} must not contain empty entries");
    }
    Ok(())
}

fn validate_id(kind: &str, id: &str) -> Result<()> {
    let mut chars = id.chars();
    if !chars.next().is_some_and(|ch| ch.is_ascii_alphanumeric())
        || !chars.all(|ch| ch.is_ascii_alphanumeric() || matches!(ch, '-' | '_' | '.'))
    {
        bail!(
            "{kind} id '{id}' must start with an ASCII letter or digit and contain only letters, digits, '.', '-', or '_'"
        );
    }
    Ok(())
}

fn parse_listen(name: &str, value: &str) -> Result<SocketAddr> {
    let address: SocketAddr = value
        .parse()
        .with_context(|| format!("{name} must be an IP socket address"))?;
    if address.port() == 0 {
        bail!("{name} must not use port 0");
    }
    Ok(address)
}

/// The server an upstream target reaches, compared by host and port only:
/// the path is part of the API, not of the server. Any loopback address,
/// `localhost` and the unspecified address are one host, since a server
/// bound to all interfaces answers on each of them. A server reached by a
/// LAN address and by loopback is not recognized as the same one.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct UpstreamServer {
    host: String,
    port: Option<u16>,
}

impl UpstreamServer {
    pub(crate) fn of(target: &str) -> Option<Self> {
        let url = reqwest::Url::parse(target).ok()?;
        let host = url
            .host_str()?
            .trim_start_matches('[')
            .trim_end_matches(']')
            .trim_end_matches('.')
            .to_ascii_lowercase();
        let local = host == "localhost"
            || host
                .parse::<IpAddr>()
                .is_ok_and(|ip| ip.is_loopback() || ip.is_unspecified());
        Some(Self {
            host: if local { "loopback".to_owned() } else { host },
            port: url.port_or_known_default(),
        })
    }
}

impl std::fmt::Display for UpstreamServer {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self.port {
            Some(port) => write!(f, "{} port {port}", self.host),
            None => f.write_str(&self.host),
        }
    }
}

/// An upstream's primary resource and `also_requires`, sorted.
fn dependency_set(upstream: &UpstreamConfig) -> Vec<&str> {
    let mut ids: Vec<&str> = std::iter::once(upstream.resource.as_str())
        .chain(upstream.also_requires.iter().map(String::as_str))
        .collect();
    ids.sort_unstable();
    ids.dedup();
    ids
}

fn is_loopback(address: SocketAddr) -> bool {
    address.ip().is_loopback()
}

fn listeners_overlap(left: SocketAddr, right: SocketAddr) -> bool {
    if left.port() != right.port() {
        return false;
    }
    let same_family = matches!(
        (left.ip(), right.ip()),
        (IpAddr::V4(_), IpAddr::V4(_)) | (IpAddr::V6(_), IpAddr::V6(_))
    );
    same_family
        && (left.ip() == right.ip() || left.ip().is_unspecified() || right.ip().is_unspecified())
}

fn normalize_nvidia_selector(selector: &str) -> Result<String> {
    if let Some(uuid) = selector.strip_prefix("GPU-") {
        if uuid.is_empty() {
            bail!("GPU UUID is empty");
        }
        return Ok(format!("uuid:{}", selector.to_ascii_lowercase()));
    }
    let index: u32 = selector
        .parse()
        .context("expected a GPU- UUID or a non-negative numeric index")?;
    Ok(format!("index:{index}"))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn config_with_resources(resources: &str) -> Config {
        toml::from_str(&format!(
            r#"
[server]
listen = "127.0.0.1:9811"
{resources}
"#
        ))
        .unwrap()
    }

    fn valid_config() -> Config {
        config_with_resources(
            r#"
[[resource]]
id = "gpu0"
kind = "nvidia-gpu"

[[upstream]]
id = "ollama"
listen = "127.0.0.1:11435"
target = "http://127.0.0.1:11434"
kind = "ollama"
resource = "gpu0"
"#,
        )
    }

    fn assert_invalid(cfg: &Config, expected: &str) {
        let error = cfg.validate().unwrap_err().to_string();
        assert!(
            error.contains(expected),
            "{error:?} did not contain {expected:?}"
        );
    }

    /// The workstation's lending layout: a separate server per upstream,
    /// each on its own loopback port.
    fn workstation_config() -> Config {
        config_with_resources(
            r#"
[owner.gpu_pressure]
enabled = true

[[resource]]
id = "gpu0"
kind = "nvidia-gpu"
selector = "GPU-5080"

[[resource]]
id = "gpu1"
kind = "nvidia-gpu"
selector = "GPU-3050"

[[resource]]
id = "mem"
kind = "system-memory"
reserve = 17179869184

[[upstream]]
id = "ollama"
listen = "0.0.0.0:11435"
target = "http://127.0.0.1:11434"
kind = "ollama"
resource = "gpu0"
also_requires = ["mem"]
yield_on_veto = true

[[upstream]]
id = "helper"
listen = "0.0.0.0:11437"
target = "http://127.0.0.1:11436"
kind = "ollama"
resource = "gpu1"
owner_scope = "resource"
on_owner_return = "cancel"

[[upstream]]
id = "llamacpp"
listen = "0.0.0.0:8001"
target = "http://127.0.0.1:8080"
kind = "llamacpp"
resource = "gpu0"
also_requires = ["mem"]

[[upstream]]
id = "comfyui"
listen = "0.0.0.0:8189"
target = "http://127.0.0.1:8188"
kind = "comfyui"
resource = "gpu0"
also_requires = ["mem"]
yield_on_veto = true

[[upstream]]
id = "chatterbox"
listen = "0.0.0.0:8859"
target = "http://127.0.0.1:8858"
kind = "chatterbox"
resource = "gpu0"
also_requires = ["mem"]
yield_on_veto = true

[auth]
allow_unauthenticated_remote = true
"#,
        )
    }

    #[test]
    fn a_separate_server_per_upstream_is_valid() {
        workstation_config().validate().unwrap();
    }

    #[test]
    fn upstreams_on_one_server_must_borrow_the_same_resources() {
        // The helper lane pointed at the main Ollama: pressure on gpu1 would
        // unload gpu0's models, and owner return would unload the helper's.
        for target in [
            "http://127.0.0.1:11434",
            "http://localhost:11434",
            "http://LOCALHOST.:11434",
            "http://[::1]:11434",
            "http://127.0.0.2:11434",
            "http://0.0.0.0:11434",
            "http://127.0.0.1:11434/v1",
        ] {
            let mut config = workstation_config();
            config.upstreams[1].target = target.into();
            let error = config.validate().unwrap_err().to_string();
            assert!(
                error.contains(
                    "upstreams 'ollama' and 'helper' reach the same server (loopback port 11434)"
                ),
                "{target}: {error}"
            );
            assert!(error.contains("gpu0, mem and gpu1"), "{error}");
            assert!(error.contains("also_requires"), "{error}");
        }
        // Another port, or another host, is another server.
        for target in ["http://127.0.0.1:11438", "http://192.0.2.7:11434"] {
            let mut config = workstation_config();
            config.upstreams[1].target = target.into();
            config.validate().unwrap();
        }
        // Default and explicit ports are compared as numbers.
        let mut config = workstation_config();
        config.upstreams[0].target = "http://192.0.2.7".into();
        config.upstreams[1].target = "http://192.0.2.7:80".into();
        assert_invalid(&config, "reach the same server (192.0.2.7 port 80)");
    }

    #[test]
    fn upstreams_on_one_server_with_the_same_resources_are_accepted() {
        // Two dialects of one Ollama (its native and OpenAI-compatible
        // routes) borrowing the same resources, listed in another order.
        let mut config = workstation_config();
        config.upstreams[2].kind = "openai".into();
        config.upstreams[2].target = "http://localhost:11434/".into();
        config.upstreams[2].resource = "mem".into();
        config.upstreams[2].also_requires = vec!["gpu0".into()];
        config.validate().unwrap();
        // A server spanning both GPUs is one upstream.
        let mut config = workstation_config();
        config.upstreams[0].also_requires = vec!["mem".into(), "gpu1".into()];
        config.upstreams.remove(1);
        config.validate().unwrap();
    }

    #[test]
    fn removed_whole_device_pressure_setting_is_rejected() {
        let error = toml::from_str::<Config>(
            r#"
[server]
listen = "127.0.0.1:9811"

[owner]
gpu_pressure_pct = 20
"#,
        )
        .unwrap_err()
        .to_string();

        assert!(error.contains("gpu_pressure_pct"), "{error}");
    }

    #[test]
    fn explicit_adapter_is_normalized_and_gpu_only() {
        let cfg = config_with_resources(
            r#"
[[resource]]
id = "gpu0"
kind = "nvidia-gpu"
adapter = "00000000:0E:00.0"
"#,
        );
        cfg.validate().unwrap();
        assert_eq!(
            cfg.resources[0].adapter.unwrap().to_string(),
            "0000:0e:00.0"
        );

        let non_gpu = config_with_resources(
            r#"
[[resource]]
id = "mem"
kind = "system-memory"
adapter = "0000:0e:00.0"
"#,
        );
        assert!(non_gpu
            .validate()
            .unwrap_err()
            .to_string()
            .contains("nvidia-gpu"));
    }

    #[test]
    fn two_resources_cannot_claim_one_explicit_adapter() {
        let cfg = config_with_resources(
            r#"
[[resource]]
id = "gpu0"
kind = "nvidia-gpu"
selector = "0"
adapter = "0000:0e:00.0"

[[resource]]
id = "gpu1"
kind = "nvidia-gpu"
selector = "1"
adapter = "00000000:0E:00.0"
"#,
        );
        let error = cfg.validate().unwrap_err().to_string();
        assert!(error.contains("same adapter"), "{error}");
    }

    #[test]
    fn remote_listeners_require_credentials_or_an_explicit_override() {
        let mut cfg = valid_config();
        cfg.server.listen = "0.0.0.0:9811".into();
        assert_invalid(&cfg, "no auth tokens");

        cfg.auth.allow_unauthenticated_remote = true;
        cfg.validate().unwrap();

        let mut upstream = valid_config();
        upstream.upstreams[0].listen = "192.0.2.10:11435".into();
        assert_invalid(&upstream, "no auth tokens");

        upstream.auth.tokens.push(crate::auth::TokenConfig {
            name: "cluster".into(),
            scopes: vec![crate::auth::Scope::Borrow],
            value: Some("0123456789abcdef".into()),
            value_env: None,
            value_file: None,
        });
        upstream.validate().unwrap();
    }

    #[test]
    fn disabling_loopback_trust_without_credentials_is_rejected() {
        let mut cfg = valid_config();
        cfg.auth.trust_loopback = false;
        assert_invalid(&cfg, "requires at least one auth token");
    }

    #[test]
    fn listener_addresses_ids_and_conflicts_are_rejected_early() {
        let mut malformed = valid_config();
        malformed.server.listen = "localhost:9811".into();
        assert_invalid(&malformed, "IP socket address");

        let mut ephemeral = valid_config();
        ephemeral.server.listen = "127.0.0.1:0".into();
        assert_invalid(&ephemeral, "port 0");

        let mut duplicate_id = valid_config();
        duplicate_id
            .upstreams
            .push(duplicate_id.upstreams[0].clone());
        assert_invalid(&duplicate_id, "duplicate upstream id");

        let mut duplicate_listen = valid_config();
        let mut second = duplicate_listen.upstreams[0].clone();
        second.id = "second".into();
        duplicate_listen.upstreams.push(second);
        assert_invalid(&duplicate_listen, "conflicts");

        let mut overlapping = valid_config();
        overlapping.auth.allow_unauthenticated_remote = true;
        overlapping.server.listen = "0.0.0.0:11435".into();
        assert_invalid(&overlapping, "conflicts");

        let mut bad_id = valid_config();
        bad_id.resources[0].id = "gpu/0".into();
        bad_id.upstreams[0].resource = "gpu/0".into();
        assert_invalid(&bad_id, "resource id");
    }

    #[test]
    fn a_parse_error_does_not_echo_the_offending_line() {
        let directory = std::env::temp_dir().join(format!(
            "vacancyd-parse-error-{}-{:?}",
            std::process::id(),
            std::thread::current().id()
        ));
        std::fs::create_dir_all(&directory).unwrap();
        let path = directory.join("vacancyd.toml");
        // An unterminated inline token: the parser's excerpt would quote it.
        std::fs::write(
            &path,
            "[server]\nlisten = \"127.0.0.1:9811\"\n\n[[auth.token]]\nname = \"cluster\"\nvalue = \"SUPERSECRETTOKENVALUE\n",
        )
        .unwrap();
        let error = format!("{:#}", Config::load(&path).unwrap_err());
        std::fs::remove_dir_all(&directory).unwrap();
        assert!(!error.contains("SUPERSECRET"), "{error}");
        assert!(error.contains("line 6"), "{error}");
        assert!(error.contains("vacancyd.toml"), "{error}");
    }

    #[test]
    fn local_admin_settings_default_and_reject_unsafe_values() {
        let cfg = valid_config();
        assert_eq!(cfg.server.admin_group, "vacancyd-admin");
        assert_eq!(
            cfg.server.admin_socket_path(),
            PathBuf::from("/run/vacancyd/admin.sock")
        );
        cfg.validate().unwrap();

        let mut custom = valid_config();
        custom.server.admin_socket = "/run/lender/admin.sock".into();
        custom.server.admin_group = "".into();
        custom.validate().unwrap();
        assert_eq!(
            custom.server.admin_socket_path(),
            PathBuf::from("/run/lender/admin.sock")
        );
        for group in ["wheel", "lan_admins", "vacancyd-admin", "grp.1"] {
            let mut ok = valid_config();
            ok.server.admin_group = group.into();
            ok.validate()
                .unwrap_or_else(|error| panic!("{group}: {error}"));
        }

        let long_name = format!("/run/vacancyd/{}", "s".repeat(65));
        let too_long = format!("/run/{}/admin.sock", "d".repeat(100));
        for socket in [
            "admin.sock",
            "run/vacancyd/admin.sock",
            "/admin.sock",
            "/run/vacancyd/",
            "/run/vacancyd/../admin.sock",
            "/run/./vacancyd/admin.sock",
            "/run//vacancyd/admin.sock",
            "/run/vacancyd/admin\n.sock",
            long_name.as_str(),
            too_long.as_str(),
        ] {
            let mut bad = valid_config();
            bad.server.admin_socket = socket.to_owned();
            assert!(bad.validate().is_err(), "{socket:?}");
        }
        for group in [
            "-admins",
            "a b",
            "a:b",
            "a,b",
            "a/b",
            "a\\b",
            "a\tb",
            &"g".repeat(257),
        ] {
            let mut bad = valid_config();
            bad.server.admin_group = group.to_string();
            assert!(bad.validate().is_err(), "{group:?}");
        }
    }

    #[test]
    fn listener_limits_default_to_safe_values_and_reject_nonsense() {
        let cfg = valid_config();
        assert_eq!(cfg.server.limits.header_read_timeout_s, 30);
        assert_eq!(cfg.server.limits.max_connections, 128);
        assert_eq!(cfg.server.limits.max_request_body_bytes(), 64 * 1024 * 1024);
        assert_eq!(cfg.server.limits.tunnel_idle_timeout_s, 3600);
        assert_eq!(cfg.server.limits.body_read_timeout_s, 120);
        assert_eq!(cfg.server.limits.body_idle_timeout_s, 15);
        assert_eq!(cfg.server.limits.write_stall_timeout_s, 60);
        assert_eq!(cfg.server.limits.buffered_body_kib(), 256 * 1024);

        let parsed: Config = toml::from_str(
            r#"
[server]
listen = "127.0.0.1:9811"
[server.limits]
max_request_body_mib = 16
[[resource]]
id = "mem"
kind = "system-memory"
"#,
        )
        .unwrap();
        assert_eq!(
            parsed.server.limits.max_request_body_bytes(),
            16 * 1024 * 1024
        );
        assert_eq!(parsed.server.limits.max_connections, 128);
        assert_eq!(parsed.server.limits.buffered_body_mib(), 256);

        // A request limit raised past the default budget, in a configuration
        // written before the budget existed, stays valid: the unset budget
        // grows to fit one body of the permitted size.
        let raised: Config = toml::from_str(
            r#"
[server]
listen = "127.0.0.1:9811"
[server.limits]
max_request_body_mib = 1024
[[resource]]
id = "mem"
kind = "system-memory"
"#,
        )
        .unwrap();
        raised.server.limits.validate().unwrap();
        assert_eq!(raised.server.limits.buffered_body_mib(), 1024);
        let explicit: Config = toml::from_str(
            r#"
[server]
listen = "127.0.0.1:9811"
[server.limits]
max_request_body_mib = 8
max_buffered_body_mib = 32
[[resource]]
id = "mem"
kind = "system-memory"
"#,
        )
        .unwrap();
        assert_eq!(explicit.server.limits.buffered_body_kib(), 32 * 1024);

        for (field, mutate) in [
            (
                "header_read_timeout_s",
                (|l: &mut ListenerLimits| l.header_read_timeout_s = 0) as fn(&mut ListenerLimits),
            ),
            ("max_connections", |l| l.max_connections = 0),
            ("max_request_body_mib", |l| l.max_request_body_mib = 0),
            ("tunnel_idle_timeout_s", |l| l.tunnel_idle_timeout_s = 0),
            ("body_read_timeout_s", |l| l.body_read_timeout_s = 0),
            ("body_read_timeout_s", |l| l.body_read_timeout_s = 3601),
            ("body_idle_timeout_s", |l| l.body_idle_timeout_s = 0),
            ("body_idle_timeout_s", |l| l.body_idle_timeout_s = 301),
            ("must not exceed body_read_timeout_s", |l| {
                l.body_read_timeout_s = 10;
                l.body_idle_timeout_s = 11;
            }),
            ("write_stall_timeout_s", |l| l.write_stall_timeout_s = 0),
            ("write_stall_timeout_s", |l| l.write_stall_timeout_s = 3601),
            ("max_buffered_body_mib", |l| {
                l.max_buffered_body_mib = Some(0)
            }),
            ("max_buffered_body_mib", |l| {
                l.max_buffered_body_mib = Some(65537)
            }),
            ("must be at least max_request_body_mib", |l| {
                l.max_request_body_mib = 512;
                l.max_buffered_body_mib = Some(511);
            }),
        ] {
            let mut cfg = valid_config();
            mutate(&mut cfg.server.limits);
            assert_invalid(&cfg, field);
        }
    }

    #[test]
    fn target_schemes_kinds_and_dependencies_are_validated() {
        let mut scheme = valid_config();
        scheme.upstreams[0].target = "https://127.0.0.1:11434".into();
        assert_invalid(&scheme, "scheme must be http");

        let mut query = valid_config();
        query.upstreams[0].target = "http://127.0.0.1:11434?destination=elsewhere".into();
        assert_invalid(&query, "query or fragment");
        query.upstreams[0].target = "http://127.0.0.1:11434/?api_key=fake-secret".into();
        assert!(!format!("{:#}", query.validate().unwrap_err()).contains("fake-secret"));

        let mut bad_adapter = valid_config();
        bad_adapter.upstreams[0].kind = "future-api".into();
        assert_invalid(&bad_adapter, "unknown upstream kind");

        let mut bad_probe = valid_config();
        bad_probe.resources[0].kind = "future-gpu".into();
        assert_invalid(&bad_probe, "unknown resource kind");

        let mut repeated = valid_config();
        repeated.upstreams[0].also_requires = vec!["gpu0".into()];
        assert_invalid(&repeated, "more than once");
    }

    #[test]
    fn model_requirements_accept_scalars_and_complete_per_resource_maps() {
        let cfg = config_with_resources(
            r#"
[[resource]]
id = "gpu0"
kind = "nvidia-gpu"
selector = "0"

[[resource]]
id = "gpu1"
kind = "nvidia-gpu"
selector = "1"

[[upstream]]
id = "llama"
listen = "127.0.0.1:11435"
target = "http://127.0.0.1:11434"
kind = "llamacpp"
resource = "gpu0"
also_requires = ["gpu1"]
[upstream.models]
legacy = 12000
split = { gpu0 = 10000, gpu1 = 14000 }
"#,
        );
        cfg.validate().unwrap();
        assert_eq!(
            cfg.upstreams[0].models["legacy"],
            ModelRequirement {
                load: LoadRequirement::Scalar(12000),
                resident_min_free_mib: 0,
            }
        );
    }

    fn models_config(models: &str) -> Result<Config> {
        let config: Config = toml::from_str(&format!(
            r#"
[server]
listen = "127.0.0.1:9811"
[[resource]]
id = "gpu0"
kind = "nvidia-gpu"
[[resource]]
id = "mem"
kind = "system-memory"
[[upstream]]
id = "ollama"
listen = "127.0.0.1:11435"
target = "http://127.0.0.1:11434"
kind = "ollama"
resource = "gpu0"
also_requires = ["mem"]
[upstream.models]
{models}
"#
        ))?;
        config.validate()?;
        Ok(config)
    }

    #[test]
    fn resident_min_free_mib_is_available_in_both_requirement_forms() {
        let config = models_config(
            r#"
"plain:1b" = 900
"scalar:12b" = { min_free_mib = 8200, resident_min_free_mib = 300 }
"split:20b" = { gpu0 = 12000, mem = 4096, resident_min_free_mib = 200 }
"#,
        )
        .unwrap();
        let models = &config.upstreams[0].models;
        assert_eq!(models["plain:1b"].load, LoadRequirement::Scalar(900));
        assert_eq!(models["plain:1b"].resident_min_free_mib, 0);
        assert_eq!(models["scalar:12b"].load, LoadRequirement::Scalar(8200));
        assert_eq!(models["scalar:12b"].resident_min_free_mib, 300);
        assert_eq!(models["split:20b"].for_resource("gpu0", true), Some(12000));
        assert_eq!(models["split:20b"].for_resource("mem", false), Some(4096));
        assert_eq!(models["split:20b"].resident_min_free_mib, 200);
    }

    fn cpu_route(primary: &str, upstream_extra: &str) -> Config {
        config_with_resources(&format!(
            r#"
[[resource]]
id = "mem"
kind = "system-memory"
reserve = 1024

[[resource]]
id = "cpu"
kind = "system-cpu"

[[upstream]]
id = "llm"
listen = "127.0.0.1:11435"
target = "http://127.0.0.1:11434"
kind = "openai"
resource = "{primary}"
also_requires = ["{other}"]
{upstream_extra}
"#,
            other = if primary == "mem" { "cpu" } else { "mem" }
        ))
    }

    #[test]
    fn a_scalar_budget_applies_to_a_memory_primary_and_never_to_a_cpu() {
        let memory = cpu_route(
            "mem",
            "default_min_free_mib = 8000
[upstream.models]
m = 4000",
        );
        memory.validate().unwrap();
        let up = &memory.upstreams[0];
        let resource = |id: &str| memory.resources.iter().find(|r| r.id == id).unwrap();
        assert!(up.scalar_applies_to(resource("mem")));
        assert!(!up.scalar_applies_to(resource("cpu")));
        assert_eq!(resource("mem").owner_reserve_bytes(), 1024);
        assert_eq!(resource("cpu").owner_reserve_bytes(), 0);

        // With a CPU primary and no GPU, a scalar budget applies nowhere.
        assert_invalid(
            &cpu_route("cpu", "default_min_free_mib = 8000"),
            "default_min_free_mib applies to an NVIDIA GPU or a system-memory primary resource",
        );
        assert_invalid(
            &cpu_route(
                "cpu",
                "[upstream.models]
m = 4000",
            ),
            "model 'm' has a single MiB requirement",
        );
        // A zero budget and a per-resource table are both meaningful there.
        cpu_route(
            "cpu",
            "default_min_free_mib = 0
[upstream.models]
m = 0
n = { mem = 4000 }",
        )
        .validate()
        .unwrap();
        // A memory primary must be named by every per-resource table.
        assert_invalid(
            &cpu_route(
                "mem",
                "[upstream.models]
m = { mem = 1, cpu = 1 }",
            ),
            "cannot declare MiB for CPU resource",
        );
        let mut missing = cpu_route(
            "mem",
            "[upstream.models]
m = { mem = 4000 }",
        );
        missing.validate().unwrap();
        missing.upstreams[0].also_requires = vec!["cpu".into(), "gpu".into()];
        missing.resources.push(ResourceConfig {
            id: "gpu".into(),
            kind: "nvidia-gpu".into(),
            selector: None,
            adapter: None,
            enabled: true,
            reserve: None,
        });
        missing.upstreams[0].models.insert(
            "g".into(),
            ModelRequirement {
                load: LoadRequirement::PerResource(BTreeMap::from([("gpu".into(), 8000)])),
                resident_min_free_mib: 0,
            },
        );
        assert_invalid(&missing, "must declare its primary memory resource 'mem'");
    }

    #[test]
    fn resident_min_free_mib_rejects_negative_and_ambiguous_values() {
        for (models, needle) in [
            (
                r#""m:1b" = { min_free_mib = 8200, resident_min_free_mib = -1 }"#,
                "must not be negative",
            ),
            (
                r#""m:1b" = { gpu0 = 8200, resident_min_free_mib = -300 }"#,
                "must not be negative",
            ),
            (r#""m:1b" = -5"#, "must not be negative"),
            (
                r#""m:1b" = { min_free_mib = 8200, gpu0 = 100 }"#,
                "cannot be combined",
            ),
            (
                r#""m:1b" = { resident_min_free_mib = 300 }"#,
                "has no per-resource requirements",
            ),
        ] {
            let error = format!("{:#}", models_config(models).unwrap_err());
            assert!(error.contains(needle), "{models}: {error}");
        }
    }

    #[test]
    fn requirement_keys_cannot_be_resource_ids() {
        let mut config = valid_config();
        config.resources[0].id = "resident_min_free_mib".into();
        config.upstreams[0].resource = "resident_min_free_mib".into();
        let error = config.validate().unwrap_err().to_string();
        assert!(error.contains("is reserved"), "{error}");
    }

    #[test]
    fn ollama_spellings_of_one_model_must_agree() {
        let ollama = |models: &str| {
            config_with_resources(&format!(
                r#"
[[resource]]
id = "gpu0"
kind = "nvidia-gpu"
selector = "0"
[[upstream]]
id = "ollama"
listen = "127.0.0.1:11435"
target = "http://127.0.0.1:11434"
kind = "ollama"
resource = "gpu0"
[upstream.models]
{models}
"#
            ))
        };
        ollama(
            "\"gpt-oss:20b\" = 12700
\"registry.ollama.ai/library/gpt-oss:20b\" = 12700",
        )
        .validate()
        .unwrap();
        assert_invalid(
            &ollama(
                "\"gpt-oss:20b\" = 12700
\"registry.ollama.ai/library/gpt-oss:20b\" = 8500",
            ),
            "name the same model with different requirements",
        );
    }

    #[test]
    fn per_resource_model_requirements_reject_gaps_and_non_dependencies() {
        let incomplete = config_with_resources(
            r#"
[[resource]]
id = "gpu0"
kind = "nvidia-gpu"
selector = "0"
[[resource]]
id = "gpu1"
kind = "nvidia-gpu"
selector = "1"
[[upstream]]
id = "llama"
listen = "127.0.0.1:11435"
target = "http://127.0.0.1:11434"
kind = "llamacpp"
resource = "gpu0"
also_requires = ["gpu1"]
[upstream.models]
split = { gpu0 = 10000 }
"#,
        );
        assert_invalid(&incomplete, "missing 'gpu1'");

        let unrelated = config_with_resources(
            r#"
[[resource]]
id = "gpu0"
kind = "nvidia-gpu"
[[resource]]
id = "mem"
kind = "system-memory"
[[upstream]]
id = "llama"
listen = "127.0.0.1:11435"
target = "http://127.0.0.1:11434"
kind = "llamacpp"
resource = "gpu0"
[upstream.models]
bad = { gpu0 = 10000, mem = 2000 }
"#,
        );
        assert_invalid(&unrelated, "not a dependency");
    }

    #[test]
    fn percentages_intervals_and_cpu_reserve_have_semantic_bounds() {
        let mut non_finite = valid_config();
        non_finite.owner.gpu_pressure.margin_pct = f64::NAN;
        assert_invalid(&non_finite, "margin_pct must be finite");

        let mut out_of_range = valid_config();
        out_of_range.owner.gpu_pressure.floor_pct = 100.1;
        assert_invalid(&out_of_range, "floor_pct must be finite");

        for (quantile, valid) in [
            (f64::INFINITY, false),
            (0.0, false),
            (1.0, false),
            (0.75, true),
        ] {
            let mut cfg = valid_config();
            cfg.owner.gpu_pressure.learn_quantile = quantile;
            match valid {
                true => cfg.validate().unwrap(),
                false => assert_invalid(&cfg, "learn_quantile must be finite"),
            }
        }
        for (half_life, valid) in [
            (f64::NAN, false),
            (0.0, false),
            (MAX_LEARN_HALF_LIFE_H + 1.0, false),
            (MAX_LEARN_HALF_LIFE_H, true),
            (0.5, true),
        ] {
            let mut cfg = valid_config();
            cfg.owner.gpu_pressure.learn_half_life_h = half_life;
            match valid {
                true => cfg.validate().unwrap(),
                false => assert_invalid(&cfg, "learn_half_life_h must be greater than 0"),
            }
        }
        for (raise, valid) in [
            (f64::INFINITY, false),
            (0.0, false),
            (100.5, false),
            (100.0, true),
        ] {
            let mut cfg = valid_config();
            cfg.owner.gpu_pressure.raise_pct_per_h = raise;
            match valid {
                true => cfg.validate().unwrap(),
                false => assert_invalid(&cfg, "raise_pct_per_h must be greater than 0"),
            }
        }

        let mut poll = valid_config();
        poll.poll.interval_ms = 0;
        assert_invalid(&poll, "poll.interval_ms");

        let mut pressure_poll = valid_config();
        pressure_poll.owner.gpu_pressure.poll_ms = 0;
        assert_invalid(&pressure_poll, "gpu_pressure.poll_ms");

        let mut media_poll = valid_config();
        media_poll.owner.media_poll_ms = 999;
        assert_invalid(&media_poll, "media_poll_ms");

        let mut cpu = valid_config();
        cpu.resources.push(ResourceConfig {
            id: "cpu".into(),
            kind: "system-cpu".into(),
            selector: None,
            adapter: None,
            enabled: true,
            reserve: Some(101),
        });
        assert_invalid(&cpu, "CPU reserve");
    }

    #[test]
    fn multi_gpu_resources_must_select_distinct_devices() {
        let mut missing = valid_config();
        let mut second = missing.resources[0].clone();
        second.id = "gpu1".into();
        missing.resources.push(second);
        assert_invalid(&missing, "selector is required");

        let mut duplicate = valid_config();
        duplicate.resources[0].selector = Some("0".into());
        let mut second = duplicate.resources[0].clone();
        second.id = "gpu1".into();
        second.selector = Some("00".into());
        duplicate.resources.push(second);
        assert_invalid(&duplicate, "same NVIDIA selector");

        let mut malformed = valid_config();
        malformed.resources[0].selector = Some("first-card".into());
        assert_invalid(&malformed, "invalid NVIDIA selector");
    }

    #[test]
    fn calibration_modes_and_feasibility_are_explicit() {
        use crate::plugin::pressure::PressureMode;
        let mut cfg = valid_config();
        assert_eq!(cfg.owner.gpu_pressure.mode, PressureMode::Calibrated);
        cfg.owner.gpu_pressure.threshold_pct = 45.0;
        assert_invalid(&cfg, "requires mode = 'fixed'");
        cfg.owner.gpu_pressure.mode = PressureMode::Fixed;
        cfg.validate().unwrap();
        cfg.owner.gpu_pressure.threshold_pct = 0.0;
        assert_invalid(&cfg, "requires threshold_pct > 0");
        cfg.owner.gpu_pressure.mode = PressureMode::Calibrated;
        // The cap bounds the learned threshold, so a margin that would take
        // it past 100 is harmless.
        cfg.owner.gpu_pressure.cap_pct = 80.0;
        cfg.owner.gpu_pressure.margin_pct = 50.0;
        cfg.validate().unwrap();
        cfg.owner.gpu_pressure.floor_pct = 0.0;
        assert_invalid(&cfg, "0 < floor_pct < 100");
        cfg.owner.gpu_pressure.floor_pct = 100.0;
        assert_invalid(&cfg, "0 < floor_pct < 100");
        assert!(toml::from_str::<GpuPressureConfig>("mode = 'magic'").is_err());
    }

    #[test]
    fn earlier_floor_and_cap_combinations_load_warn_and_run_adjusted() {
        // (floor, cap, effective cap, warning)
        for (floor, cap, effective, warning) in [
            (25.0, 40.0, 40.0, None),
            (25.0, 25.0, 25.0, None),
            (25.0, 90.0, 90.0, None),
            // v1 meaning: the threshold is never below the floor.
            (30.0, 20.0, 30.0, Some("cap runs at the floor")),
            (25.0, 0.0, 25.0, Some("cap runs at the floor")),
            (25.0, 100.0, 90.0, Some("cap_pct above 90 runs at 90")),
            (95.0, 40.0, 95.0, Some("cap runs at the floor")),
        ] {
            let mut cfg = valid_config();
            cfg.owner.gpu_pressure.floor_pct = floor;
            cfg.owner.gpu_pressure.cap_pct = cap;
            cfg.validate().unwrap();
            let tracker = cfg.owner.gpu_pressure.tracker_config(120);
            assert_eq!(tracker.cap_pct, effective, "{floor} {cap}");
            assert!(tracker.cap_pct >= tracker.floor_pct);
            let log = crate::test_log::capture();
            cfg.log_setting_warnings();
            let lines = log.contents();
            drop(log);
            match warning {
                Some(text) => assert!(
                    lines.contains(text)
                        && lines.contains("setting=\"owner.gpu_pressure.cap_pct\""),
                    "{floor} {cap}: {lines}"
                ),
                None => assert_eq!(lines, "", "{floor} {cap}"),
            }
        }
        // Fixed mode does not use the cap, so it does not warn about it.
        let mut fixed = valid_config();
        fixed.owner.gpu_pressure.mode = crate::plugin::pressure::PressureMode::Fixed;
        fixed.owner.gpu_pressure.threshold_pct = 50.0;
        fixed.owner.gpu_pressure.cap_pct = 100.0;
        fixed.validate().unwrap();
        let log = crate::test_log::capture();
        fixed.log_setting_warnings();
        assert_eq!(log.contents(), "");
    }

    #[test]
    fn retired_and_deprecated_pressure_settings_load_warn_and_change_nothing() {
        use crate::plugin::pressure::PressureMode;
        let old: GpuPressureConfig = toml::from_str(
            "mode = 'adaptive'\nbaseline_quantile = 0.25\nbaseline_window_s = 21600\n",
        )
        .unwrap();
        assert_eq!(old.mode, PressureMode::Adaptive);
        assert_eq!(old.baseline_quantile, Some(0.25));
        // The tracker sees calibrated mode and the learning quantile, not
        // the deprecated one.
        let tracker = old.tracker_config(120);
        assert_eq!(tracker.mode, PressureMode::Calibrated);
        assert_eq!(
            tracker,
            GpuPressureConfig::default().tracker_config(120),
            "deprecated keys reach nothing the tracker uses"
        );
        // Unknown keys are still refused.
        assert!(toml::from_str::<GpuPressureConfig>("baseline_quantiles = 0.25").is_err());

        let mut cfg = valid_config();
        cfg.owner.gpu_pressure = old;
        cfg.validate().unwrap();
        let log = crate::test_log::capture();
        cfg.log_setting_warnings();
        let lines = log.contents();
        drop(log);
        for setting in [
            "owner.gpu_pressure.mode",
            "owner.gpu_pressure.baseline_quantile",
            "owner.gpu_pressure.baseline_window_s",
        ] {
            assert!(
                lines.contains(&format!("setting=\"{setting}\"")),
                "{setting}: {lines}"
            );
        }
        assert!(lines.contains("retired"), "{lines}");

        let log = crate::test_log::capture();
        valid_config().log_setting_warnings();
        assert_eq!(log.contents(), "");
    }

    #[test]
    fn empty_ignore_patterns_are_rejected() {
        let mut cfg = valid_config();
        cfg.owner.gpu_pressure.ignore.push(String::new());
        assert_invalid(&cfg, "gpu_pressure.ignore");

        let mut blank_state = valid_config();
        blank_state.owner.gpu_pressure.state_file = "   ".into();
        assert_invalid(&blank_state, "state_file");
    }

    #[test]
    fn relative_files_are_resolved_from_the_config_directory() {
        let config_path = Path::new("configs/vacancyd.toml");
        let mut cfg = valid_config();
        cfg.owner.gpu_pressure.state_file = "state/pressure.json".into();
        cfg.auth.tokens.push(crate::auth::TokenConfig {
            name: "cluster".into(),
            scopes: vec![crate::auth::Scope::Borrow],
            value: None,
            value_env: None,
            value_file: Some("secrets/cluster.token".into()),
        });
        cfg.resolve_paths(config_path);

        assert_eq!(
            cfg.owner.gpu_pressure.state_path(config_path),
            PathBuf::from("configs/state/pressure.json")
        );
        assert_eq!(
            cfg.auth.tokens[0].value_file.as_deref(),
            Some(Path::new("configs/secrets/cluster.token"))
        );
    }

    #[test]
    fn url_credentials_are_refused_without_being_echoed() {
        for target in [
            "http://demo:fake-secret@127.0.0.1:19813",
            "http://fake-secret@127.0.0.1:19813",
            "http://:fake-secret@127.0.0.1:19813/",
            "http://demo:fake-secret@127.0.0.1:19813?also=fake-secret",
        ] {
            let mut cfg = valid_config();
            cfg.upstreams[0].target = target.into();
            let error = format!("{:#}", cfg.validate().unwrap_err());
            assert!(error.contains("must not contain credentials"), "{error}");
            assert!(error.contains("[upstream.credential]"), "{error}");
            assert!(!error.contains("fake-secret"), "{error}");
            // Debug of the whole configuration shows the target redacted.
            let debug = format!("{cfg:?}");
            assert!(!debug.contains("fake-secret"), "{debug}");
            assert!(debug.contains("***@127.0.0.1:19813"), "{debug}");
        }
    }

    #[test]
    fn a_target_that_does_not_parse_is_not_echoed() {
        let mut cfg = valid_config();
        cfg.upstreams[0].target = "http://demo:fake-secret@[::1".into();
        let error = format!("{:#}", cfg.validate().unwrap_err());
        assert!(error.contains("invalid target URL"), "{error}");
        assert!(!error.contains("fake-secret"), "{error}");
        assert!(!format!("{cfg:?}").contains("fake-secret"));
    }

    fn credentialed(credential: &str, forward_authorization: bool) -> Config {
        config_with_resources(&format!(
            r#"
[[resource]]
id = "gpu0"
kind = "nvidia-gpu"

[[upstream]]
id = "llama"
listen = "127.0.0.1:11435"
target = "http://127.0.0.1:8080"
kind = "openai"
resource = "gpu0"
forward_authorization = {forward_authorization}

[upstream.credential]
{credential}
"#
        ))
    }

    #[test]
    fn upstream_credentials_are_validated_with_the_configuration() {
        credentialed("scheme = \"bearer\"\nvalue_file = \"llama.key\"", false)
            .validate()
            .unwrap();
        credentialed(
            "scheme = \"basic\"\nusername = \"vacancyd\"\nvalue_env = \"LLAMA_PASSWORD\"",
            false,
        )
        .validate()
        .unwrap();
        assert_invalid(
            &credentialed("scheme = \"bearer\"\nvalue_file = \"llama.key\"", true),
            "choose one",
        );
        assert_invalid(
            &credentialed("scheme = \"bearer\"", false),
            "exactly one of value_env, value_file",
        );
        assert!(toml::from_str::<Config>(
            "[server]\nlisten = \"127.0.0.1:9811\"\n[[upstream]]\nid = \"u\"\nlisten = \"127.0.0.1:1\"\ntarget = \"http://127.0.0.1:2\"\nkind = \"ollama\"\nresource = \"r\"\n[upstream.credential]\nscheme = \"bearer\"\nvalue = \"inline-secret\"\n"
        )
        .is_err(), "an inline secret is not accepted");
    }

    #[test]
    fn credential_files_include_upstream_credentials_and_resolve_relative_paths() {
        let config_path = Path::new("configs/vacancyd.toml");
        let mut cfg = credentialed("scheme = \"bearer\"\nvalue_file = \"llama.key\"", false);
        cfg.auth.tokens.push(crate::auth::TokenConfig {
            name: "cluster".into(),
            scopes: vec![crate::auth::Scope::Borrow],
            value: None,
            value_env: None,
            value_file: Some("cluster.token".into()),
        });
        assert_eq!(
            cfg.credential_files().collect::<Vec<_>>(),
            [Path::new("cluster.token"), Path::new("llama.key")]
        );
        cfg.resolve_paths(config_path);
        assert_eq!(
            cfg.credential_files().collect::<Vec<_>>(),
            [
                Path::new("configs/cluster.token"),
                Path::new("configs/llama.key")
            ]
        );
    }

    #[test]
    fn shipped_example_is_valid_and_loopback_only() {
        let cfg: Config = toml::from_str(include_str!("../vacancyd.example.toml")).unwrap();
        cfg.validate().unwrap();
        assert!(cfg.server.listen.starts_with("127."));
        assert!(cfg.upstreams.iter().all(|u| u.listen.starts_with("127.")));
    }

    #[test]
    fn sign_in_screen_lends_unless_the_owner_opts_out() {
        let default: OwnerConfig = toml::from_str("").unwrap();
        assert!(!default.sign_in_screen_is_presence);
        assert!(!default.presence_options().sign_in_screen_is_presence);
        let refusing: OwnerConfig = toml::from_str("sign_in_screen_is_presence = true").unwrap();
        assert!(refusing.presence_options().sign_in_screen_is_presence);
        assert!(toml::from_str::<OwnerConfig>("sign_in_screen_is_presence = \"yes\"").is_err());
    }

    #[test]
    fn presence_defaults_to_the_platform_backend_and_headless_is_explicit() {
        let default: OwnerConfig = toml::from_str("").unwrap();
        assert_eq!(default.presence, PresenceMode::Auto);
        assert!(!default.presence_options().headless);
        let headless: OwnerConfig = toml::from_str("presence = \"headless\"").unwrap();
        assert!(headless.presence_options().headless);
        for invalid in ["\"Headless\"", "\"none\"", "true"] {
            assert!(toml::from_str::<OwnerConfig>(&format!("presence = {invalid}")).is_err());
        }

        let mut cfg = valid_config();
        cfg.owner.presence = PresenceMode::Headless;
        cfg.owner.processes.clear();
        cfg.validate().unwrap();
        cfg.owner.processes = vec!["Marathon".into()];
        assert_invalid(&cfg, "owner.presence = \"headless\"");
    }

    #[test]
    fn upstream_management_is_denied_unless_allowed_by_name() {
        assert_eq!(
            valid_config().upstreams[0].management,
            ManagementPolicy::Deny
        );
        let upstream = |value: &str| {
            toml::from_str::<Config>(&format!(
                r#"
[server]
listen = "127.0.0.1:9811"
[[resource]]
id = "gpu0"
kind = "nvidia-gpu"
[[upstream]]
id = "ollama"
listen = "127.0.0.1:11435"
target = "http://127.0.0.1:11434"
kind = "ollama"
resource = "gpu0"
management = {value}
"#
            ))
        };
        assert_eq!(
            upstream("\"allow\"").unwrap().upstreams[0].management,
            ManagementPolicy::Allow
        );
        assert_eq!(
            upstream("\"deny\"").unwrap().upstreams[0].management,
            ManagementPolicy::Deny
        );
        for rejected in ["\"gated\"", "\"Allow\"", "\"\""] {
            let error = upstream(rejected).unwrap_err().message().to_owned();
            assert!(
                error.contains("expected `deny` or `allow`"),
                "{rejected}: {error}"
            );
        }
        assert!(upstream("true").is_err());
    }

    #[test]
    fn log_level_defaults_to_info_and_accepts_only_known_levels() {
        assert_eq!(valid_config().log.level, LogLevel::Info);
        let parse = |table: &str| {
            toml::from_str::<Config>(&format!(
                "[server]\nlisten = \"127.0.0.1:9811\"\n[log]\n{table}\n"
            ))
        };
        for (text, level) in [
            ("error", LogLevel::Error),
            ("warn", LogLevel::Warn),
            ("info", LogLevel::Info),
            ("debug", LogLevel::Debug),
            ("trace", LogLevel::Trace),
        ] {
            let config = parse(&format!("level = \"{text}\"")).unwrap();
            assert_eq!(config.log.level, level);
            assert_eq!(level.as_str(), text);
        }
        assert_eq!(parse("").unwrap().log.level, LogLevel::Info);
        for rejected in ["level = \"verbose\"", "level = \"DEBUG\"", "level = 3"] {
            assert!(parse(rejected).is_err(), "{rejected}");
        }
        let unknown = parse("level = \"info\"\nfile = \"vacancyd.log\"")
            .unwrap_err()
            .message()
            .to_owned();
        assert!(unknown.contains("unknown field `file`"), "{unknown}");
    }
}
