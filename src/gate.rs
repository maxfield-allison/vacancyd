//! Admission: may this request borrow the hardware right now?
//!
//! The decision is made *before* the upstream ever sees the request, so a
//! refused borrow costs the owner nothing — no allocation, no model load,
//! no page-out of whatever the owner is running. Reactive
//! load-then-fail-then-fallback designs disturb the owner's workload with
//! the failed attempt itself; vacancyd exists to answer first.

use std::sync::atomic::Ordering;
use std::time::Duration;

use serde::Serialize;

use crate::admission::Ledger;
use crate::config::UpstreamConfig;
use crate::plugin::adapter::Residency;
use crate::plugin::probe::Capacity;
use crate::state::{AppState, ObservationQuality};

const MIB: u64 = 1024 * 1024;

/// What a gated request asks for, as far as admission can read it.
#[derive(Debug, Clone, Copy, Default)]
pub struct Demand<'a> {
    /// The model named in the body, when the dialect carries one.
    pub model: Option<&'a str>,
    /// What the request asks of the runner; see `Adapter::runner_request`.
    pub runner: Option<crate::plugin::adapter::RunnerRequest>,
}

impl<'a> Demand<'a> {
    /// A request known only by its model name, which can never prove that
    /// it reuses a resident runner as loaded.
    pub fn model(model: Option<&'a str>) -> Self {
        Self {
            model,
            runner: None,
        }
    }
}

/// A model observation older than this cannot prove residency. It is
/// tighter than the lifecycle freshness because a keep-alive expiry or an
/// eviction can end residency between polls.
pub(crate) const RESIDENT_FRESH_FOR: Duration = Duration::from_secs(4);
/// A model due to unload sooner than this may be gone before the request
/// reaches it, and loading it again would need the full requirement.
const RESIDENT_EXPIRY_MARGIN_MS: i64 = 30_000;

/// Proof that a request can reuse memory the upstream already holds.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Credit {
    /// What was proved, for refusal details, `/status` and logs.
    pub label: String,
    pub kind: CreditKind,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum CreditKind {
    /// The model is loaded as the request needs it.
    Model,
    /// The upstream's allocator holds `mib` on `resource` for the next job.
    Pool { resource: String, mib: u64 },
}

impl Credit {
    /// The MiB a request is charged on `resource` in place of `load_mib`.
    /// A credit never charges more than a cold load would.
    pub(crate) fn charge(&self, resource: &str, load_mib: u64, resident_min_free_mib: u64) -> u64 {
        match &self.kind {
            CreditKind::Model => load_mib.min(resident_min_free_mib),
            CreditKind::Pool { resource: gpu, mib } if gpu == resource => {
                load_mib.saturating_sub(*mib)
            }
            CreditKind::Pool { .. } => load_mib,
        }
    }
}

/// Why a request is charged as a cold load.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum NoCredit {
    /// The adapter has no way to prove residency.
    Unsupported,
    /// Residency was not proved. `refreshable` is true when only the age or
    /// timing of the model observation stood in the way, so a new
    /// observation could change the answer.
    Unproved { reason: String, refreshable: bool },
}

impl NoCredit {
    fn fixed(reason: impl Into<String>) -> Self {
        Self::Unproved {
            reason: reason.into(),
            refreshable: false,
        }
    }

    fn refreshable(reason: impl Into<String>) -> Self {
        Self::Unproved {
            reason: reason.into(),
            refreshable: true,
        }
    }

    pub(crate) fn is_refreshable(&self) -> bool {
        matches!(
            self,
            Self::Unproved {
                refreshable: true,
                ..
            }
        )
    }
}

/// An admitting decision, and the residency credit that shaped it.
#[derive(Debug, Clone, Default)]
pub struct Admission {
    pub(crate) credit: Option<Credit>,
    /// Set when the credit lowered a charge, saying what was charged.
    pub residency: Option<String>,
}

#[derive(Debug, Clone, Serialize)]
pub struct Refusal {
    pub reason: RefusalReason,
    pub detail: String,
    /// Resource responsible for a resource-scoped refusal. Host policy and
    /// the global switch leave this unset.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub resource: Option<String>,
    /// Hint for the caller's retry/backoff, mirrored in `Retry-After`.
    pub retry_after_s: u32,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum RefusalReason {
    LendingDisabled,
    ResourceDisabled,
    ResourceUnmeasured,
    ResourcePressured,
    OwnerActive,
    InsufficientCapacity,
    ResourceBusy,
}

impl RefusalReason {
    pub fn outcome_label(self) -> &'static str {
        match self {
            RefusalReason::LendingDisabled => "refused_disabled",
            RefusalReason::ResourceDisabled => "refused_disabled",
            RefusalReason::ResourceUnmeasured => "refused_unmeasured",
            RefusalReason::ResourcePressured => "refused_pressure",
            RefusalReason::OwnerActive => "refused_owner",
            RefusalReason::InsufficientCapacity => "refused_capacity",
            RefusalReason::ResourceBusy => "refused_busy",
        }
    }
}

/// Is a resource lendable on its own terms, before any upstream/model math?
/// GPUs: governed by upstream thresholds, so any sample qualifies.
/// Memory: `reserve` bytes must stay free for the owner.
/// CPU: load must be at or below `reserve` percent.
pub fn resource_lendable(state: &AppState, rc: &crate::config::ResourceConfig) -> bool {
    checked_resource_capacity(state, rc).is_ok()
}

/// Why the owner's presence refuses `upstream` now, or `None`: presence not
/// yet sampled, incomplete session evidence or the owner veto, the last two
/// only for a host-scoped upstream. Admission applies this first; a drain
/// re-checks it before it stands down.
pub(crate) fn owner_refusal(
    state: &AppState,
    snapshot: &Snapshot,
    upstream: &UpstreamConfig,
) -> Option<Refusal> {
    // Until the presence loop publishes once, `state.presence` and the veto
    // are defaults, not observations: an owner who is at the keyboard when
    // the daemon starts would read as absent. `/readyz` uses the same signal.
    // It is set once and never cleared, so reading it live is consistent
    // with any snapshot taken after it was set.
    if state.presence_kind.get().is_none() {
        return Some(Refusal {
            reason: RefusalReason::OwnerActive,
            detail: "owner presence has not been sampled yet".into(),
            resource: None,
            retry_after_s: 5,
        });
    }
    // A resource-scoped upstream answers only to its own resources: the
    // owner veto and session evidence guard the host, and its pressure
    // checks below still refuse when the owner loads its card.
    let host_scoped = upstream.owner_scope == crate::config::OwnerScope::Host;
    // A sampler that stalls leaves its last reading in place. An "owner
    // away" reading from before the owner came back must not keep lending.
    if host_scoped {
        if let Some(age) = state.stale_presence() {
            return Some(Refusal {
                reason: RefusalReason::OwnerActive,
                detail: match age {
                    Some(age) => format!(
                        "owner presence reading is out of date ({} s old)",
                        age.as_secs()
                    ),
                    None => "owner presence reading is out of date".into(),
                },
                resource: None,
                retry_after_s: 5,
            });
        }
    }
    let evidence = state.session_evidence(&snapshot.presence);
    if host_scoped
        && matches!(
            snapshot.presence.session,
            crate::plugin::presence::SessionState::Active
                | crate::plugin::presence::SessionState::Unknown
        )
        && !matches!(
            evidence.authority,
            crate::plugin::presence::SessionAuthority::Ready
                | crate::plugin::presence::SessionAuthority::NotRequired
        )
    {
        return Some(Refusal {
            reason: RefusalReason::OwnerActive,
            detail: match evidence.authority {
                crate::plugin::presence::SessionAuthority::Missing => {
                    "the owner may be using this machine (session evidence incomplete)".into()
                }
                _ => "the owner may be using this machine (session state unavailable)".into(),
            },
            resource: None,
            retry_after_s: 30,
        });
    }
    if host_scoped && snapshot.veto {
        return Some(Refusal {
            reason: RefusalReason::OwnerActive,
            detail: format!(
                "lending is paused by owner policy ({})",
                snapshot.veto_cause.as_deref().unwrap_or("session active")
            ),
            resource: None,
            retry_after_s: 120,
        });
    }
    None
}

