//! Versioned, read-only status contract. This module has no runtime authority.
//!
//! Additive fields are compatible within a schema version. Clients must check
//! the version before deserializing the complete response and must never reuse
//! a previous successful response after a read or validation failure.
use serde::{Deserialize, Serialize};

pub const SCHEMA_VERSION: u32 = 1;
pub const MAX_STATUS_BYTES: usize = 1024 * 1024;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Status {
    pub schema_version: u32,
    pub daemon: DaemonIdentity,
    pub ready: bool,
    pub readiness_failures: Vec<String>,
    pub lending_enabled: bool,
    pub owner_veto: OwnerVeto,
    pub presence: Presence,
    pub presence_backend: String,
    pub agent: Option<Agent>,
    pub agent_channel: AgentChannel,
    pub session_authority: SessionAuthority,
    pub resources: Vec<Resource>,
    pub upstreams: Vec<Upstream>,
    pub uptime_s: u64,
    #[serde(default)]
    pub work: Vec<Work>,
    /// Revision of the config this running process actually loaded, so the
    /// console can tell a saved-but-not-yet-active settings change from one
    /// already in effect. Absent only if the daemon could not hash its
    /// config file at startup.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub config_revision: Option<String>,
    /// What the daemon is logging. Absent from a daemon that predates the
    /// debug switch.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub logging: Option<Logging>,
    /// The local admin channel. Absent from a daemon that predates it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub admin_channel: Option<AdminChannel>,
}

/// The local admin channel: the admin pipe on Windows, the admin socket on
/// Linux. Without it a local owner can still read status and pause over
/// loopback, but cannot make the other administrative changes.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AdminChannel {
    /// `listening`; `unavailable` while it could not be started, which the
    /// service retries with backoff and explains in its log; or
    /// `not_supported` on a platform without one.
    pub state: String,
}

