//! Shared runtime state: capacity and pressure observations per resource, the
//! host policy veto, runtime switches, and constructed plugin instances.

use std::collections::{HashMap, HashSet};
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, RwLock};
use std::time::{Duration, Instant};

use anyhow::Result;

use crate::config::Config;
use crate::metrics::Metrics;
use crate::plugin::adapter::{Adapter, AdapterRegistry};
use crate::plugin::presence::PresenceSignal;
use crate::plugin::probe::{Capacity, Probe, ProbeRegistry};
use crate::probes::gpu_id::PciAddress;

/// How the resource's vendor identity was selected before its exact Windows
/// LUID association was made.
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize)]
#[serde(rename_all = "snake_case")]
pub enum AdapterMappingSource {
    Config,
    Probe,
}

/// Exact association used to attribute a Windows GPU counter to one resource.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize)]
pub struct AdapterMapping {
    pub pci: PciAddress,
    pub luid: String,
    pub source: AdapterMappingSource,
}

/// Whether a measured input is safe to use for admission.
///
/// `Stale` is computed when the value is read, not when it is written. A
/// collector that stops running therefore cannot leave a resource looking
/// ready forever.
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize)]
#[serde(rename_all = "snake_case")]
pub enum ObservationQuality {
    Unknown,
    Ready,
    Stale,
}

impl ObservationQuality {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Unknown => "unknown",
            Self::Ready => "ready",
            Self::Stale => "stale",
        }
    }
}

/// Latest value and validity of one admission input.
///
/// A failed read invalidates the input immediately but retains the last value
/// and timestamp for diagnostics. Retaining a value must never make it usable:
/// callers may admit only from a [`ObservationView`] whose quality is `Ready`.
#[derive(Debug)]
pub struct Observation<T> {
    value: Option<T>,
    observed_at: Option<Instant>,
    last_error: Option<String>,
}

impl<T> Default for Observation<T> {
    fn default() -> Self {
        Self {
            value: None,
            observed_at: None,
            last_error: None,
        }
    }
}

#[derive(Debug, Clone)]
pub struct ObservationView<T> {
    pub quality: ObservationQuality,
    pub value: Option<T>,
    pub age: Option<Duration>,
    pub last_error: Option<String>,
}

impl<T: Clone> Observation<T> {
    pub fn record_success(&mut self, value: T) {
        self.record_success_at(value, Instant::now());
    }

    pub fn record_success_at(&mut self, value: T, observed_at: Instant) {
        self.value = Some(value);
        self.observed_at = Some(observed_at);
        self.last_error = None;
    }

    pub fn record_failure(&mut self, error: impl Into<String>) {
        self.last_error = Some(error.into());
    }

    /// The failure that currently invalidates this observation, if any.
    pub fn last_error(&self) -> Option<&str> {
        self.last_error.as_deref()
    }

    /// When the current value was sampled, unless a failure has since
    /// invalidated it.
    pub fn valid_since(&self) -> Option<Instant> {
        self.observed_at
            .filter(|_| self.last_error.is_none() && self.value.is_some())
    }

    pub fn sampled_after(&self, boundary: Instant) -> bool {
        self.last_error.is_none() && self.observed_at.is_some_and(|at| at > boundary)
    }

    pub fn view(&self, fresh_for: Duration) -> ObservationView<T> {
        let age = self.observed_at.map(|at| at.elapsed());
        let quality = if self.value.is_none() || self.last_error.is_some() {
            ObservationQuality::Unknown
        } else if age.is_some_and(|age| age > fresh_for) {
            ObservationQuality::Stale
        } else {
            ObservationQuality::Ready
        };
        ObservationView {
            quality,
            value: self.value.clone(),
            age,
            last_error: self.last_error.clone(),
        }
    }
}