/// Validate the runtime switch, freshness, and kind-specific reserve for one
/// resource, returning only a capacity value that is safe to use.
fn checked_resource_capacity(
    state: &AppState,
    rc: &crate::config::ResourceConfig,
) -> Result<Capacity, Refusal> {
    let Some(rs) = state.resources.get(&rc.id) else {
        return Err(Refusal {
            reason: RefusalReason::ResourceUnmeasured,
            detail: format!("resource '{}' has no runtime state", rc.id),
            resource: Some(rc.id.clone()),
            retry_after_s: 30,
        });
    };
    if !rs.enabled.load(Ordering::Relaxed) {
        return Err(Refusal {
            reason: RefusalReason::ResourceDisabled,
            detail: format!("resource '{}' is not lending", rc.id),
            resource: Some(rc.id.clone()),
            retry_after_s: 300,
        });
    }

    // Hold the observation read lock through the cached reserve verdict read.
    // The collector writes both while holding the corresponding write lock,
    // so an admission cannot combine one sample's capacity with another
    // sample's verdict.
    let (observation, probe_lendable) = {
        let capacity = rs.capacity.read().unwrap();
        let observation = capacity.view(state.capacity_fresh_for());
        let probe_lendable = rs.probe_lendable.load(Ordering::Relaxed);
        (observation, probe_lendable)
    };
    let capacity = match observation.quality {
        ObservationQuality::Ready => observation
            .value
            .expect("a ready observation always carries a value"),
        ObservationQuality::Unknown => {
            let suffix = observation
                .last_error
                .as_deref()
                .map(|error| format!(": {error}"))
                .unwrap_or_default();
            return Err(Refusal {
                reason: RefusalReason::ResourceUnmeasured,
                detail: format!("resource '{}' capacity is unknown{suffix}", rc.id),
                resource: Some(rc.id.clone()),
                retry_after_s: 30,
            });
        }
        ObservationQuality::Stale => {
            let age_s = observation.age.map(|age| age.as_secs()).unwrap_or(0);
            return Err(Refusal {
                reason: RefusalReason::ResourceUnmeasured,
                detail: format!(
                    "resource '{}' capacity sample is stale ({age_s}s old)",
                    rc.id
                ),
                resource: Some(rc.id.clone()),
                retry_after_s: 30,
            });
        }
    };

    // The probe owns its own lendability rule, computed alongside the sample
    // and cached so admission never waits on a slow probe's mutex. This check
    // applies to the primary resource as well as additional dependencies.
    if !probe_lendable {
        return Err(Refusal {
            reason: RefusalReason::InsufficientCapacity,
            detail: format!("resource '{}' is not currently lendable", rc.id),
            resource: Some(rc.id.clone()),
            retry_after_s: 30,
        });
    }

    if let Some(refusal) = pressure_refusal(state, rc) {
        return Err(refusal);
    }
    Ok(capacity)
}

/// Whether attributed GPU pressure decides admission to `rc`.
pub(crate) fn pressure_gated(state: &AppState, rc: &crate::config::ResourceConfig) -> bool {
    rc.kind == "nvidia-gpu" && state.config.owner.gpu_pressure.enabled
}

/// Why GPU pressure refuses work on `rc` now, or `None` when it admits it or
/// the resource is not pressure-gated. Admission applies this to every
/// resource an upstream needs; a drain uses it to see whether the pressure
/// refusal that started a reclaim has cleared.
pub(crate) fn pressure_refusal(
    state: &AppState,
    rc: &crate::config::ResourceConfig,
) -> Option<Refusal> {
    if !pressure_gated(state, rc) {
        return None;
    }
    let observation = state
        .pressure_view(&rc.id)
        .expect("every configured resource has runtime state");
    let verdict = match observation.quality {
        ObservationQuality::Ready => observation
            .value
            .expect("a ready pressure observation always carries a value"),
        ObservationQuality::Unknown => {
            let suffix = observation
                .last_error
                .as_deref()
                .map(|error| format!(": {error}"))
                .unwrap_or_default();
            return Some(Refusal {
                reason: RefusalReason::ResourceUnmeasured,
                detail: format!("resource '{}' pressure is unknown{suffix}", rc.id),
                resource: Some(rc.id.clone()),
                retry_after_s: 30,
            });
        }
        ObservationQuality::Stale => {
            let age_s = observation.age.map(|age| age.as_secs()).unwrap_or(0);
            return Some(Refusal {
                reason: RefusalReason::ResourceUnmeasured,
                detail: format!(
                    "resource '{}' pressure sample is stale ({age_s}s old)",
                    rc.id
                ),
                resource: Some(rc.id.clone()),
                retry_after_s: 30,
            });
        }
    };
    if !verdict.calibrated {
        return Some(Refusal {
            reason: RefusalReason::ResourceUnmeasured,
            detail: format!(
                "resource '{}' pressure calibration is not ready ({}/{}): {}",
                rc.id,
                verdict.calibration_samples,
                verdict.calibration_required,
                verdict
                    .calibration_hint
                    .as_deref()
                    .unwrap_or("waiting for calibration")
            ),
            resource: Some(rc.id.clone()),
            retry_after_s: 30,
        });
    }
    if verdict.engaged {
        return Some(Refusal {
            reason: RefusalReason::ResourcePressured,
            detail: format!(
                "resource '{}' has {:.1}% non-lender pressure (threshold {:.1}%)",
                rc.id, verdict.non_lender_pct, verdict.threshold_pct
            ),
            resource: Some(rc.id.clone()),
            retry_after_s: 30,
        });
    }
    None
}

/// The configured requirement for `name`. An exact key wins; otherwise a key
/// the upstream's dialect resolves to the same model applies, so
/// `registry.ollama.ai/library/gpt-oss:20b` is budgeted as `gpt-oss:20b`
/// rather than falling through to the default.
pub(crate) fn model_requirement<'a>(
    state: &AppState,
    upstream: &'a UpstreamConfig,
    name: &str,
) -> Option<&'a crate::config::ModelRequirement> {
    if let Some(requirement) = upstream.models.get(name) {
        return Some(requirement);
    }
    let adapter = &state.upstreams.get(&upstream.id)?.adapter;
    let wanted = adapter.canonical_model(name);
    upstream
        .models
        .iter()
        .find(|(key, _)| adapter.canonical_model(key) == wanted)
        .map(|(_, requirement)| requirement)
}

/// A single, consistent read of owner/lending state, taken once so a
/// multi-route decision built from one status snapshot never mixes a veto
/// read from one instant with a lending-switch or presence read from
/// another. Building the status response this way previously let the veto
/// clear (or lending toggle) mid-build, producing a combination the decoder
/// correctly rejects as inconsistent even though nothing was actually
/// wrong. Each real inference request still takes its own fresh snapshot
/// immediately before deciding, so live enforcement is unchanged.
pub struct Snapshot {
    pub lending_enabled: bool,
    pub veto: bool,
    pub veto_cause: Option<String>,
    pub presence: crate::plugin::presence::PresenceSignal,
}

impl Snapshot {
    pub fn read(state: &AppState) -> Self {
        let (veto, veto_cause) = state.veto.status();
        Self {
            lending_enabled: state.lending_enabled.load(Ordering::Relaxed),
            veto,
            veto_cause,
            presence: state.presence.read().unwrap().clone(),
        }
    }
}

/// Decide admission for one inference request. `model` is the model name
/// extracted from the request body, when the dialect exposes one. Reads
/// current owner/lending state itself; call `admit_with` instead when
/// deciding several routes from one already-read `Snapshot`.
///
/// Production code always has a `Snapshot` already in hand (the status
/// endpoint) or reads one immediately before its one decision (the proxy's
/// admission path), so this convenience wrapper is test-only; a caller
/// outside tests should call `admit_with(state, &Snapshot::read(state), ..)`
/// explicitly at its own call site instead of gaining a hidden second read.
#[cfg(test)]
pub(crate) fn admit(
    state: &AppState,
    upstream: &UpstreamConfig,
    model: Option<&str>,
) -> Result<Admission, Refusal> {
    admit_with(state, &Snapshot::read(state), upstream, model)
}