/// The daemon's log level and any temporary debug window.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Logging {
    /// The level vacancyd's own events are written at: `error`, `warn`,
    /// `info`, `debug` or `trace`, or `custom` while a `RUST_LOG` filter
    /// decides it.
    pub level: String,
    /// What set `level`: `config` (`[log] level`), `environment`
    /// (`RUST_LOG`, which takes precedence over the configuration) or
    /// `debug_window` (a temporary debug window).
    pub source: String,
    /// `[log] level` from the running configuration.
    pub configured_level: String,
    /// `RUST_LOG` is set and replaces `configured_level`.
    pub environment_override: bool,
    /// When the open debug window closes on its own, as an RFC 3339 UTC
    /// time. Absent when no window is open.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub debug_until: Option<String>,
    /// Whole seconds until the debug window closes. Absent when no window
    /// is open.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub debug_remaining_s: Option<u64>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DaemonIdentity {
    pub name: String,
    pub version: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct OwnerVeto {
    pub active: bool,
    pub cause: Option<String>,
    /// Upstreams the owner veto never refuses or reclaims, because they are
    /// configured with `owner_scope = "resource"`. Listed whether or not the
    /// veto is active: while it is, lending is paused only for the other
    /// upstreams, and each listed upstream's own `vacancy` says whether it is
    /// admitting. Absent from a daemon without owner scopes, which is the
    /// same as empty.
    #[serde(default)]
    pub exempt_upstreams: Vec<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Presence {
    pub session: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub session_id: Option<u32>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub applicable_session_ids: Vec<u32>,
    pub session_authority: String,
    pub foreground: Option<String>,
    pub owner_app: Option<String>,
    pub idle_seconds: Option<u64>,
    pub media: Media,
    /// Why media detection is unreadable, present only while `media` is
    /// `unknown`. Diagnostic text; do not parse it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub media_error: Option<String>,
    /// One line on why the presence backend reached this state. Diagnostic
    /// text; do not parse it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub basis: Option<String>,
    /// Sessions a backend that enumerates them (systemd-logind) read, with
    /// whether each one counts as someone at the machine.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub sessions: Vec<PresenceSession>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PresenceSession {
    pub id: String,
    pub class: String,
    #[serde(rename = "type")]
    pub kind: String,
    pub remote: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub seat: Option<String>,
    pub state: String,
    pub active: bool,
    pub locked: bool,
    pub counted: bool,
    pub reason: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Media {
    NotChecked,
    Idle,
    Playing(String),
    Unknown,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Agent {
    pub session_id: u32,
    pub agent_version: String,
    pub protocol_version: u32,
    pub age_s: u64,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AgentChannel {
    pub state: String,
    pub protocol_version: u32,
    pub report_ttl_s: u64,
    pub active_connections: i64,
    pub connection_limit: usize,
    pub last_rejection: Option<AgentRejection>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AgentRejection {
    pub reason: String,
    pub age_s: u64,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SessionAuthority {
    pub state: String,
    pub policy: String,
    pub applicable_count: usize,
    pub usable_count: usize,
    pub sessions: Vec<AgentSession>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AgentSession {
    pub session_id: u32,
    pub report_state: String,
    pub agent_version: Option<String>,
    pub protocol_version: Option<u32>,
    pub age_s: Option<u64>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Quality {
    Unknown,
    Ready,
    Stale,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Observation {
    pub quality: Quality,
    pub age_s: Option<u64>,
    pub last_error: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Resource {
    pub id: String,
    pub kind: String,
    pub probe: String,
    pub label: Option<String>,
    /// Whether at least one configured upstream owns this resource as an
    /// admission dependency. False means the resource is explicitly unused.
    #[serde(default = "default_required")]
    pub required: bool,
    #[serde(default)]
    pub required_by: Vec<String>,
    pub enabled: bool,
    pub lendable: bool,
    pub free_mib: Option<u64>,
    pub total_mib: Option<u64>,
    pub utilization_pct: Option<u32>,
    pub capacity: Observation,
    pub pressure: Option<Pressure>,
    pub adapter_mapping: Option<AdapterMapping>,
}

fn default_required() -> bool {
    true
}

fn default_owner_scope() -> String {
    OWNER_SCOPE_HOST.into()
}

/// `owner_scope` of an upstream the owner veto refuses and reclaims.
pub const OWNER_SCOPE_HOST: &str = "host";
/// `owner_scope` of an upstream the owner veto does not apply to.
pub const OWNER_SCOPE_RESOURCE: &str = "resource";

impl Upstream {
    /// Whether the owner veto leaves this upstream alone. Only the documented
    /// `resource` value exempts it; anything else is treated as host-scoped.
    pub fn veto_exempt(&self) -> bool {
        self.owner_scope == OWNER_SCOPE_RESOURCE
    }
}

impl Status {
    /// Veto-exempt upstreams that are serving borrowed work right now: either
    /// admitting new work or holding accepted work. While the owner veto is
    /// active these are the only upstreams lending.
    pub fn exempt_serving(&self) -> Vec<&str> {
        self.upstreams
            .iter()
            .filter(|upstream| {
                upstream.veto_exempt()
                    && (upstream.vacancy
                        || self.work.iter().any(|work| work.upstream == upstream.id))
            })
            .map(|upstream| upstream.id.as_str())
            .collect()
    }

    /// Whether the service is not ready only because GPU pressure baselines
    /// are still calibrating. That is a normal wait, not a fault: the console
    /// window and the tray both present it as "Preparing safe lending".
    /// Mirrors the console renderer's check; keep the two in step.
    pub fn calibrating_only(&self) -> bool {
        !self.ready
            && !self.readiness_failures.is_empty()
            && self.readiness_failures.iter().all(|failure| {
                self.resources.iter().any(|resource| {
                    resource.required
                        && resource.pressure.as_ref().is_some_and(|pressure| {
                            pressure.observation.quality == Quality::Ready
                                && pressure.calibrated == Some(false)
                        })
                        && *failure == format!("resource '{}' pressure is not ready", resource.id)
                })
            })
    }

    /// Whether the service is not ready because a resource it needs has a
    /// stale or unknown reading. Mirrors the console renderer's check.
    pub fn untrusted_evidence(&self) -> bool {
        !self.ready
            && self.resources.iter().any(|resource| {
                resource.required
                    && resource.enabled
                    && (resource.capacity.quality != Quality::Ready
                        || resource
                            .pressure
                            .as_ref()
                            .is_some_and(|pressure| pressure.observation.quality != Quality::Ready))
            })
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Pressure {
    #[serde(flatten)]
    pub observation: Observation,
    pub non_lender_pct: Option<f64>,
    pub engaged: Option<bool>,
    /// The engage threshold in force.
    pub threshold_pct: Option<f64>,
    /// The learned quantile, once the profile's histogram is warm.
    pub baseline_pct: Option<f64>,
    /// The threshold the learned quantile asks for. While it is above
    /// `threshold_pct`, the threshold in force is rising toward it.
    #[serde(default)]
    pub target_pct: Option<f64>,
    /// Valid readings the observed profile has learned from.
    #[serde(default)]
    pub learned_samples: Option<u64>,
    /// Observed seconds those readings span.
    #[serde(default)]
    pub learned_s: Option<u64>,
    pub mode: String,
    pub calibrated: Option<bool>,
    pub calibration_samples: Option<usize>,
    pub calibration_required: usize,
    pub calibration_hint: Option<String>,
    pub above_for_s: Option<u64>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AdapterMapping {
    #[serde(flatten)]
    pub observation: Observation,
    pub pci: Option<String>,
    pub luid: Option<String>,
    pub source: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Upstream {
    pub id: String,
    pub adapter: String,
    pub can_unload: bool,
    pub per_model_gating: bool,
    pub listen: String,
    /// The upstream's base URL as [`RedactedUrl`] shows it.
    pub target: String,
    pub resource: String,
    /// What the owner veto applies to. `host`: owner presence and incomplete
    /// session evidence refuse this upstream's new work and reclaim its
    /// accepted work. `resource`: exempt from both; only its own resources
    /// (capacity, pressure, switches) gate it. Absent from a daemon without
    /// owner scopes, where every upstream is `host`.
    #[serde(default = "default_owner_scope")]
    pub owner_scope: String,
    pub vacancy: bool,
    pub refusal: Option<Refusal>,
    /// Set when admission credits a model the upstream already holds, so a
    /// vacancy that would not exist for a cold load says why it exists.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub residency: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub health: Option<TargetHealth>,
    #[serde(default)]
    pub models: Vec<Model>,
    #[serde(default)]
    pub model_observation: Option<Observation>,
    #[serde(default)]
    pub listener: Listener,
    /// Present once this upstream's borrowed work has run while no process
    /// serving its target used a GPU, until one is seen using a GPU again.
    /// GPU pressure may then be counting that work as owner load. Absent
    /// otherwise, and from a daemon that predates the field.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub unseen_gpu_work: Option<UnseenGpuWork>,
}

/// Borrowed GPU work that pressure attribution may not see: the serving
/// process tree did no GPU work while the upstream's work ran, as when the
/// listener only relays connections into a virtual machine or container.
/// Diagnostic only; nothing extra is subtracted from pressure.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct UnseenGpuWork {
    /// The tracked GPUs this upstream borrows, whose pressure may include
    /// its work, sorted.
    pub resources: Vec<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Listener {
    pub state: String,
    #[serde(flatten)]
    pub observation: Observation,
}

impl Default for Listener {
    fn default() -> Self {
        Self {
            state: "unknown".into(),
            observation: Observation {
                quality: Quality::Unknown,
                age_s: None,
                last_error: None,
            },
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TargetHealth {
    #[serde(flatten)]
    pub observation: Observation,
    pub http_status: Option<u16>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Refusal {
    pub reason: String,
    pub detail: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub resource: Option<String>,
    pub retry_after_s: u32,
}

#[derive(Debug, thiserror::Error)]
pub enum DecodeError {
    #[error("status response exceeds the size limit")]
    TooLarge,
    #[error("incompatible status schema {0}")]
    Incompatible(u32),
    #[error("malformed status response: {0}")]
    Malformed(#[from] serde_json::Error),
    #[error("inconsistent status observation")]
    Inconsistent,
}

/// Validate before handing data to a renderer. Unknown or stale measurements
/// remain absent even if a faulty peer includes retained last-good numbers.
pub fn decode(bytes: &[u8]) -> Result<Status, DecodeError> {
    if bytes.len() > MAX_STATUS_BYTES {
        return Err(DecodeError::TooLarge);
    }
    #[derive(Deserialize)]
    struct Envelope {
        schema_version: u32,
    }
    let envelope: Envelope = serde_json::from_slice(bytes)?;
    if envelope.schema_version != SCHEMA_VERSION {
        return Err(DecodeError::Incompatible(envelope.schema_version));
    }
    let mut status: Status = serde_json::from_slice(bytes)?;
    if status.daemon.name != "vacancyd"
        || status.daemon.version.is_empty()
        || status.ready != status.readiness_failures.is_empty()
        || status
            .upstreams
            .iter()
            .any(|u| u.vacancy != u.refusal.is_none())
    {
        return Err(DecodeError::Inconsistent);
    }
    for resource in &mut status.resources {
        if resource.capacity.quality != Quality::Ready {
            resource.label = None;
            resource.free_mib = None;
            resource.total_mib = None;
            resource.utilization_pct = None;
            resource.lendable = false;
        }
        if let Some(pressure) = &mut resource.pressure {
            if pressure.observation.quality != Quality::Ready {
                pressure.non_lender_pct = None;
                pressure.engaged = None;
                pressure.threshold_pct = None;
                pressure.baseline_pct = None;
                pressure.target_pct = None;
                pressure.learned_samples = None;
                pressure.learned_s = None;
                pressure.calibrated = None;
                pressure.calibration_samples = None;
                pressure.calibration_hint = None;
                pressure.above_for_s = None;
                resource.lendable = false;
            }
        }
        if let Some(mapping) = &mut resource.adapter_mapping {
            if mapping.observation.quality != Quality::Ready {
                mapping.pci = None;
                mapping.luid = None;
                mapping.source = None;
                resource.lendable = false;
            }
        }
    }
    for upstream in &mut status.upstreams {
        if upstream
            .model_observation
            .as_ref()
            .is_none_or(|o| o.quality != Quality::Ready)
        {
            upstream.models.clear();
        }
        if let Some(health) = &mut upstream.health {
            if health.observation.quality != Quality::Ready {
                health.http_status = None;
            }
        }
        if upstream.listener.observation.quality != Quality::Ready {
            upstream.listener.state = "unknown".into();
        }
    }
    // The exempt list is derived from the upstreams' scopes, so a response
    // where they disagree was not built from one configuration.
    let mut exempt: Vec<&str> = status
        .upstreams
        .iter()
        .filter(|upstream| upstream.veto_exempt())
        .map(|upstream| upstream.id.as_str())
        .collect();
    let mut listed: Vec<&str> = status
        .owner_veto
        .exempt_upstreams
        .iter()
        .map(String::as_str)
        .collect();
    exempt.sort_unstable();
    listed.sort_unstable();
    if exempt != listed {
        return Err(DecodeError::Inconsistent);
    }
    if status.upstreams.iter().any(|upstream| {
        upstream.vacancy
            && (!status.lending_enabled
                || (status.owner_veto.active && !upstream.veto_exempt())
                || status
                    .work
                    .iter()
                    .any(|work| work.resources.contains(&upstream.resource))
                || !status.resources.iter().any(|resource| {
                    resource.id == upstream.resource && resource.enabled && resource.lendable
                }))
    }) {
        return Err(DecodeError::Inconsistent);
    }
    Ok(status)
}

/// Held resource ownership, without prompts, credentials or response contents.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Work {
    #[serde(default)]
    pub started_unix_s: u64,
    pub id: u64,
    pub upstream: String,
    pub model: Option<String>,
    pub resources: Vec<String>,
    pub age_s: u64,
    pub completion: String,
    pub phase: String,
    pub tracking_id: Option<String>,
    #[serde(default)]
    pub model_state: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Model {
    pub name: Option<String>,
    pub state: String,
    pub resident_bytes: Option<u64>,
    pub expires_at: Option<String>,
    /// Context length the resident runner was loaded with, where the
    /// upstream reports it (Ollama `/api/ps`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub context_length: Option<u64>,
    /// Digest of the loaded model, where the upstream reports it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub digest: Option<String>,
    /// Context the upstream loads this model with for a request that leaves
    /// the context unset, where the model's own definition fixes it (an
    /// Ollama Modelfile's `num_ctx`). Absent when the upstream's default
    /// decides instead, or while it has not been read yet.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub default_context_length: Option<u64>,
}

/// An upstream URL in the only form it is shown: in this contract, a log
/// line, the command line or an error.
///
/// URL credentials and a query string are replaced with `***` and a fragment
/// is dropped, so a secret written into a URL never reaches a reader of any
/// of those. The daemon refuses such a target when it loads its
/// configuration; this is the second line of defence, for every path that
/// formats a URL. Text that does not parse as a URL with a host is not
/// shown at all, because nothing in it can be shown safely. Control characters and
/// whitespace, which could forge a log line, never survive either.
pub struct RedactedUrl<'a>(pub &'a str);

impl std::fmt::Display for RedactedUrl<'_> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        // Without a host, "scheme:rest" parses with everything after the
        // colon as an opaque path, which could be `user:secret@...`.
        let Some(mut url) = reqwest::Url::parse(self.0)
            .ok()
            .filter(|url| url.host().is_some())
        else {
            return f.write_str("<invalid URL>");
        };
        let mut redacted = false;
        if !url.username().is_empty() || url.password().is_some() {
            // Both setters fail only for a URL that cannot have credentials.
            let _ = url.set_username("***");
            let _ = url.set_password(None);
            redacted = true;
        }
        if url.query().is_some() {
            url.set_query(Some("***"));
            redacted = true;
        }
        if url.fragment().is_some() {
            url.set_fragment(None);
            redacted = true;
        }
        // An untouched URL is shown as written, so `http://host:1` does not
        // gain the trailing slash the parser adds; the parser's form is used
        // only when something was removed or the text needs normalizing.
        if redacted || self.0.chars().any(|c| c.is_control() || c.is_whitespace()) {
            f.write_str(url.as_str())
        } else {
            f.write_str(self.0)
        }
    }
}

impl std::fmt::Debug for RedactedUrl<'_> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{:?}", self.to_string())
    }
}

#[cfg(test)]
mod redacted_url_tests {
    use super::RedactedUrl;

    fn shown(url: &str) -> String {
        RedactedUrl(url).to_string()
    }

    #[test]
    fn a_plain_url_is_shown_as_written() {
        assert_eq!(shown("http://127.0.0.1:11434"), "http://127.0.0.1:11434");
        assert_eq!(shown("http://[::1]:8080/base/"), "http://[::1]:8080/base/");
    }

    #[test]
    fn credentials_query_and_fragment_never_survive() {
        for (url, expected) in [
            (
                "http://demo:fake-secret@127.0.0.1:8000",
                "http://***@127.0.0.1:8000/",
            ),
            (
                "http://fake-secret@127.0.0.1:8000/",
                "http://***@127.0.0.1:8000/",
            ),
            (
                "http://:fake-secret@127.0.0.1:8000/",
                "http://***@127.0.0.1:8000/",
            ),
            (
                "http://demo%3Afake-secret@127.0.0.1/",
                "http://***@127.0.0.1/",
            ),
            (
                "http://127.0.0.1:8000/?api_key=fake-secret",
                "http://127.0.0.1:8000/?***",
            ),
            (
                "http://127.0.0.1:8000/#fake-secret",
                "http://127.0.0.1:8000/",
            ),
            (
                "http://demo:fake-secret@127.0.0.1/p?token=fake-secret#fake-secret",
                "http://***@127.0.0.1/p?***",
            ),
        ] {
            let text = shown(url);
            assert_eq!(text, expected, "{expected}");
            assert!(!text.contains("fake-secret"));
            assert!(!format!("{:?}", RedactedUrl(url)).contains("fake-secret"));
        }
    }

    #[test]
    fn text_that_is_not_a_url_is_not_shown() {
        assert_eq!(shown("demo:fake-secret@@ not a url"), "<invalid URL>");
        assert_eq!(shown("demo:fake-secret"), "<invalid URL>");
        assert_eq!(shown("http://demo:fake-secret@[::1"), "<invalid URL>");
        assert_eq!(shown(""), "<invalid URL>");
    }

    #[test]
    fn line_breaks_cannot_forge_a_log_line() {
        let text = shown("http://127.0.0.1:1/a\nforged line");
        assert!(!text.contains('\n'), "{text}");
    }
}