pub struct ResourceState {
    /// The probe instance for this resource. `Mutex` because sampling takes
    /// `&mut self` and the loop is the only caller.
    pub probe: Mutex<Box<dyn Probe>>,
    pub probe_kind: &'static str,
    pub capacity: RwLock<Observation<Capacity>>,
    /// Kind-specific reserve verdict computed with the capacity sample. This
    /// keeps request admission from waiting on a slow probe's mutex.
    pub probe_lendable: AtomicBool,
    /// Attributed pressure is a separate admission input from capacity. It is
    /// populated only for resource kinds with a configured pressure backend.
    pub pressure: RwLock<Observation<crate::plugin::pressure::PressureVerdict>>,
    /// Recent pressure samples and decisions, for the diagnostics export.
    pub pressure_history: Mutex<crate::pressure_history::PressureHistory>,
    /// Reclaim edges for this resource; unrelated resources have independent
    /// channels, so their traffic cannot cancel this resource's borrowers.
    pub reclaims: tokio::sync::broadcast::Sender<tokio::time::Instant>,
    /// Stable PCI address joined to the Windows counter LUID. This remains a
    /// separate readiness input so identity failures are visible rather than
    /// disguised as zero pressure.
    pub adapter_mapping: RwLock<Observation<AdapterMapping>>,
    /// Runtime switch, initialised from config, flipped via the admin API.
    pub enabled: AtomicBool,
}

/// The owner veto: engaged the moment presence is detected, released only
/// after `release_after_s` of continuous absence.
#[derive(Debug)]
pub struct Veto {
    inner: Mutex<VetoInner>,
    returns: tokio::sync::broadcast::Sender<tokio::time::Instant>,
}

impl Default for Veto {
    fn default() -> Self {
        Self {
            inner: Mutex::default(),
            returns: tokio::sync::broadcast::channel(64).0,
        }
    }
}

#[derive(Debug, Default)]
struct VetoInner {
    active: bool,
    cause: Option<String>,
    last_match: Option<Instant>,
    /// When the latest rising edge was published.
    returned_at: Option<tokio::time::Instant>,
}

impl Veto {
    pub fn subscribe(&self) -> tokio::sync::broadcast::Receiver<tokio::time::Instant> {
        self.returns.subscribe()
    }
    /// Record one observation. Returns `true` on the *rising edge* — the
    /// moment to hand the hardware back.
    pub fn observe(&self, matched: Option<String>, release_after_s: u64) -> bool {
        let mut v = self.inner.lock().unwrap();
        match matched {
            Some(cause) => {
                let rising = !v.active;
                v.active = true;
                v.cause = Some(cause);
                v.last_match = Some(Instant::now());
                if rising {
                    // Publish while holding the veto lock: admission that sees
                    // the rising edge can never precede its cancellation event.
                    let at = tokio::time::Instant::now();
                    v.returned_at = Some(at);
                    let _ = self.returns.send(at);
                }
                rising
            }
            None => {
                if v.active
                    && v.last_match
                        .map(|t| t.elapsed().as_secs() >= release_after_s)
                        .unwrap_or(true)
                {
                    v.active = false;
                    v.cause = None;
                }
                false
            }
        }
    }

    pub fn while_clear<T>(&self, action: impl FnOnce() -> T) -> Option<T> {
        let veto = self.inner.lock().unwrap();
        if veto.active {
            None
        } else {
            Some(action())
        }
    }

    /// When the latest rising edge was published, as subscribers received it.
    pub fn returned_at(&self) -> Option<tokio::time::Instant> {
        self.inner.lock().unwrap().returned_at
    }

    pub fn status(&self) -> (bool, Option<String>) {
        let v = self.inner.lock().unwrap();
        (v.active, v.cause.clone())
    }
}

pub struct UpstreamRuntime {
    pub adapter: Box<dyn Adapter>,
    pub health: RwLock<Observation<u16>>,
    pub models: RwLock<Observation<crate::lifecycle::Models>>,
    pub listener: RwLock<Observation<crate::probes::listeners::ListenerState>>,
    /// The tracked GPUs whose pressure may be counting this upstream's
    /// borrowed work as owner load, because its work ran while no process
    /// serving its target used a GPU. Empty until the pressure engine sees
    /// that; see `collect::unseen_work`.
    pub unseen_gpu_work: RwLock<Vec<String>>,
    /// Open WebSocket tunnels. An upgraded connection leaves the listener's
    /// connection limit, so tunnels are capped here instead.
    pub tunnels: Arc<tokio::sync::Semaphore>,
    /// Request-body memory this upstream's proxy may hold, in KiB
    /// (`server.limits.max_buffered_body_mib`). A buffered body holds its
    /// share until it has been forwarded.
    pub body_budget: Arc<tokio::sync::Semaphore>,
    /// Forwards borrowed requests to this upstream.
    pub http: reqwest::Client,
    /// vacancyd's own calls to this upstream: model observation, completion
    /// tracking, cancellation and unload. Separate from [`Self::http`] so
    /// they never queue behind borrowed traffic.
    pub observer_http: reqwest::Client,
    /// The `Authorization` value vacancyd presents to this upstream, marked
    /// sensitive. Both clients send it by default; the WebSocket tunnel,
    /// which does not use them, adds it itself.
    pub credential: Option<reqwest::header::HeaderValue>,
    /// The latest owner-return or pressure reclaim of this upstream and
    /// whether it is still waiting, stood down or went ahead. Proxied work
    /// waiting out a drain follows it, so a stream is cut exactly when the
    /// reclaim proceeds.
    pub reclaim_phase: tokio::sync::watch::Sender<Option<crate::yield_control::ReclaimPhase>>,
}