/// As `admit`, but against an already-read `Snapshot` instead of a fresh
/// live read. Use this to build several routes' decisions (for example the
/// status endpoint's per-upstream loop) from one consistent read.
#[cfg(test)]
pub fn admit_with(
    state: &AppState,
    snapshot: &Snapshot,
    upstream: &UpstreamConfig,
    model: Option<&str>,
) -> Result<Admission, Refusal> {
    let ledger = state.admission.lock().unwrap();
    let effective = ledger.effective_upstream(upstream);
    let upstream = &effective;
    let admission = check(state, &ledger, snapshot, upstream, Demand::model(model))?;
    ledger.check(state, upstream)?;
    Ok(admission)
}

pub(crate) fn check(
    state: &AppState,
    ledger: &Ledger,
    snapshot: &Snapshot,
    upstream: &UpstreamConfig,
    demand: Demand<'_>,
) -> Result<Admission, Refusal> {
    let model = demand.model;
    if !snapshot.lending_enabled {
        return Err(Refusal {
            reason: RefusalReason::LendingDisabled,
            detail: "lending is switched off on this host".into(),
            resource: None,
            retry_after_s: 300,
        });
    }
    if let Some(refusal) = owner_refusal(state, snapshot, upstream) {
        return Err(refusal);
    }

    // A model the upstream already holds is charged its resident
    // requirement instead of a cold load's, but only on proof: anything
    // unproved charges the full requirement, as for a cold load.
    let residency = residency(state, ledger, upstream, demand);
    let requirement = model.and_then(|name| model_requirement(state, upstream, name));
    let resident_min_free_mib = requirement.map_or(0, |r| r.resident_min_free_mib);
    let mut charged = Vec::new();

    // The primary resource and every co-required resource pass through the
    // same switch, freshness, kind-specific reserve, and declared model
    // capacity checks. A scalar model budget applies to every NVIDIA GPU and
    // to a memory primary; an explicit map may also budget another
    // byte-capacity dependency. The owner's memory reserve is a floor under
    // the job: the job's requirement must fit in what is free beyond it.
    for id in std::iter::once(&upstream.resource).chain(upstream.also_requires.iter()) {
        let rc = state
            .config
            .resources
            .iter()
            .find(|resource| &resource.id == id)
            .expect("configuration validation requires every dependency");
        let capacity = checked_resource_capacity(state, rc)?;
        let scalar_applies = upstream.scalar_applies_to(rc);
        let load_mib = requirement
            .and_then(|requirement| requirement.for_resource(id, scalar_applies))
            .or_else(|| {
                (requirement.is_none() && scalar_applies).then_some(upstream.default_min_free_mib)
            })
            .unwrap_or(0);
        let required_mib = residency.as_ref().map_or(load_mib, |credit| {
            credit.charge(id, load_mib, resident_min_free_mib)
        });
        let credited = required_mib != load_mib;
        if credited {
            charged.push(format!(
                "{required_mib} MiB instead of {load_mib} MiB on '{id}'"
            ));
        }
        if required_mib > 0 {
            let reserve = rc.owner_reserve_bytes();
            let free_mib = usable_mib(&capacity, rc);
            if free_mib < required_mib {
                let (beyond, beyond_it) = reserve_note(reserve);
                let detail = match &residency {
                    Ok(credit) if credited => format!(
                        "{}; it still needs {required_mib} MiB free on '{id}'{beyond}, only {free_mib} MiB free{beyond_it} (a cold load needs {load_mib} MiB)",
                        credit.label
                    ),
                    _ => {
                        let mut detail = format!(
                            "model {} needs {} MiB free on '{}'{beyond}, only {} MiB free{beyond_it}",
                            model.unwrap_or("(default)"),
                            required_mib,
                            id,
                            free_mib
                        );
                        if let Err(NoCredit::Unproved { reason, .. }) = &residency {
                            detail.push_str(&format!(" (no resident credit: {reason})"));
                        }
                        detail
                    }
                };
                return Err(Refusal {
                    reason: RefusalReason::InsufficientCapacity,
                    detail,
                    resource: Some(id.clone()),
                    retry_after_s: 30,
                });
            }
        }
    }
    let residency_note = residency
        .as_ref()
        .ok()
        .filter(|_| !charged.is_empty())
        .map(|credit| format!("{}; charged {}", credit.label, charged.join(", ")));
    Ok(Admission {
        credit: residency.ok(),
        residency: residency_note,
    })
}

/// Whole MiB a job may use on `resource`: what the sample shows free, less
/// the bytes its owner reserve keeps back. Rounded down, so a job is never
/// credited with memory that would dip into the reserve.
pub(crate) fn usable_mib(capacity: &Capacity, resource: &crate::config::ResourceConfig) -> u64 {
    capacity
        .free_bytes
        .saturating_sub(resource.owner_reserve_bytes())
        / MIB
}

/// How a refusal names the reserve its figures already exclude: text for
/// after the requirement, and for after the free figure.
pub(crate) fn reserve_note(reserve_bytes: u64) -> (String, &'static str) {
    if reserve_bytes == 0 {
        (String::new(), "")
    } else {
        (
            format!(
                " beyond the owner's {} MiB reserve",
                reserve_bytes.div_ceil(MIB)
            ),
            " beyond it",
        )
    }
}

/// Prove, or fail to prove, that `demand` can reuse what the upstream holds.
///
/// Every condition must hold on the upstream's latest model observation:
/// it is ready and at most [`RESIDENT_FRESH_FOR`] old; it was sampled after
/// the last job or yield on every dependency, so a model a yield unloaded
/// is not still counted; the upstream reports no queued or running work;
/// and the adapter's own rule below is met.
///
/// - Named models (Ollama): a loaded row has the request's canonical model
///   name, its `expires_at` parses and is at least 30 s away, the request
///   sets no runner option other than `num_ctx`, and the row's
///   `context_length` is the one such requests were learned to load, or,
///   absent evidence of parallel slots, the request's own `num_ctx` or the
///   `num_ctx` the model's definition sets for a request that leaves it
///   unset.
/// - A single model (llama.cpp): the observation shows it loaded and idle.
/// - An allocator pool (ComfyUI): the route has exactly one GPU and the
///   upstream reports how much its allocator holds.
///
/// Eviction of other idle models is never credited.
pub(crate) fn residency(
    state: &AppState,
    ledger: &Ledger,
    upstream: &UpstreamConfig,
    demand: Demand<'_>,
) -> Result<Credit, NoCredit> {
    let runtime = &state.upstreams[&upstream.id];
    let rule = runtime.adapter.residency();
    if rule == Residency::None {
        return Err(NoCredit::Unsupported);
    }
    let dependencies = crate::admission::dependencies(upstream);
    let (view, predates_boundary) = {
        let observation = runtime.models.read().unwrap();
        let predates_boundary = dependencies.iter().any(|id| {
            ledger
                .settled_at(id)
                .is_some_and(|at| !observation.sampled_after(at))
        });
        (observation.view(RESIDENT_FRESH_FOR), predates_boundary)
    };
    let models = match view.quality {
        ObservationQuality::Ready => view
            .value
            .expect("a ready observation always carries a value"),
        ObservationQuality::Stale => {
            let age_s = view.age.map(|age| age.as_secs()).unwrap_or(0);
            return Err(NoCredit::refreshable(format!(
                "the model observation is {age_s}s old"
            )));
        }
        ObservationQuality::Unknown => {
            return Err(NoCredit::refreshable(
                "the model observation is unavailable",
            ))
        }
    };
    if predates_boundary {
        return Err(NoCredit::refreshable(
            "the model observation predates the last job or yield",
        ));
    }
    if models.busy {
        return Err(NoCredit::fixed(
            "the upstream reports running or queued work",
        ));
    }
    match rule {
        Residency::None => Err(NoCredit::Unsupported),
        Residency::NamedModels => named_model(
            ledger,
            &upstream.id,
            runtime.adapter.as_ref(),
            &models,
            demand,
        ),
        Residency::SingleModel => {
            let served = models.models.first().filter(|row| {
                models.models.len() == 1
                    && row.state == "loaded_idle"
                    && row.name.as_deref().is_none_or(|served| {
                        demand.model.is_some_and(|requested| {
                            runtime.adapter.canonical_model(requested)
                                == runtime.adapter.canonical_model(served)
                        })
                    })
            });
            if let Some(served) = served {
                Ok(Credit {
                    label: served.name.as_ref().map_or_else(
                        || "the served model is resident".into(),
                        |name| format!("model {name} is resident"),
                    ),
                    kind: CreditKind::Model,
                })
            } else {
                Err(NoCredit::fixed(
                    "the upstream reports no matching idle loaded model",
                ))
            }
        }
        Residency::AllocatorPool => {
            let gpus: Vec<&String> = dependencies
                .iter()
                .filter(|id| {
                    state
                        .config
                        .resources
                        .iter()
                        .any(|r| &&r.id == id && r.kind == "nvidia-gpu")
                })
                .collect();
            let [gpu] = gpus.as_slice() else {
                return Err(NoCredit::fixed(
                    "allocator credit applies only to a route with one GPU",
                ));
            };
            let Some(bytes) = models.models.first().and_then(|m| m.resident_bytes) else {
                return Err(NoCredit::fixed(
                    "the upstream does not report its allocator size",
                ));
            };
            let mib = bytes / MIB;
            Ok(Credit {
                label: format!("the upstream's allocator holds {mib} MiB on '{gpu}'"),
                kind: CreditKind::Pool {
                    resource: (*gpu).clone(),
                    mib,
                },
            })
        }
    }
}

fn named_model(
    ledger: &Ledger,
    upstream: &str,
    adapter: &dyn crate::plugin::adapter::Adapter,
    models: &crate::lifecycle::Models,
    demand: Demand<'_>,
) -> Result<Credit, NoCredit> {
    let Some(name) = demand.model else {
        return Err(NoCredit::fixed("the request names no model"));
    };
    let wanted = adapter.canonical_model(name);
    let Some(row) = models.models.iter().find(|m| {
        matches!(m.state.as_str(), "loaded_idle" | "running")
            && m.name
                .as_deref()
                .is_some_and(|n| adapter.canonical_model(n) == wanted)
    }) else {
        return Err(NoCredit::fixed(format!("{wanted} is not loaded")));
    };
    let Some(expires_ms) = row.expires_at.as_deref().and_then(rfc3339_unix_ms) else {
        return Err(NoCredit::fixed(format!(
            "{wanted} has no readable unload time"
        )));
    };
    let now_ms = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |since| {
            i64::try_from(since.as_millis()).unwrap_or(i64::MAX)
        });
    let remaining_ms = expires_ms.saturating_sub(now_ms);
    if remaining_ms < RESIDENT_EXPIRY_MARGIN_MS {
        return Err(NoCredit::fixed(format!(
            "{wanted} unloads in {}s",
            remaining_ms.max(0) / 1000
        )));
    }
    let Some(runner) = demand.runner else {
        return Err(NoCredit::fixed(
            "the request sets a runner option other than num_ctx, so the upstream may reload the model",
        ));
    };
    let Some(loaded) = row.context_length else {
        return Err(NoCredit::fixed(format!(
            "the upstream does not report {wanted}'s context length"
        )));
    };
    // The request reuses the runner when a finished request like it left
    // the model loaded with exactly this context (Ollama's default for an
    // unset num_ctx, or a multiple of an explicit one with parallel slots),
    // or, with one slot per runner, when it asks for the loaded context
    // itself or leaves it to a model definition that fixes it. A learned
    // context that shows parallel slots withholds the last two: Ollama then
    // loads a multiple of what the request asks for.
    let asked = runner
        .num_ctx
        .map_or_else(|| "an unset num_ctx".to_owned(), |n| format!("num_ctx {n}"));
    let parallel = ledger.parallel_slots(upstream);
    let learned = ledger.learned_context(upstream, &wanted, runner);
    if let Some(learned) = learned {
        if learned.digest.is_none() || learned.digest == row.digest {
            if learned.context_length == loaded {
                return Ok(Credit {
                    label: format!(
                        "model {wanted} is resident with context {loaded}, which requests with {asked} were seen to load"
                    ),
                    kind: CreditKind::Model,
                });
            }
            return Err(NoCredit::fixed(format!(
                "requests with {asked} loaded context {}, but the resident context is {loaded}",
                learned.context_length
            )));
        }
    }
    let single_slot = || {
        parallel.map_or(Ok(()), |(k, c)| {
            Err(NoCredit::fixed(format!(
                "the upstream runs parallel slots (a request with num_ctx {k} loaded context {c}), so {asked} is not known to reuse the resident context {loaded}"
            )))
        })
    };
    if runner.num_ctx == Some(loaded) {
        single_slot()?;
        return Ok(Credit {
            label: format!("model {wanted} is resident with context {loaded}"),
            kind: CreditKind::Model,
        });
    }
    if runner.num_ctx.is_none() && row.default_context_length == Some(loaded) {
        single_slot()?;
        return Ok(Credit {
            label: format!(
                "model {wanted} is resident with context {loaded}, the num_ctx its definition sets for requests that leave it unset"
            ),
            kind: CreditKind::Model,
        });
    }
    Err(NoCredit::fixed(match (learned, runner.num_ctx) {
        (Some(_), _) => format!(
            "the context for {asked} was learned for a different {wanted} digest"
        ),
        (None, Some(n)) => format!(
            "the request's num_ctx {n} differs from the resident context {loaded}, and no earlier request with it was seen to load {loaded}"
        ),
        (None, None) => format!(
            "no earlier request with {asked} has shown which context the upstream loads {wanted} with, and its definition does not fix one equal to the resident context {loaded}"
        ),
    }))
}