#[derive(Debug, Clone)]
pub struct AgentObservation {
    pub report: crate::agent::AgentReport,
    pub sampled_at: Instant,
    pub expires_at: Instant,
}

#[derive(Debug, Default)]
pub struct AgentSession {
    #[cfg_attr(not(windows), allow(dead_code))]
    pub connection_generation: u64,
    pub observation: Option<AgentObservation>,
}

#[derive(Debug, Clone)]
pub struct SessionEvidence {
    pub authority: crate::plugin::presence::SessionAuthority,
    pub observations: Vec<Option<AgentObservation>>,
}

pub struct AppState {
    pub config: Config,
    /// The protected source of record for next-start settings. Runtime state
    /// continues to use `config` until the service is restarted.
    pub config_path: PathBuf,
    /// The revision (content hash) of the config this running process
    /// actually loaded, captured once at startup. Compared against a fresh
    /// read of `config_path` for `/status`'s `config_revision`, so the
    /// console can tell the owner a save is waiting on a restart. `None`
    /// only if the file could not be read at startup, which `Config::load`
    /// having already succeeded makes very unlikely.
    pub active_config_revision: Option<String>,
    pub settings_write: Mutex<()>,
    pub admission: Mutex<crate::admission::Ledger>,
    pub resources: HashMap<String, ResourceState>,
    pub upstreams: HashMap<String, UpstreamRuntime>,
    pub veto: Veto,
    pub presence: RwLock<PresenceSignal>,
    /// Latest verified report per Windows session. A report expires from its
    /// sample time, so neither arrival order nor buffered input selects a
    /// different user's state or revives an old observation.
    pub agent_reports: RwLock<HashMap<u32, AgentSession>>,
    #[cfg_attr(not(windows), allow(dead_code))]
    pub next_agent_generation: std::sync::atomic::AtomicU64,
    pub agent_channel_listening: AtomicBool,
    /// The local admin channel (the Windows admin pipe or the Linux admin
    /// socket) is accepting connections.
    #[cfg_attr(not(any(windows, target_os = "linux")), allow(dead_code))]
    pub admin_channel_listening: AtomicBool,
    pub last_agent_rejection: Mutex<Option<(crate::agent::AgentRejectReason, Instant)>>,
    /// Which presence backend is in use, for /status. Set once by the
    /// collector on its first poll.
    pub presence_kind: std::sync::OnceLock<&'static str>,
    /// When the sample now in [`Self::presence`] was started. A stalled
    /// sampler leaves the last reading in place, so host-scoped admission
    /// judges its age rather than trusting it indefinitely.
    pub presence_sampled_at: RwLock<Option<Instant>>,
    /// Proxy listeners that have successfully bound. The API listener is
    /// implicit when `/readyz` is being served.
    pub ready_upstreams: RwLock<HashSet<String>>,
    /// Last (session, veto) pair written to the log, so transitions are
    /// logged once rather than every poll.
    pub last_logged_state: Mutex<Option<(crate::plugin::presence::SessionState, bool)>>,
    /// Global lending switch. Off = every gate refuses. Changed only through
    /// `api::change_lending`, together with [`Self::paused_by`].
    pub lending_enabled: AtomicBool,
    /// Who paused lending while it is off, which decides who may resume it.
    pub paused_by: Mutex<Option<vacancyd::local_admin::PausedBy>>,
    /// How far a TCP loopback caller is trusted with admin actions.
    pub local_admin_policy: vacancyd::local_admin::Policy,
    /// Wakes the capacity loop early when a job ends, so the post-job sample
    /// the next admission needs does not wait for the poll interval.
    pub capacity_wake: tokio::sync::Notify,
    pub metrics: Metrics,
    pub started: Instant,
    /// Resolved bearer credentials. Built at startup so a missing env var or
    /// unreadable secret file stops the daemon rather than quietly leaving a
    /// client unable to authenticate.
    pub auth: crate::auth::Authenticator,
    /// The process log filter: configured level and any debug window.
    pub log: Arc<crate::log_control::LogControl>,
    /// Recent reclaims and their causes, for the diagnostics export.
    pub reclaim_log: Mutex<crate::reclaim_log::ReclaimLog>,
}