/// Milliseconds since the Unix epoch for an RFC 3339 timestamp, such as
/// Go's `2026-09-23T03:45:12.123456789-05:00`. Anything else is `None`, so
/// an unreadable expiry never proves residency.
fn rfc3339_unix_ms(text: &str) -> Option<i64> {
    fn digits(text: &str, range: std::ops::Range<usize>) -> Option<i64> {
        let part = text.get(range)?;
        part.bytes()
            .all(|b| b.is_ascii_digit())
            .then(|| part.parse().ok())?
    }
    let bytes = text.as_bytes();
    if bytes.len() < 20
        || bytes[4] != b'-'
        || bytes[7] != b'-'
        || !matches!(bytes[10], b'T' | b't')
        || bytes[13] != b':'
        || bytes[16] != b':'
    {
        return None;
    }
    let (year, month, day) = (
        digits(text, 0..4)?,
        digits(text, 5..7)?,
        digits(text, 8..10)?,
    );
    let (hour, minute, second) = (
        digits(text, 11..13)?,
        digits(text, 14..16)?,
        digits(text, 17..19)?,
    );
    let leap = (year % 4 == 0 && year % 100 != 0) || year % 400 == 0;
    let month_days = match month {
        1 | 3 | 5 | 7 | 8 | 10 | 12 => 31,
        4 | 6 | 9 | 11 => 30,
        2 if leap => 29,
        2 => 28,
        _ => return None,
    };
    if day < 1 || day > month_days || hour > 23 || minute > 59 || second > 60 {
        return None;
    }
    let mut rest = &text[19..];
    let mut millis = 0;
    if let Some(fraction) = rest.strip_prefix('.') {
        let count = fraction.bytes().take_while(u8::is_ascii_digit).count();
        if count == 0 {
            return None;
        }
        let kept = &fraction[..count.min(3)];
        millis = kept.parse::<i64>().ok()? * 10_i64.pow(3 - kept.len() as u32);
        rest = &fraction[count..];
    }
    let offset_minutes = match rest {
        "Z" | "z" => 0,
        _ => {
            let sign = match rest.as_bytes().first()? {
                b'+' => 1,
                b'-' => -1,
                _ => return None,
            };
            if rest.len() != 6 || rest.as_bytes()[3] != b':' {
                return None;
            }
            let (hours, minutes) = (digits(rest, 1..3)?, digits(rest, 4..6)?);
            if hours > 23 || minutes > 59 {
                return None;
            }
            sign * (hours * 60 + minutes)
        }
    };
    // Days from 1970-01-01 to the civil date (Howard Hinnant's algorithm).
    let y = if month <= 2 { year - 1 } else { year };
    let era = y.div_euclid(400);
    let year_of_era = y - era * 400;
    let day_of_year = (153 * ((month + 9) % 12) + 2) / 5 + day - 1;
    let day_of_era = year_of_era * 365 + year_of_era / 4 - year_of_era / 100 + day_of_year;
    let days = era * 146_097 + day_of_era - 719_468;
    let seconds = days * 86_400 + hour * 3_600 + minute * 60 + second - offset_minutes * 60;
    Some(seconds * 1_000 + millis)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::Config;
    use crate::plugin::pressure::PressureVerdict;

    fn test_state() -> AppState {
        let state = unsampled_presence_state();
        state.mark_presence_sampled();
        state
    }

    fn unsampled_presence_state() -> AppState {
        let cfg: Config = toml::from_str(
            r#"
            [server]
            listen = "127.0.0.1:0"
            [owner.gpu_pressure]
            enabled = false
            [[resource]]
            id = "gpu0"
            kind = "nvidia-gpu"
            [[upstream]]
            id = "ollama"
            listen = "127.0.0.1:0"
            target = "http://127.0.0.1:11434"
            kind = "ollama"
            resource = "gpu0"
            default_min_free_mib = 8000
            [upstream.models]
            "big:20b" = 13000
            "#,
        )
        .unwrap();
        AppState::new(cfg).unwrap()
    }

    fn set_capacity(state: &AppState, id: &str, capacity: Capacity) {
        let resource = state
            .config
            .resources
            .iter()
            .find(|resource| resource.id == id)
            .unwrap();
        let lendable = state.resources[id]
            .probe
            .lock()
            .unwrap()
            .lendable(&capacity, resource.reserve);
        let resource_state = &state.resources[id];
        let mut observation = resource_state.capacity.write().unwrap();
        resource_state
            .probe_lendable
            .store(lendable, Ordering::Relaxed);
        observation.record_success(capacity);
    }

    fn set_free(state: &AppState, mib: u64) {
        set_capacity(
            state,
            "gpu0",
            Capacity {
                free_bytes: mib * 1024 * 1024,
                ..Capacity::default()
            },
        );
    }

    #[test]
    fn startup_state_refuses_until_presence_has_been_sampled() {
        let state = unsampled_presence_state();
        set_free(&state, 14000);
        let up = &state.config.upstreams[0];
        // Before the first presence publish, the session and the veto are
        // defaults that read as "owner away".
        let err = admit(&state, up, Some("big:20b")).unwrap_err();
        assert_eq!(err.reason, RefusalReason::OwnerActive);
        assert!(err.detail.contains("presence has not been sampled"));
        state.mark_presence_sampled();
        assert!(admit(&state, up, Some("big:20b")).is_ok());
    }

    #[test]
    fn startup_state_refuses_until_capacity_has_been_observed() {
        let state = test_state();
        let up = &state.config.upstreams[0];
        let err = admit(&state, up, Some("small:12b")).unwrap_err();
        assert_eq!(err.reason, RefusalReason::ResourceUnmeasured);
        assert_eq!(err.resource.as_deref(), Some("gpu0"));
    }

    #[test]
    fn admits_when_capacity_suffices() {
        let state = test_state();
        set_free(&state, 14000);
        let up = &state.config.upstreams[0].clone();
        assert!(admit(&state, up, Some("big:20b")).is_ok());
        assert!(admit(&state, up, Some("unknown")).is_ok());
    }

    #[test]
    fn admit_with_uses_the_given_snapshot_not_live_state() {
        // The status endpoint reads veto/lending/presence once and decides
        // every upstream route from that one snapshot, so a build can no
        // longer mix a value read at one instant with live state read a
        // moment later (see CORR-1). Prove admit_with honors what it is
        // given: live state still allows lending, but a snapshot built with
        // it switched off must refuse anyway.
        let state = test_state();
        set_free(&state, 14000);
        let up = &state.config.upstreams[0].clone();
        assert!(state.lending_enabled.load(Ordering::Relaxed));
        let mut snapshot = Snapshot::read(&state);
        snapshot.lending_enabled = false;
        let err = admit_with(&state, &snapshot, up, None).unwrap_err();
        assert_eq!(err.reason, RefusalReason::LendingDisabled);
        // Live state itself was never touched, so a fresh read still admits.
        assert!(admit(&state, up, None).is_ok());
    }

    #[test]
    fn refuses_per_model_threshold() {
        let state = test_state();
        set_free(&state, 11400); // the measured warm-llama-server budget
        let up = &state.config.upstreams[0].clone();
        let err = admit(&state, up, Some("big:20b")).unwrap_err();
        assert_eq!(err.reason, RefusalReason::InsufficientCapacity);
        // A smaller model still fits the same moment.
        assert!(admit(&state, up, Some("small:12b")).is_ok());
    }

    #[test]
    fn registry_spelling_takes_the_short_name_threshold() {
        let state = test_state();
        set_free(&state, 11400);
        let up = &state.config.upstreams[0].clone();
        for name in [
            "registry.ollama.ai/library/big:20b",
            "library/big:20b",
            "big:20b",
        ] {
            let err = admit(&state, up, Some(name)).unwrap_err();
            assert_eq!(err.reason, RefusalReason::InsufficientCapacity, "{name}");
        }
        set_free(&state, 14000);
        assert!(admit(&state, up, Some("registry.ollama.ai/library/big:20b")).is_ok());
    }

    #[test]
    fn owner_veto_refuses_everything() {
        let state = test_state();
        set_free(&state, 15000);
        state.veto.observe(Some("Marathon".into()), 120);
        let up = &state.config.upstreams[0].clone();
        let err = admit(&state, up, Some("small:12b")).unwrap_err();
        assert_eq!(err.reason, RefusalReason::OwnerActive);
    }

    #[test]
    fn a_resource_scoped_upstream_lends_through_the_owner_veto() {
        let state = test_state();
        set_free(&state, 15000);
        state.veto.observe(Some("Marathon".into()), 120);
        let mut up = state.config.upstreams[0].clone();
        up.owner_scope = crate::config::OwnerScope::Resource;
        assert!(admit(&state, &up, Some("small:12b")).is_ok());
        // Its own capacity still governs.
        set_free(&state, 100);
        let err = admit(&state, &up, Some("small:12b")).unwrap_err();
        assert_ne!(err.reason, RefusalReason::OwnerActive);
    }

    #[test]
    fn a_resource_scoped_upstream_does_not_need_session_evidence() {
        use crate::plugin::presence::{PresenceSignal, SessionAuthority, SessionState};

        let state = test_state();
        set_free(&state, 15000);
        *state.presence.write().unwrap() = PresenceSignal {
            session: SessionState::Active,
            session_authority: SessionAuthority::Missing,
            ..PresenceSignal::default()
        };
        let mut up = state.config.upstreams[0].clone();
        let err = admit(&state, &up, None).unwrap_err();
        assert_eq!(err.reason, RefusalReason::OwnerActive);
        up.owner_scope = crate::config::OwnerScope::Resource;
        assert!(admit(&state, &up, None).is_ok());
    }

    #[test]
    fn an_owner_away_reading_older_than_its_freshness_bound_refuses_host_scoped_work() {
        // The owner locked the machine, then the presence sampler stalled
        // while capacity kept updating. The last reading still says away.
        let state = test_state();
        set_free(&state, 15000);
        let up = &state.config.upstreams[0];
        assert!(admit(&state, up, Some("big:20b")).is_ok());
        let presence_failure = |state: &AppState| {
            state
                .readiness_failures()
                .into_iter()
                .any(|failure| failure.contains("presence"))
        };
        assert!(!presence_failure(&state));

        let too_old = state.presence_fresh_for() + std::time::Duration::from_secs(1);
        *state.presence_sampled_at.write().unwrap() =
            Some(std::time::Instant::now().checked_sub(too_old).unwrap());

        let err = admit(&state, up, Some("big:20b")).unwrap_err();
        assert_eq!(err.reason, RefusalReason::OwnerActive);
        assert!(err.detail.contains("out of date"), "{}", err.detail);
        assert!(presence_failure(&state));
        // A resource-scoped upstream never used the reading.
        let mut resource_scoped = up.clone();
        resource_scoped.owner_scope = crate::config::OwnerScope::Resource;
        assert!(admit(&state, &resource_scoped, Some("big:20b")).is_ok());
        // A fresh sample restores host-scoped admission.
        *state.presence_sampled_at.write().unwrap() = Some(std::time::Instant::now());
        assert!(admit(&state, up, Some("big:20b")).is_ok());
    }

    #[test]
    fn a_complete_backend_reading_of_an_active_session_is_left_to_the_owner_check() {
        // systemd-logind needs no session reporter: an active session it
        // publishes as NotRequired is decided by the owner veto (idle
        // lending, unlocked_session_is_presence), not refused as ambiguous.
        use crate::plugin::presence::{PresenceSignal, SessionAuthority, SessionState};

        let state = test_state();
        set_free(&state, 15000);
        *state.presence.write().unwrap() = PresenceSignal {
            session: SessionState::Active,
            session_authority: SessionAuthority::NotRequired,
            idle_seconds: Some(900),
            ..PresenceSignal::default()
        };
        assert_eq!(
            state
                .session_evidence(&state.presence.read().unwrap())
                .authority,
            SessionAuthority::NotRequired
        );
        assert!(admit(&state, &state.config.upstreams[0], None).is_ok());

        // Ambiguous stays refused, as does a Windows-shaped active session
        // whose reporter requirements are unmet.
        state.presence.write().unwrap().session_authority = SessionAuthority::Ambiguous;
        assert_eq!(
            admit(&state, &state.config.upstreams[0], None)
                .unwrap_err()
                .reason,
            RefusalReason::OwnerActive
        );
    }

    #[test]
    fn an_unresolved_unknown_session_is_refused_by_the_gate_itself() {
        // A backend with no trustworthy answer yet (unreadable, inconsistent,
        // or not yet confirmed) publishes an unknown, ambiguous session. The
        // gate refuses it on its own, as it does an ambiguous active one,
        // without relying on the veto; a platform with no presence backend
        // (unknown, NotRequired) is still left to the other signals.
        use crate::plugin::presence::{PresenceSignal, SessionAuthority, SessionState};

        let state = test_state();
        set_free(&state, 15000);
        *state.presence.write().unwrap() = PresenceSignal {
            session: SessionState::Unknown,
            session_authority: SessionAuthority::Ambiguous,
            basis: Some("logind: session view not yet confirmed".into()),
            ..PresenceSignal::default()
        };
        assert_eq!(
            state
                .session_evidence(&state.presence.read().unwrap())
                .authority,
            SessionAuthority::Ambiguous
        );
        let refusal = admit(&state, &state.config.upstreams[0], None).unwrap_err();
        assert_eq!(refusal.reason, RefusalReason::OwnerActive);

        state.presence.write().unwrap().session_authority = SessionAuthority::NotRequired;
        assert_eq!(
            state
                .session_evidence(&state.presence.read().unwrap())
                .authority,
            SessionAuthority::NotRequired
        );
        assert!(admit(&state, &state.config.upstreams[0], None).is_ok());
    }

    #[test]
    fn reporter_disconnect_or_expiry_refuses_before_the_next_presence_poll() {
        use crate::plugin::presence::{
            MediaState, PresenceSignal, SessionAuthority, SessionRequirement, SessionState,
        };

        let state = test_state();
        set_free(&state, 15000);
        let fresh_after = std::time::Instant::now() - std::time::Duration::from_secs(1);
        *state.presence.write().unwrap() = PresenceSignal {
            session: SessionState::Active,
            session_id: Some(7),
            applicable_session_ids: vec![7],
            session_authority: SessionAuthority::Ready,
            session_requirements: vec![SessionRequirement {
                session_id: 7,
                fresh_after,
            }],
            foreground: None,
            owner_app: None,
            idle_seconds: Some(600),
            media: MediaState::Idle,
            basis: None,
            sessions: Vec::new(),
        };

        let report = crate::agent::AgentReport {
            protocol_version: crate::agent::AGENT_PROTOCOL_VERSION,
            sequence: 1,
            session_id: 7,
            idle_seconds: 600,
            foreground: None,
            agent_version: env!("CARGO_PKG_VERSION").into(),
            sampled_at_unix_ms: crate::agent::unix_time_ms(),
        };
        let generation = state.begin_agent_connection();
        assert!(state.record_agent_report(
            report.clone(),
            generation,
            std::time::Duration::from_secs(15),
            std::time::Duration::ZERO,
            std::time::Instant::now()
        ));
        assert!(admit(&state, &state.config.upstreams[0], None).is_ok());

        state.end_agent_connection(7, generation);
        assert_eq!(
            admit(&state, &state.config.upstreams[0], None)
                .unwrap_err()
                .reason,
            RefusalReason::OwnerActive
        );

        let expired_generation = state.begin_agent_connection();
        assert!(state.record_agent_report(
            report,
            expired_generation,
            std::time::Duration::ZERO,
            std::time::Duration::ZERO,
            std::time::Instant::now()
        ));
        assert_eq!(
            admit(&state, &state.config.upstreams[0], None)
                .unwrap_err()
                .reason,
            RefusalReason::OwnerActive
        );
    }

    #[test]
    fn a_failed_sample_invalidates_the_last_good_capacity() {
        let state = test_state();
        set_free(&state, 15000);
        state.resources["gpu0"]
            .capacity
            .write()
            .unwrap()
            .record_failure("nvidia-smi timed out");

        let err = admit(&state, &state.config.upstreams[0], None).unwrap_err();
        assert_eq!(err.reason, RefusalReason::ResourceUnmeasured);
        assert!(err.detail.contains("nvidia-smi timed out"));
    }

    #[test]
    fn a_stale_sample_cannot_authorize_admission() {
        let state = test_state();
        state.resources["gpu0"]
            .capacity
            .write()
            .unwrap()
            .record_success_at(
                Capacity {
                    free_bytes: 15000 * 1024 * 1024,
                    ..Capacity::default()
                },
                std::time::Instant::now()
                    - state.capacity_fresh_for()
                    - std::time::Duration::from_secs(1),
            );

        let err = admit(&state, &state.config.upstreams[0], None).unwrap_err();
        assert_eq!(err.reason, RefusalReason::ResourceUnmeasured);
        assert!(err.detail.contains("stale"));
    }

    #[test]
    fn gpu_pressure_is_a_required_resource_scoped_input() {
        let cfg: Config = toml::from_str(
            r#"
            [server]
            listen = "127.0.0.1:0"
            [[resource]]
            id = "gpu0"
            kind = "nvidia-gpu"
            [[upstream]]
            id = "ollama"
            listen = "127.0.0.1:0"
            target = "http://127.0.0.1:11434"
            kind = "ollama"
            resource = "gpu0"
            "#,
        )
        .unwrap();
        let state = AppState::new(cfg).unwrap();
        state.mark_presence_sampled();
        set_capacity(&state, "gpu0", Capacity::default());

        let unknown = admit(&state, &state.config.upstreams[0], None).unwrap_err();
        assert_eq!(unknown.reason, RefusalReason::ResourceUnmeasured);
        assert_eq!(unknown.resource.as_deref(), Some("gpu0"));
        assert!(unknown.detail.contains("pressure is unknown"));

        state.resources["gpu0"]
            .pressure
            .write()
            .unwrap()
            .record_success(PressureVerdict {
                non_lender_pct: 95.0,
                engaged: true,
                threshold_pct: 45.0,
                baseline_pct: Some(25.0),
                target_pct: None,
                learned_samples: 0,
                learned_s: 0,
                mode: Default::default(),
                calibrated: true,
                calibration_samples: 60,
                calibration_required: 60,
                calibration_hint: None,
                above_for_s: 15u64,
            });
        let pressured = admit(&state, &state.config.upstreams[0], None).unwrap_err();
        assert_eq!(pressured.reason, RefusalReason::ResourcePressured);
        assert_eq!(pressured.resource.as_deref(), Some("gpu0"));

        state.resources["gpu0"]
            .pressure
            .write()
            .unwrap()
            .record_success(PressureVerdict {
                non_lender_pct: 8.0,
                engaged: false,
                threshold_pct: 45.0,
                baseline_pct: Some(25.0),
                target_pct: None,
                learned_samples: 0,
                learned_s: 0,
                mode: Default::default(),
                calibrated: true,
                calibration_samples: 60,
                calibration_required: 60,
                calibration_hint: None,
                above_for_s: 0u64,
            });
        assert!(admit(&state, &state.config.upstreams[0], None).is_ok());
    }

    #[test]
    fn pressure_on_one_gpu_does_not_refuse_another_gpu() {
        let cfg: Config = toml::from_str(
            r#"
            [server]
            listen = "127.0.0.1:0"
            [[resource]]
            id = "gpu0"
            kind = "nvidia-gpu"
            [[resource]]
            id = "gpu1"
            kind = "nvidia-gpu"
            [[upstream]]
            id = "first"
            listen = "127.0.0.1:0"
            target = "http://127.0.0.1:11434"
            kind = "ollama"
            resource = "gpu0"
            [[upstream]]
            id = "second"
            listen = "127.0.0.1:0"
            target = "http://127.0.0.1:11435"
            kind = "ollama"
            resource = "gpu1"
            "#,
        )
        .unwrap();
        let state = AppState::new(cfg).unwrap();
        state.mark_presence_sampled();
        for id in ["gpu0", "gpu1"] {
            set_capacity(&state, id, Capacity::default());
            state.resources[id]
                .pressure
                .write()
                .unwrap()
                .record_success(PressureVerdict {
                    non_lender_pct: if id == "gpu0" { 95.0 } else { 8.0 },
                    engaged: id == "gpu0",
                    threshold_pct: 45.0,
                    baseline_pct: Some(25.0),
                    target_pct: None,
                    learned_samples: 0,
                    learned_s: 0,
                    above_for_s: if id == "gpu0" { 15 } else { 0 },
                    mode: Default::default(),
                    calibrated: true,
                    calibration_samples: 60,
                    calibration_required: 60,
                    calibration_hint: None,
                });
        }

        let first = admit(&state, &state.config.upstreams[0], None).unwrap_err();
        assert_eq!(first.reason, RefusalReason::ResourcePressured);
        assert_eq!(first.resource.as_deref(), Some("gpu0"));
        assert!(admit(&state, &state.config.upstreams[1], None).is_ok());
    }

    #[test]
    fn stale_gpu_pressure_cannot_authorize_admission() {
        let cfg: Config = toml::from_str(
            r#"
            [server]
            listen = "127.0.0.1:0"
            [[resource]]
            id = "gpu0"
            kind = "nvidia-gpu"
            [[upstream]]
            id = "ollama"
            listen = "127.0.0.1:0"
            target = "http://127.0.0.1:11434"
            kind = "ollama"
            resource = "gpu0"
            "#,
        )
        .unwrap();
        let state = AppState::new(cfg).unwrap();
        state.mark_presence_sampled();
        set_capacity(&state, "gpu0", Capacity::default());
        state.resources["gpu0"]
            .pressure
            .write()
            .unwrap()
            .record_success_at(
                PressureVerdict {
                    non_lender_pct: 8.0,
                    engaged: false,
                    threshold_pct: 45.0,
                    baseline_pct: Some(25.0),
                    target_pct: None,
                    learned_samples: 0,
                    learned_s: 0,
                    mode: Default::default(),
                    calibrated: true,
                    calibration_samples: 60,
                    calibration_required: 60,
                    calibration_hint: None,
                    above_for_s: 0u64,
                },
                std::time::Instant::now()
                    - state.pressure_fresh_for()
                    - std::time::Duration::from_secs(1),
            );

        let refusal = admit(&state, &state.config.upstreams[0], None).unwrap_err();
        assert_eq!(refusal.reason, RefusalReason::ResourceUnmeasured);
        assert_eq!(refusal.resource.as_deref(), Some("gpu0"));
        assert!(refusal.detail.contains("pressure sample is stale"));
    }

    #[test]
    fn the_primary_resource_uses_its_probe_reserve_rule() {
        let cfg: Config = toml::from_str(
            r#"
            [server]
            listen = "127.0.0.1:0"
            [owner.gpu_pressure]
            enabled = false
            [[resource]]
            id = "cpu"
            kind = "system-cpu"
            reserve = 50
            [[upstream]]
            id = "cpu-work"
            listen = "127.0.0.1:0"
            target = "http://127.0.0.1:9000"
            kind = "openai"
            resource = "cpu"
            "#,
        )
        .unwrap();
        let state = AppState::new(cfg).unwrap();
        state.mark_presence_sampled();
        set_capacity(
            &state,
            "cpu",
            Capacity {
                utilization_pct: Some(90),
                ..Capacity::default()
            },
        );

        let err = admit(&state, &state.config.upstreams[0], None).unwrap_err();
        assert_eq!(err.reason, RefusalReason::InsufficientCapacity);
        assert_eq!(err.resource.as_deref(), Some("cpu"));
    }

    #[test]
    fn every_additional_dependency_requires_fresh_capacity() {
        let cfg: Config = toml::from_str(
            r#"
            [server]
            listen = "127.0.0.1:0"
            [owner.gpu_pressure]
            enabled = false
            [[resource]]
            id = "gpu0"
            kind = "nvidia-gpu"
            [[resource]]
            id = "mem"
            kind = "system-memory"
            reserve = 1024
            [[upstream]]
            id = "ollama"
            listen = "127.0.0.1:0"
            target = "http://127.0.0.1:11434"
            kind = "ollama"
            resource = "gpu0"
            also_requires = ["mem"]
            "#,
        )
        .unwrap();
        let state = AppState::new(cfg).unwrap();
        state.mark_presence_sampled();
        set_capacity(&state, "gpu0", Capacity::default());

        let err = admit(&state, &state.config.upstreams[0], None).unwrap_err();
        assert_eq!(err.reason, RefusalReason::ResourceUnmeasured);
        assert_eq!(err.resource.as_deref(), Some("mem"));

        set_capacity(
            &state,
            "mem",
            Capacity {
                free_bytes: 2048,
                ..Capacity::default()
            },
        );
        assert!(admit(&state, &state.config.upstreams[0], None).is_ok());
    }

    #[test]
    fn scalar_model_budget_is_enforced_on_every_gpu_dependency() {
        let cfg: Config = toml::from_str(
            r#"
            [server]
            listen = "127.0.0.1:0"
            [owner.gpu_pressure]
            enabled = false
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
            listen = "127.0.0.1:0"
            target = "http://127.0.0.1:11434"
            kind = "llamacpp"
            resource = "gpu0"
            also_requires = ["gpu1"]
            [upstream.models]
            split = 12000
            "#,
        )
        .unwrap();
        let state = AppState::new(cfg).unwrap();
        state.mark_presence_sampled();
        state.upstreams["llama"]
            .models
            .write()
            .unwrap()
            .record_success(crate::lifecycle::Models::default());
        set_capacity(
            &state,
            "gpu0",
            Capacity {
                free_bytes: 13000 * 1024 * 1024,
                ..Capacity::default()
            },
        );
        set_capacity(
            &state,
            "gpu1",
            Capacity {
                free_bytes: 11000 * 1024 * 1024,
                ..Capacity::default()
            },
        );

        let refusal = admit(&state, &state.config.upstreams[0], Some("split")).unwrap_err();
        assert_eq!(refusal.reason, RefusalReason::InsufficientCapacity);
        assert_eq!(refusal.resource.as_deref(), Some("gpu1"));
    }

    #[test]
    fn per_resource_model_budgets_are_checked_independently() {
        let cfg: Config = toml::from_str(
            r#"
            [server]
            listen = "127.0.0.1:0"
            [owner.gpu_pressure]
            enabled = false
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
            listen = "127.0.0.1:0"
            target = "http://127.0.0.1:11434"
            kind = "llamacpp"
            resource = "gpu0"
            also_requires = ["gpu1"]
            [upstream.models]
            split = { gpu0 = 10000, gpu1 = 14000 }
            "#,
        )
        .unwrap();
        let state = AppState::new(cfg).unwrap();
        state.mark_presence_sampled();
        state.upstreams["llama"]
            .models
            .write()
            .unwrap()
            .record_success(crate::lifecycle::Models::default());
        set_capacity(
            &state,
            "gpu0",
            Capacity {
                free_bytes: 11000 * 1024 * 1024,
                ..Capacity::default()
            },
        );
        set_capacity(
            &state,
            "gpu1",
            Capacity {
                free_bytes: 13000 * 1024 * 1024,
                ..Capacity::default()
            },
        );
        let refusal = admit(&state, &state.config.upstreams[0], Some("split")).unwrap_err();
        assert_eq!(refusal.resource.as_deref(), Some("gpu1"));
        set_capacity(
            &state,
            "gpu1",
            Capacity {
                free_bytes: 15000 * 1024 * 1024,
                ..Capacity::default()
            },
        );
        let admitted = admit(&state, &state.config.upstreams[0], Some("split"));
        assert!(admitted.is_ok(), "{admitted:?}");
    }

    fn configured(toml_text: &str) -> AppState {
        let cfg: Config = toml::from_str(toml_text).unwrap();
        let state = AppState::new(cfg).unwrap();
        state.mark_presence_sampled();
        state
    }

    fn set_free_bytes(state: &AppState, id: &str, free_bytes: u64) {
        set_capacity(
            state,
            id,
            Capacity {
                free_bytes,
                ..Capacity::default()
            },
        );
    }

    const RESERVE_MIB: u64 = 23_582;

    /// A CPU-only upstream whose model needs as much RAM as the owner keeps
    /// back, as in the review's reproduction.
    fn memory_route() -> AppState {
        configured(&format!(
            r#"
            [server]
            listen = "127.0.0.1:0"
            [owner.gpu_pressure]
            enabled = false
            [[resource]]
            id = "mem"
            kind = "system-memory"
            reserve = {}
            [[upstream]]
            id = "cpu-llm"
            listen = "127.0.0.1:0"
            target = "http://127.0.0.1:9000"
            kind = "openai"
            resource = "mem"
            default_min_free_mib = 1000
            [upstream.models]
            "big" = {RESERVE_MIB}
            "#,
            RESERVE_MIB * MIB
        ))
    }

    #[test]
    fn the_owner_memory_reserve_is_a_floor_under_the_job_not_beside_it() {
        let state = memory_route();
        let up = state.config.upstreams[0].clone();
        // 31,357 MiB free: the reserve fits and the job fits, but not both.
        set_free_bytes(&state, "mem", 31_357 * MIB);
        let refusal = admit(&state, &up, Some("big")).unwrap_err();
        assert_eq!(refusal.reason, RefusalReason::InsufficientCapacity);
        assert_eq!(refusal.resource.as_deref(), Some("mem"));
        assert!(
            refusal.detail.contains(&format!(
                "needs {RESERVE_MIB} MiB free on 'mem' beyond the owner's {RESERVE_MIB} MiB reserve, only 7775 MiB free beyond it"
            )),
            "{}",
            refusal.detail
        );
        // Exactly the reserve plus the job is enough; one byte less is not.
        set_free_bytes(&state, "mem", 2 * RESERVE_MIB * MIB);
        assert!(admit(&state, &up, Some("big")).is_ok());
        set_free_bytes(&state, "mem", 2 * RESERVE_MIB * MIB - 1);
        assert_eq!(
            admit(&state, &up, Some("big")).unwrap_err().reason,
            RefusalReason::InsufficientCapacity
        );
        // The default requirement is charged the same way.
        set_free_bytes(&state, "mem", (RESERVE_MIB + 999) * MIB);
        assert!(admit(&state, &up, Some("other")).is_err());
        set_free_bytes(&state, "mem", (RESERVE_MIB + 1000) * MIB);
        assert!(admit(&state, &up, Some("other")).is_ok());
        // Below the reserve nothing is lendable, whatever the job needs.
        set_free_bytes(&state, "mem", RESERVE_MIB * MIB - 1);
        let refusal = admit(&state, &up, Some("other")).unwrap_err();
        assert!(
            refusal.detail.contains("not currently lendable"),
            "{}",
            refusal.detail
        );
    }

    #[test]
    fn a_cpu_only_route_enforces_its_default_memory_requirement() {
        // The review's reproduction: an impossible requirement was admitted
        // because the default applied only to GPUs.
        let state = configured(
            r#"
            [server]
            listen = "127.0.0.1:0"
            [owner.gpu_pressure]
            enabled = false
            [[resource]]
            id = "mem"
            kind = "system-memory"
            reserve = 0
            [[upstream]]
            id = "cpu-llm"
            listen = "127.0.0.1:0"
            target = "http://127.0.0.1:9000"
            kind = "openai"
            resource = "mem"
            default_min_free_mib = 1000000000000
            "#,
        );
        let up = state.config.upstreams[0].clone();
        set_free_bytes(&state, "mem", 64 * 1024 * MIB);
        let refusal = admit(&state, &up, Some("any")).unwrap_err();
        assert_eq!(refusal.reason, RefusalReason::InsufficientCapacity);
        assert!(
            refusal
                .detail
                .contains("needs 1000000000000 MiB free on 'mem', only 65536 MiB free"),
            "{}",
            refusal.detail
        );
        assert!(admit(&state, &up, None).is_err());
    }

    #[test]
    fn a_gpu_route_charges_memory_beside_the_reserve_and_leaves_the_gpu_alone() {
        let state = configured(&format!(
            r#"
            [server]
            listen = "127.0.0.1:0"
            [owner.gpu_pressure]
            enabled = false
            [[resource]]
            id = "gpu0"
            kind = "nvidia-gpu"
            [[resource]]
            id = "mem"
            kind = "system-memory"
            reserve = {}
            [[upstream]]
            id = "llama"
            listen = "127.0.0.1:0"
            target = "http://127.0.0.1:9000"
            kind = "openai"
            resource = "gpu0"
            also_requires = ["mem"]
            default_min_free_mib = 6000
            [upstream.models]
            "offload" = {{ gpu0 = 8000, mem = 4096 }}
            "#,
            16384 * MIB
        ));
        let up = state.config.upstreams[0].clone();
        set_free_bytes(&state, "gpu0", 8000 * MIB);
        set_free_bytes(&state, "mem", (16384 + 4095) * MIB);
        let refusal = admit(&state, &up, Some("offload")).unwrap_err();
        assert_eq!(refusal.resource.as_deref(), Some("mem"));
        set_free_bytes(&state, "mem", (16384 + 4096) * MIB);
        assert!(admit(&state, &up, Some("offload")).is_ok());
        // A scalar or default budgets the GPU only; the co-required memory
        // keeps its reserve and nothing more.
        set_free_bytes(&state, "gpu0", 6000 * MIB);
        set_free_bytes(&state, "mem", 16384 * MIB);
        assert!(admit(&state, &up, Some("unlisted")).is_ok());
        set_free_bytes(&state, "gpu0", 5999 * MIB);
        assert_eq!(
            admit(&state, &up, Some("unlisted"))
                .unwrap_err()
                .resource
                .as_deref(),
            Some("gpu0")
        );
    }

    #[test]
    fn a_resident_credit_on_memory_still_leaves_the_reserve_free() {
        let state = configured(&format!(
            r#"
            [server]
            listen = "127.0.0.1:0"
            [owner.gpu_pressure]
            enabled = false
            [[resource]]
            id = "mem"
            kind = "system-memory"
            reserve = {}
            [[upstream]]
            id = "llama"
            listen = "127.0.0.1:0"
            target = "http://127.0.0.1:9000"
            kind = "llamacpp"
            resource = "mem"
            [upstream.models]
            "m" = {{ min_free_mib = 8000, resident_min_free_mib = 500 }}
            "#,
            4096 * MIB
        ));
        state.upstreams["llama"]
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
        let up = state.config.upstreams[0].clone();
        set_free_bytes(&state, "mem", (4096 + 499) * MIB);
        let refusal = admit(&state, &up, Some("m")).unwrap_err();
        assert!(
            refusal
                .detail
                .contains("still needs 500 MiB free on 'mem' beyond the owner's 4096 MiB reserve"),
            "{}",
            refusal.detail
        );
        set_free_bytes(&state, "mem", (4096 + 500) * MIB);
        let admission = admit(&state, &up, Some("m")).unwrap();
        assert!(admission.residency.is_some());
    }

    #[test]
    fn veto_releases_only_after_grace() {
        let state = test_state();
        let rising = state.veto.observe(Some("Marathon".into()), 3600);
        assert!(rising);
        // Owner alt-tabbed away — veto must hold through the grace period.
        let rising2 = state.veto.observe(None, 3600);
        assert!(!rising2);
        assert!(state.veto.status().0, "veto released too early");
        // With zero grace it releases immediately.
        state.veto.observe(None, 0);
        assert!(!state.veto.status().0);
    }
}

#[cfg(test)]
#[path = "residency_tests.rs"]
mod residency_tests;