/// How long to wait for a TCP connection to an upstream. The upstream is
/// normally on the same machine, so this only bounds a dead or filtered
/// target; it never limits how long a response may take.
const UPSTREAM_CONNECT_TIMEOUT: Duration = Duration::from_secs(10);

/// A client for one upstream.
///
/// It is a reverse proxy's client, so it passes an upstream redirect back to
/// the borrower rather than following it on the borrower's behalf, and it
/// ignores `HTTP_PROXY`-style environment settings: a system proxy meant for
/// web browsing must not receive borrowed traffic bound for a local service.
/// An upstream credential is a default header, which a request's own
/// `Authorization` would replace; configuration refuses a credential together
/// with `forward_authorization`, so the proxy never adds one.
pub(crate) fn upstream_client(
    credential: Option<&reqwest::header::HeaderValue>,
) -> reqwest::Result<reqwest::Client> {
    let mut headers = reqwest::header::HeaderMap::new();
    if let Some(value) = credential {
        headers.insert(reqwest::header::AUTHORIZATION, value.clone());
    }
    reqwest::Client::builder()
        .redirect(reqwest::redirect::Policy::none())
        .no_proxy()
        .connect_timeout(UPSTREAM_CONNECT_TIMEOUT)
        .default_headers(headers)
        .build()
}

impl AppState {
    /// Build runtime state, constructing every plugin named in the config.
    /// Fails loudly on an unknown `kind` rather than silently skipping it.
    #[cfg(test)]
    pub fn new(config: Config) -> Result<Self> {
        Self::new_with_config_path(config, std::path::Path::new("vacancyd.toml"))
    }

    pub fn new_with_config_path(config: Config, config_path: impl Into<PathBuf>) -> Result<Self> {
        let config_path = config_path.into();
        let active_config_revision = std::fs::read(&config_path)
            .ok()
            .map(|bytes| crate::api::revision(&bytes));
        let probes = ProbeRegistry::with_builtins();
        let adapters = AdapterRegistry::with_builtins();

        let mut resources = HashMap::new();
        for rc in &config.resources {
            let probe = probes.build(rc)?;
            let probe_kind = probe.kind();
            resources.insert(
                rc.id.clone(),
                ResourceState {
                    probe: Mutex::new(probe),
                    probe_kind,
                    capacity: RwLock::new(Observation::default()),
                    // Capacity starts unknown, so this value cannot authorize
                    // admission until the first successful sample replaces it.
                    probe_lendable: AtomicBool::new(true),
                    pressure: RwLock::new(Observation::default()),
                    pressure_history: Mutex::new(crate::pressure_history::PressureHistory::new(
                        crate::pressure_history::capacity_for(config.owner.gpu_pressure.poll_ms),
                    )),
                    reclaims: tokio::sync::broadcast::channel(64).0,
                    adapter_mapping: RwLock::new(Observation::default()),
                    enabled: AtomicBool::new(rc.enabled),
                },
            );
        }

        let crate::config::Credentials {
            auth,
            upstreams: mut credentials,
        } = config.resolve_credentials()?;
        let mut upstreams = HashMap::new();
        for uc in &config.upstreams {
            let credential = credentials.remove(&uc.id);
            upstreams.insert(
                uc.id.clone(),
                UpstreamRuntime {
                    adapter: adapters.build(&uc.kind)?,
                    health: RwLock::new(Observation::default()),
                    models: RwLock::new(Observation::default()),
                    listener: RwLock::new(Observation::default()),
                    unseen_gpu_work: RwLock::new(Vec::new()),
                    tunnels: Arc::new(tokio::sync::Semaphore::new(
                        config.server.limits.max_connections,
                    )),
                    body_budget: Arc::new(tokio::sync::Semaphore::new(
                        config.server.limits.buffered_body_kib(),
                    )),
                    http: upstream_client(credential.as_ref())?,
                    observer_http: upstream_client(credential.as_ref())?,
                    credential,
                    reclaim_phase: tokio::sync::watch::Sender::new(None),
                },
            );
        }

        let metrics = Metrics::new();
        for upstream in &config.upstreams {
            metrics
                .upstream_owner_veto_exempt
                .with_label_values(&[upstream.id.as_str()])
                .set((upstream.owner_scope == crate::config::OwnerScope::Resource) as i64);
            for phase in ["forwarding", "tracking", "held"] {
                metrics
                    .active_work
                    .with_label_values(&[upstream.id.as_str(), phase])
                    .set(0);
            }
        }

        Ok(Self {
            config_path,
            active_config_revision,
            settings_write: Mutex::new(()),
            admission: Mutex::default(),
            resources,
            upstreams,
            veto: Veto::default(),
            presence: RwLock::new(PresenceSignal::default()),
            agent_reports: RwLock::new(HashMap::new()),
            next_agent_generation: std::sync::atomic::AtomicU64::new(1),
            agent_channel_listening: AtomicBool::new(false),
            admin_channel_listening: AtomicBool::new(false),
            last_agent_rejection: Mutex::new(None),
            presence_kind: std::sync::OnceLock::new(),
            presence_sampled_at: RwLock::new(None),
            ready_upstreams: RwLock::new(HashSet::new()),
            last_logged_state: Mutex::new(None),
            lending_enabled: AtomicBool::new(true),
            paused_by: Mutex::new(None),
            local_admin_policy: vacancyd::local_admin::Policy::for_this_platform(),
            capacity_wake: tokio::sync::Notify::new(),
            metrics,
            started: Instant::now(),
            auth,
            log: crate::log_control::installed(),
            reclaim_log: Mutex::default(),
            config,
        })
    }

    pub fn resource_enabled(&self, id: &str) -> bool {
        self.resources
            .get(id)
            .map(|r| r.enabled.load(Ordering::Relaxed))
            .unwrap_or(false)
    }

    #[cfg_attr(not(windows), allow(dead_code))]
    pub fn agent_report_fresh_for(&self) -> Duration {
        Duration::from_millis(self.config.poll.interval_ms.saturating_mul(4).max(15_000))
    }

    /// How long a host-presence sample may be used: four presence polls and
    /// at least 15 s, the same bound as a Windows session report. That covers
    /// a poll whose subprocesses run to their five-second deadlines.
    pub fn presence_fresh_for(&self) -> Duration {
        Duration::from_millis(self.config.poll.interval_ms.saturating_mul(4).max(15_000))
    }

    /// The age of the current host-presence sample when it is too old to use,
    /// or `Some(None)` when it has no sample time. `None` when it is fresh.
    pub fn stale_presence(&self) -> Option<Option<Duration>> {
        match *self.presence_sampled_at.read().unwrap() {
            Some(at) if at.elapsed() <= self.presence_fresh_for() => None,
            Some(at) => Some(Some(at.elapsed())),
            None => Some(None),
        }
    }

    /// Record a presence backend and a sample taken now, as the presence
    /// loop does after its first poll.
    #[cfg(test)]
    pub fn mark_presence_sampled(&self) {
        self.presence_kind.set("test").unwrap();
        *self.presence_sampled_at.write().unwrap() = Some(Instant::now());
    }

    #[cfg_attr(not(windows), allow(dead_code))]
    pub fn begin_agent_connection(&self) -> u64 {
        self.next_agent_generation.fetch_add(1, Ordering::Relaxed)
    }

    #[cfg_attr(not(windows), allow(dead_code))]
    pub fn record_agent_report(
        &self,
        report: crate::agent::AgentReport,
        connection_generation: u64,
        valid_for: Duration,
        sample_age: Duration,
        received_at: Instant,
    ) -> bool {
        let mut reports = self.agent_reports.write().unwrap();
        if reports.get(&report.session_id).is_some_and(|session| {
            session.connection_generation > connection_generation
                || (session.connection_generation == connection_generation
                    && session
                        .observation
                        .as_ref()
                        .is_some_and(|current| report.sequence <= current.report.sequence))
        }) {
            return false;
        }
        let observation = AgentObservation {
            report,
            // Wire timestamps have millisecond precision. Subtract one more
            // millisecond so rounding can only reject a boundary sample, not
            // move a pre-transition sample across the authority cutoff.
            sampled_at: received_at
                .checked_sub(sample_age.saturating_add(Duration::from_millis(1)))
                .unwrap_or(received_at),
            expires_at: received_at + valid_for,
        };
        reports.insert(
            observation.report.session_id,
            AgentSession {
                connection_generation,
                observation: Some(observation),
            },
        );
        true
    }

    #[cfg_attr(not(windows), allow(dead_code))]
    pub fn end_agent_connection(&self, session_id: u32, connection_generation: u64) {
        let mut reports = self.agent_reports.write().unwrap();
        if let Some(session) = reports.get_mut(&session_id) {
            if session.connection_generation == connection_generation {
                // Retain the generation tombstone so a superseded connection
                // cannot publish again after its replacement disconnects.
                session.observation = None;
            }
        }
    }

    #[cfg(test)]
    pub fn agent_report_for_session(&self, session_id: u32) -> Option<AgentObservation> {
        self.agent_reports
            .read()
            .unwrap()
            .get(&session_id)
            .and_then(|session| session.observation.as_ref())
            .filter(|observation| observation.expires_at > Instant::now())
            .cloned()
    }

    /// Read one coherent snapshot for every active session requirement.
    ///
    /// A report received before the current logon/unlock epoch cannot make a
    /// newly active session look idle, even if its ordinary TTL has not yet
    /// elapsed.
    pub fn agent_reports_for_requirements(
        &self,
        requirements: &[crate::plugin::presence::SessionRequirement],
    ) -> Vec<Option<AgentObservation>> {
        let now = Instant::now();
        let reports = self.agent_reports.read().unwrap();
        requirements
            .iter()
            .map(|requirement| {
                reports
                    .get(&requirement.session_id)
                    .and_then(|session| session.observation.as_ref())
                    .filter(|observation| {
                        observation.expires_at > now
                            && observation.sampled_at >= requirement.fresh_after
                    })
                    .cloned()
            })
            .collect()
    }

    /// Evaluate the published WTS topology against current report state.
    /// Admission and `/status` both use this so a disconnect or expiry takes
    /// effect immediately, without waiting for the next presence poll.
    ///
    /// An active session published as `NotRequired` with no reporter
    /// requirements comes from a backend whose own reading is complete
    /// (systemd-logind); it stays `NotRequired`, and the owner check alone
    /// decides it. Windows never publishes that combination.
    ///
    /// An unknown session a backend published as `Ambiguous` (unreadable,
    /// inconsistent or not yet confirmed) stays `Ambiguous`: it is the same
    /// missing evidence as an ambiguous active session, not a platform
    /// without a presence backend.
    pub fn session_evidence(
        &self,
        presence: &crate::plugin::presence::PresenceSignal,
    ) -> SessionEvidence {
        use crate::plugin::presence::{SessionAuthority, SessionState};

        let observations = self.agent_reports_for_requirements(&presence.session_requirements);
        let authority = if presence.session == SessionState::Unknown
            && presence.session_authority == SessionAuthority::Ambiguous
        {
            SessionAuthority::Ambiguous
        } else if presence.session != SessionState::Active
            || (presence.session_authority == SessionAuthority::NotRequired
                && presence.session_requirements.is_empty())
        {
            SessionAuthority::NotRequired
        } else if presence.session_authority == SessionAuthority::Ambiguous
            || presence.session_requirements.is_empty()
        {
            SessionAuthority::Ambiguous
        } else if observations.iter().all(Option::is_some) {
            SessionAuthority::Ready
        } else {
            SessionAuthority::Missing
        };
        SessionEvidence {
            authority,
            observations,
        }
    }

    #[cfg_attr(not(windows), allow(dead_code))]
    pub fn record_agent_rejection(&self, reason: crate::agent::AgentRejectReason) {
        self.metrics
            .agent_rejections_total
            .with_label_values(&[reason.as_str()])
            .inc();
        *self.last_agent_rejection.lock().unwrap() = Some((reason, Instant::now()));
    }

    pub fn last_agent_rejection(&self) -> Option<(crate::agent::AgentRejectReason, Duration)> {
        self.last_agent_rejection
            .lock()
            .unwrap()
            .as_ref()
            .map(|(reason, at)| (*reason, at.elapsed()))
    }

    /// How long a capacity observation remains admissible. This is deliberately
    /// internal policy rather than public configuration: enough for one missed
    /// poll, with a floor that avoids flapping at short cadences.
    pub fn capacity_fresh_for(&self) -> Duration {
        Duration::from_millis(self.config.poll.interval_ms.saturating_mul(2).max(15_000))
    }

    pub fn capacity_view(&self, id: &str) -> Option<ObservationView<Capacity>> {
        self.resources
            .get(id)
            .map(|r| r.capacity.read().unwrap().view(self.capacity_fresh_for()))
    }

    pub fn pressure_fresh_for(&self) -> Duration {
        Duration::from_millis(
            self.config
                .owner
                .gpu_pressure
                .poll_ms
                .saturating_mul(2)
                .max(15_000),
        )
    }

    pub fn pressure_view(
        &self,
        id: &str,
    ) -> Option<ObservationView<crate::plugin::pressure::PressureVerdict>> {
        self.resources
            .get(id)
            .map(|r| r.pressure.read().unwrap().view(self.pressure_fresh_for()))
    }

    /// Adapter identity is refreshed on a slower discovery cadence than
    /// pressure. Two missed minute-scale refreshes make it stale.
    pub fn adapter_mapping_view(&self, id: &str) -> Option<ObservationView<AdapterMapping>> {
        self.resources.get(id).map(|r| {
            r.adapter_mapping
                .read()
                .unwrap()
                .view(Duration::from_secs(120))
        })
    }

    /// Reasons this process is not yet safe to advertise as ready.
    pub fn readiness_failures(&self) -> Vec<String> {
        let mut failures = Vec::new();
        if self.presence_kind.get().is_none() {
            failures.push("presence has not been sampled".to_string());
        } else if self.stale_presence().is_some()
            && self
                .config
                .upstreams
                .iter()
                .any(|upstream| upstream.owner_scope == crate::config::OwnerScope::Host)
        {
            failures.push("presence reading is out of date".to_string());
        }

        let ready_upstreams = self.ready_upstreams.read().unwrap();
        for upstream in &self.config.upstreams {
            if !ready_upstreams.contains(&upstream.id) {
                failures.push(format!("upstream '{}' is not listening", upstream.id));
            }
        }
        drop(ready_upstreams);

        let required: HashSet<&str> = self
            .config
            .upstreams
            .iter()
            .flat_map(|upstream| {
                std::iter::once(upstream.resource.as_str())
                    .chain(upstream.also_requires.iter().map(String::as_str))
            })
            .collect();
        for resource in self.config.resources.iter().filter(|resource| {
            self.resource_enabled(&resource.id) && required.contains(resource.id.as_str())
        }) {
            if self
                .capacity_view(&resource.id)
                .is_none_or(|view| view.quality != ObservationQuality::Ready)
            {
                failures.push(format!("resource '{}' capacity is not ready", resource.id));
            }
            if resource.kind == "nvidia-gpu" && self.config.owner.gpu_pressure.enabled {
                if self
                    .adapter_mapping_view(&resource.id)
                    .is_none_or(|view| view.quality != ObservationQuality::Ready)
                {
                    failures.push(format!(
                        "resource '{}' adapter mapping is not ready",
                        resource.id
                    ));
                }
                if self.pressure_view(&resource.id).is_none_or(|view| {
                    view.quality != ObservationQuality::Ready
                        || view.value.as_ref().is_none_or(|v| !v.calibrated)
                }) {
                    failures.push(format!("resource '{}' pressure is not ready", resource.id));
                }
            }
        }
        failures
    }
}

#[cfg(test)]
mod observation_tests {
    use super::*;

    fn state() -> AppState {
        let config = toml::from_str(
            r#"
            [server]
            listen = "127.0.0.1:9811"
            "#,
        )
        .unwrap();
        AppState::new(config).unwrap()
    }

    fn report(session_id: u32, sequence: u64) -> crate::agent::AgentReport {
        crate::agent::AgentReport {
            protocol_version: crate::agent::AGENT_PROTOCOL_VERSION,
            sequence,
            session_id,
            idle_seconds: 30,
            foreground: None,
            agent_version: env!("CARGO_PKG_VERSION").into(),
            sampled_at_unix_ms: crate::agent::unix_time_ms(),
        }
    }

    #[test]
    fn observations_start_unknown_and_become_stale_at_read_time() {
        let mut observation = Observation::default();
        assert_eq!(
            observation.view(Duration::from_secs(15)).quality,
            ObservationQuality::Unknown
        );

        observation.record_success_at(7_u32, Instant::now() - Duration::from_secs(16));
        let view = observation.view(Duration::from_secs(15));
        assert_eq!(view.quality, ObservationQuality::Stale);
        assert_eq!(view.value, Some(7));
    }

    #[test]
    fn a_failed_read_invalidates_but_does_not_erase_diagnostics() {
        let mut observation = Observation::default();
        observation.record_success(7_u32);
        observation.record_failure("probe timed out");

        let view = observation.view(Duration::from_secs(15));
        assert_eq!(view.quality, ObservationQuality::Unknown);
        assert_eq!(view.value, Some(7));
        assert_eq!(view.last_error.as_deref(), Some("probe timed out"));

        observation.record_success(8_u32);
        let recovered = observation.view(Duration::from_secs(15));
        assert_eq!(recovered.quality, ObservationQuality::Ready);
        assert_eq!(recovered.value, Some(8));
        assert!(recovered.last_error.is_none());
    }

    #[test]
    fn agent_reports_are_scoped_by_session_and_connection_generation() {
        let state = state();
        let first = state.begin_agent_connection();
        assert!(state.record_agent_report(
            report(7, 1),
            first,
            Duration::from_secs(15),
            Duration::ZERO,
            Instant::now()
        ));
        assert_eq!(
            state.agent_report_for_session(7).unwrap().report.sequence,
            1
        );
        assert!(state.agent_report_for_session(8).is_none());

        let rejected_replacement = state.begin_agent_connection();
        assert_eq!(
            state.agent_report_for_session(7).unwrap().report.sequence,
            1
        );
        state.end_agent_connection(7, rejected_replacement);
        assert_eq!(
            state.agent_report_for_session(7).unwrap().report.sequence,
            1
        );
        assert!(state.record_agent_report(
            report(7, 2),
            first,
            Duration::from_secs(15),
            Duration::ZERO,
            Instant::now()
        ));
        let replacement = state.begin_agent_connection();
        assert!(state.record_agent_report(
            report(7, 1),
            replacement,
            Duration::from_secs(15),
            Duration::ZERO,
            Instant::now()
        ));
        assert!(!state.record_agent_report(
            report(7, 1),
            replacement,
            Duration::from_secs(15),
            Duration::ZERO,
            Instant::now()
        ));
        assert!(!state.record_agent_report(
            report(7, 3),
            first,
            Duration::from_secs(15),
            Duration::ZERO,
            Instant::now()
        ));
        assert_eq!(
            state.agent_report_for_session(7).unwrap().report.sequence,
            1
        );

        state.end_agent_connection(7, first);
        assert!(state.agent_report_for_session(7).is_some());
        state.end_agent_connection(7, replacement);
        assert!(state.agent_report_for_session(7).is_none());
        assert!(!state.record_agent_report(
            report(7, 3),
            first,
            Duration::from_secs(15),
            Duration::ZERO,
            Instant::now()
        ));
    }

    #[test]
    fn a_buffered_pre_transition_sample_is_not_fresh_for_the_new_epoch() {
        let state = state();
        let generation = state.begin_agent_connection();
        let transition = Instant::now();
        assert!(state.record_agent_report(
            report(7, 1),
            generation,
            Duration::from_secs(10),
            Duration::from_secs(5),
            Instant::now()
        ));
        let requirements = [crate::plugin::presence::SessionRequirement {
            session_id: 7,
            fresh_after: transition,
        }];
        assert!(state.agent_reports_for_requirements(&requirements)[0].is_none());
    }
}
