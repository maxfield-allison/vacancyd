//! The status / metrics / admin server.
//!
//! - `GET  /status`                      — full JSON state (resources, veto, gates)
//! - `GET  /healthz`                     — liveness (200 while running)
//! - `GET  /readyz`                      — listeners and mandatory observations ready
//! - `GET  /metrics`                     — Prometheus exposition
//! - `POST /admin/lending/enable|disable`         — the global switch
//! - `POST /admin/resources/{id}/enable|disable`  — per-resource switches
//! - `GET|POST /admin/settings`: saved lending policy
//! - `POST /admin/debug/enable|disable`: a temporary debug logging window
//! - `GET  /admin/diagnostics`: one JSON document for a report
//!
//! Authentication is applied by [`guard`] as one layer over the whole
//! router, so a route added later cannot forget it: the scope is derived
//! from the path, and anything unrecognised is treated as `Read` rather
//! than as public. `/healthz` and the detail-free `/readyz` are the deliberate
//! exceptions for service supervisors.
//!
//! Admin requests then pass the local-admin policy
//! ([`vacancyd::local_admin::permits`]): on Windows and Linux a loopback
//! caller without a token may read settings and diagnostics and pause
//! lending, and nothing else. The same actions arrive with a verified account over the Windows
//! admin pipe (`agent::admin_server`) or the Linux admin socket
//! (`admin_socket`), which call [`perform_local_admin`].

use std::io::Write;
use std::net::SocketAddr;
use std::path::{Path as FsPath, PathBuf};
use std::sync::atomic::Ordering;
use std::sync::Arc;

use axum::extract::{ConnectInfo, Extension, Path, Request, State};
use axum::http::StatusCode;
use axum::middleware::Next;
use axum::response::IntoResponse;
use axum::routing::{get, post};
use axum::{Json, Router};
use serde_json::json;
use tracing::{info, warn};
use vacancyd::local_admin::{Action, Caller, PausedBy, RefusalReason, RequestedAction, Response};
use vacancyd::settings::{
    GpuPressureSettings, ModelResourceSetting, ModelSetting, OwnerSettings, ResourceSetting,
    ServerLimitsSettings, SettingsApplication, SettingsCapabilities, SettingsSnapshot,
    SettingsUpdate, UpstreamSetting,
};
use vacancyd::status as wire;

use crate::auth::{Denial, Grant, Scope};
use crate::config::Config;
use crate::state::{AppState, ObservationQuality};

/// Explicitly marks a state-changing administrative request.
///
/// This is not a credential. Its security property is that a cross-origin web
/// page cannot attach a non-safelisted header without a successful CORS
/// preflight, and this server exposes neither CORS permission nor an OPTIONS
/// route. Requiring it keeps a blind form/fetch from mutating the loopback
/// API. Which local caller may do what is decided separately, by
/// [`vacancyd::local_admin::permits`].
const ADMIN_INTENT_HEADER: &str = "x-vacancyd-admin";
const ADMIN_INTENT_VALUE: &str = "1";

pub async fn serve(state: Arc<AppState>) -> anyhow::Result<()> {
    let listen = state.config.server.listen.clone();
    let app = routes(state.clone());
    let listener = crate::listener::bind("api", &listen).await?;
    info!(listen, tokens = state.auth.token_count(), "api listening");
    warn_if_open(&state, &listen);
    // The listener attaches the peer and local addresses to each request:
    // loopback trust is a decision about both.
    crate::listener::serve(listener, app, &state.config.server.limits).await
}

fn routes(state: Arc<AppState>) -> Router {
    Router::new()
        .route("/status", get(status))
        .route("/healthz", get(|| async { "ok" }))
        .route("/readyz", get(readiness))
        .route("/metrics", get(metrics))
        .route("/admin/lending/{action}", post(set_lending))
        .route("/admin/work/{id}/release", post(release_work))
        .route("/admin/resources/{id}/{action}", post(set_resource))
        .route("/admin/settings", get(read_settings).post(save_settings))
        .route("/admin/debug/{action}", post(set_debug))
        .route("/admin/diagnostics", get(crate::diagnostics::export))
        .layer(axum::middleware::from_fn_with_state(state.clone(), guard))
        .with_state(state)
}

/// Say plainly, once, when the daemon is reachable off-box without
/// credentials. A quiet insecure default is the thing worth avoiding.
fn warn_if_open(state: &Arc<AppState>, listen: &str) {
    if state.auth.is_configured() {
        return;
    }
    let loopback_only = listen.starts_with("127.") || listen.starts_with("[::1]");
    if !loopback_only {
        warn!(
            listen,
            "no auth tokens configured: /status and /metrics are readable by anyone who can \
             reach this port, including owner presence. Admin endpoints are refused remotely. \
             See docs/authentication.md"
        );
    }
}

/// Map a path to the scope it needs.
///
/// Defaults to `Read` for anything unmatched, so a route added later fails
/// closed rather than open.
fn scope_for(path: &str) -> Option<Scope> {
    match path {
        "/healthz" | "/readyz" => None,
        p if p.starts_with("/admin") => Some(Scope::Admin),
        _ => Some(Scope::Read),
    }
}

async fn guard(
    State(state): State<Arc<AppState>>,
    ConnectInfo(peer): ConnectInfo<SocketAddr>,
    req: Request,
    next: Next,
) -> axum::response::Response {
    let Some(scope) = scope_for(req.uri().path()) else {
        return next.run(req).await;
    };
    if scope == Scope::Admin {
        let Some(reason) = admin_intent_denial_reason(req.headers()) else {
            return authorize(state, peer, req, next, scope).await;
        };
        state
            .metrics
            .auth_total
            .with_label_values(&[scope.as_str(), reason])
            .inc();
        warn!(
            scope = scope.as_str(),
            peer = %peer.ip(),
            path = req.uri().path(),
            reason,
            "request denied"
        );
        return admin_intent_denial();
    }
    authorize(state, peer, req, next, scope).await
}

async fn authorize(
    state: Arc<AppState>,
    peer: SocketAddr,
    req: Request,
    next: Next,
    scope: Scope,
) -> axum::response::Response {
    let listener_port = req
        .extensions()
        .get::<crate::listener::ListenerAddr>()
        .map(|listener| listener.0.port());
    match state
        .auth
        .check(scope, req.headers(), peer.ip(), listener_port)
    {
        Ok(grant) => {
            let mut req = req;
            if scope == Scope::Admin {
                // A token that reached the admin scope carries it; anything
                // else here is a direct loopback client with no account.
                let caller = match grant {
                    Grant::Token(_) => Caller::AdminToken,
                    Grant::Loopback | Grant::Unauthenticated => Caller::Loopback,
                };
                let action = local_action(req.method(), req.uri().path());
                let paused = state.paused_by.lock().unwrap().clone();
                if let Err(reason) = vacancyd::local_admin::permits(
                    state.local_admin_policy,
                    action,
                    &caller,
                    paused.as_ref(),
                ) {
                    state
                        .metrics
                        .auth_total
                        .with_label_values(&[scope.as_str(), reason.as_str()])
                        .inc();
                    warn!(
                        scope = scope.as_str(),
                        peer = %peer.ip(),
                        path = req.uri().path(),
                        reason = reason.as_str(),
                        "request denied"
                    );
                    return local_refusal_response(reason);
                }
                req.extensions_mut().insert(caller);
            }
            state
                .metrics
                .auth_total
                .with_label_values(&[scope.as_str(), "allowed"])
                .inc();
            tracing::debug!(scope = scope.as_str(), grant = grant.label(), "authorised");
            next.run(req).await
        }
        Err(denial) => {
            state
                .metrics
                .auth_total
                .with_label_values(&[scope.as_str(), denial.label()])
                .inc();
            warn!(
                scope = scope.as_str(),
                peer = %peer.ip(),
                path = req.uri().path(),
                reason = denial.label(),
                "request denied"
            );
            denial_response(&denial)
        }
    }
}

/// Which local admin action an HTTP admin request is. Anything unrecognised
/// is [`Action::Other`], which only an administrator may attempt.
fn local_action(method: &axum::http::Method, path: &str) -> Action {
    use axum::http::Method;
    let segments: Vec<&str> = path.trim_start_matches('/').split('/').collect();
    match (method, segments.as_slice()) {
        (&Method::GET, ["admin", "settings"]) => Action::ReadSettings,
        (&Method::GET, ["admin", "diagnostics"]) => Action::ReadDiagnostics,
        (&Method::POST, ["admin", "debug", "enable" | "disable"]) => Action::SetDebug,
        (&Method::POST, ["admin", "settings"]) => Action::SaveSettings,
        (&Method::POST, ["admin", "lending", "disable"]) => Action::Pause,
        (&Method::POST, ["admin", "lending", "enable"]) => Action::Resume,
        (&Method::POST, ["admin", "resources", _, _]) => Action::SetResource,
        (&Method::POST, ["admin", "work", _, "release"]) => Action::ReleaseWork,
        _ => Action::Other,
    }
}

/// An admin request the local-admin policy refuses. It is authenticated,
/// so this is a 403 that says how to get the change made instead.
fn local_refusal_response(reason: RefusalReason) -> axum::response::Response {
    (
        StatusCode::FORBIDDEN,
        Json(json!({
            "error": {
                "type": reason.as_str(),
                "message": "This change needs an administrator. A loopback connection does not show which account made it. Use `vacancyd admin-request`, which the service identifies by your account (on Windows from an elevated prompt or through the owner console's approval prompt; on Linux as root or a member of the admin group), or present an admin-scoped token.",
            }
        })),
    )
        .into_response()
}

fn admin_intent_denial_reason(headers: &axum::http::HeaderMap) -> Option<&'static str> {
    if headers.contains_key(axum::http::header::ORIGIN) {
        return Some("browser_origin");
    }
    headers
        .get(ADMIN_INTENT_HEADER)
        .is_none_or(|value| value.as_bytes() != ADMIN_INTENT_VALUE.as_bytes())
        .then_some("missing_intent")
}

fn admin_intent_denial() -> axum::response::Response {
    (
        StatusCode::FORBIDDEN,
        Json(json!({
            "error": {
                "message": "admin requests require explicit intent",
                "type": "forbidden",
            }
        })),
    )
        .into_response()
}

/// A denial names the scope required but never which tokens exist.
pub fn denial_response(denial: &Denial) -> axum::response::Response {
    let body = json!({
        "error": {
            "message": match denial {
                Denial::Missing => "authentication required",
                Denial::Invalid => "invalid credential",
                Denial::Forbidden => "credential does not carry the required scope",
                Denial::NotDirect => "loopback trust does not cover proxied or non-loopback-host requests; configure a token",
            },
            "type": "unauthorized",
        }
    });
    let mut resp = (denial.status(), Json(body)).into_response();
    if matches!(denial, Denial::Missing | Denial::Invalid) {
        resp.headers_mut().insert(
            axum::http::header::WWW_AUTHENTICATE,
            axum::http::HeaderValue::from_static("Bearer"),
        );
    }
    resp
}

async fn status(State(state): State<Arc<AppState>>) -> impl IntoResponse {
    Json(status_body(&state))
}

async fn readiness(State(state): State<Arc<AppState>>) -> axum::response::Response {
    if state.readiness_failures().is_empty() {
        (StatusCode::OK, "ready").into_response()
    } else {
        (StatusCode::SERVICE_UNAVAILABLE, "not ready").into_response()
    }
}

pub(crate) fn status_body(state: &AppState) -> wire::Status {
    // Read once. Building this response from several independent reads of
    // veto/lending/presence let one clear or toggle mid-build, producing a
    // combination the decoder correctly rejects as inconsistent (a spurious
    // "check versions" outage) even though nothing was actually wrong.
    let snapshot = crate::gate::Snapshot::read(state);
    let readiness_failures = state.readiness_failures();
    let resources: Vec<_> = state
        .config
        .resources
        .iter()
        .map(|rc| {
            let rs = &state.resources[&rc.id];
            let required_by = state
                .config
                .upstreams
                .iter()
                .filter(|upstream| {
                    upstream.resource == rc.id || upstream.also_requires.contains(&rc.id)
                })
                .map(|upstream| upstream.id.clone())
                .collect::<Vec<_>>();
            let required = !required_by.is_empty();
            let observation = state
                .capacity_view(&rc.id)
                .expect("configured resource has runtime state");
            // Retain the previous sample internally for diagnostics, but do not
            // expose it through the current-capacity fields once its observation
            // is no longer admissible.
            let sample = if observation.quality == ObservationQuality::Ready {
                observation.value.as_ref()
            } else {
                None
            };
            let pressure_observation =
                (required && rc.kind == "nvidia-gpu" && state.config.owner.gpu_pressure.enabled)
                    .then(|| state.pressure_view(&rc.id))
                    .flatten();
            let pressure_sample = pressure_observation.as_ref().and_then(|observation| {
                (observation.quality == ObservationQuality::Ready)
                    .then_some(observation.value.as_ref())
                    .flatten()
            });
            let mapping_observation =
                (required && rc.kind == "nvidia-gpu" && state.config.owner.gpu_pressure.enabled)
                    .then(|| state.adapter_mapping_view(&rc.id))
                    .flatten();
            let mapping_sample = mapping_observation.as_ref().and_then(|observation| {
                (observation.quality == ObservationQuality::Ready)
                    .then_some(observation.value.as_ref())
                    .flatten()
            });
            wire::Resource {
                id: rc.id.clone(),
                kind: rc.kind.clone(),
                probe: rs.probe_kind.into(),
                label: sample.map(|s| s.label.clone()),
                required,
                required_by,
                enabled: rs.enabled.load(Ordering::Relaxed),
                lendable: required && crate::gate::resource_lendable(state, rc),
                // A CPU has no byte capacity; its sample's zero bytes are a
                // placeholder, not a reading, and are not published.
                free_mib: sample
                    .filter(|_| rc.kind != "system-cpu")
                    .map(|s| s.free_bytes / (1024 * 1024)),
                total_mib: sample
                    .filter(|_| rc.kind != "system-cpu")
                    .map(|s| s.total_bytes / (1024 * 1024)),
                utilization_pct: sample.and_then(|s| s.utilization_pct),
                capacity: observation_status(&observation),
                pressure: pressure_observation
                    .as_ref()
                    .map(|observation| wire::Pressure {
                        observation: observation_status(observation),
                        non_lender_pct: pressure_sample.map(|v| v.non_lender_pct),
                        engaged: pressure_sample.map(|v| v.engaged),
                        threshold_pct: pressure_sample.map(|v| v.threshold_pct),
                        baseline_pct: pressure_sample.and_then(|v| v.baseline_pct),
                        target_pct: pressure_sample.and_then(|v| v.target_pct),
                        learned_samples: pressure_sample.map(|v| v.learned_samples),
                        learned_s: pressure_sample.map(|v| v.learned_s),
                        mode: state
                            .config
                            .owner
                            .gpu_pressure
                            .mode
                            .effective()
                            .as_str()
                            .into(),
                        calibrated: pressure_sample.map(|v| v.calibrated),
                        calibration_samples: pressure_sample.map(|v| v.calibration_samples),
                        calibration_required: state.config.owner.gpu_pressure.warmup_samples,
                        calibration_hint: pressure_sample.and_then(|v| v.calibration_hint.clone()),
                        above_for_s: pressure_sample.map(|v| v.above_for_s),
                    }),
                adapter_mapping: mapping_observation.as_ref().map(|observation| {
                    wire::AdapterMapping {
                        observation: observation_status(observation),
                        pci: mapping_sample.map(|mapping| mapping.pci.to_string()),
                        luid: mapping_sample.map(|mapping| mapping.luid.clone()),
                        source: mapping_sample.map(|mapping| match mapping.source {
                            crate::state::AdapterMappingSource::Config => "config".into(),
                            crate::state::AdapterMappingSource::Probe => "probe".into(),
                        }),
                    }
                }),
            }
        })
        .collect();
    let ledger = state.admission.lock().unwrap();
    let work = ledger.snapshot();
    let effective_upstreams: Vec<_> = state
        .config
        .upstreams
        .iter()
        .map(|upstream| ledger.effective_upstream(upstream))
        .collect();
    let upstreams: Vec<_> = effective_upstreams
        .iter()
        .map(|u| {
            let decision = crate::gate::check(
                state,
                &ledger,
                &snapshot,
                u,
                crate::gate::Demand::model(None),
            )
            .and_then(|admission| ledger.check(state, u).map(|_| admission));
            let residency = decision
                .as_ref()
                .ok()
                .and_then(|admission| admission.residency.clone());
            let refusal = decision.err().or_else(|| {
                work.iter()
                    .find(|entry| {
                        std::iter::once(&u.resource)
                            .chain(u.also_requires.iter())
                            .any(|r| entry.resources.contains(r))
                    })
                    .map(|_| crate::gate::Refusal {
                        reason: crate::gate::RefusalReason::ResourceBusy,
                        detail: "a job still owns this resource".into(),
                        resource: Some(u.resource.clone()),
                        retry_after_s: 2,
                    })
            });
            let rt = &state.upstreams[&u.id];
            let caps = rt.adapter.capabilities();
            let health = rt
                .health
                .read()
                .unwrap()
                .view(crate::collect::health::FRESH_FOR);
            let mut models = rt.models.read().unwrap().view(crate::lifecycle::FRESH_FOR);
            let listener = rt
                .listener
                .read()
                .unwrap()
                .view(crate::lifecycle::FRESH_FOR);
            if let Some(observed) = &mut models.value {
                for model in &mut observed.models {
                    if work.iter().any(|w| {
                        w.upstream == u.id && w.model == model.name && w.model_state == "running"
                    }) {
                        model.state = "running".into();
                    }
                }
            }
            wire::Upstream {
                models: if models.quality == ObservationQuality::Ready {
                    models
                        .value
                        .as_ref()
                        .map(|m| m.models.clone())
                        .unwrap_or_default()
                } else {
                    vec![]
                },
                model_observation: Some(observation_status(&models)),
                listener: wire::Listener {
                    state: if listener.quality == ObservationQuality::Ready {
                        listener
                            .value
                            .map(|state| state.as_str())
                            .unwrap_or("unknown")
                    } else {
                        "unknown"
                    }
                    .into(),
                    observation: observation_status(&listener),
                },
                unseen_gpu_work: Some(rt.unseen_gpu_work.read().unwrap().clone())
                    .filter(|resources| !resources.is_empty())
                    .map(|resources| wire::UnseenGpuWork { resources }),
                health: rt.adapter.health_path().map(|_| wire::TargetHealth {
                    http_status: (health.quality == ObservationQuality::Ready)
                        .then_some(health.value)
                        .flatten(),
                    observation: observation_status(&health),
                }),
                id: u.id.clone(),
                adapter: rt.adapter.kind().into(),
                can_unload: caps.can_unload,
                per_model_gating: caps.reports_model,
                listen: u.listen.clone(),
                target: u.target.to_string(),
                resource: u.resource.clone(),
                owner_scope: owner_scope_name(u.owner_scope).into(),
                vacancy: refusal.is_none(),
                residency: residency.filter(|_| refusal.is_none()),
                refusal: refusal.map(|r| wire::Refusal {
                    reason: match r.reason {
                        crate::gate::RefusalReason::LendingDisabled => "lending_disabled",
                        crate::gate::RefusalReason::ResourceDisabled => "resource_disabled",
                        crate::gate::RefusalReason::ResourceUnmeasured => "resource_unmeasured",
                        crate::gate::RefusalReason::ResourcePressured => "resource_pressured",
                        crate::gate::RefusalReason::OwnerActive => "owner_active",
                        crate::gate::RefusalReason::InsufficientCapacity => "insufficient_capacity",
                        crate::gate::RefusalReason::ResourceBusy => "resource_busy",
                    }
                    .into(),
                    detail: r.detail,
                    resource: r.resource,
                    retry_after_s: r.retry_after_s,
                }),
            }
        })
        .collect();
    drop(ledger);
    let presence = snapshot.presence.clone();
    let evidence = state.session_evidence(&presence);
    let observations = evidence.observations;
    let agent_sessions = presence
        .session_requirements
        .iter()
        .zip(observations.iter())
        .map(|(requirement, observation)| wire::AgentSession {
            session_id: requirement.session_id,
            report_state: if observation.is_some() {
                "fresh"
            } else {
                "missing"
            }
            .into(),
            agent_version: observation
                .as_ref()
                .map(|value| value.report.agent_version.clone()),
            protocol_version: observation
                .as_ref()
                .map(|value| value.report.protocol_version),
            age_s: observation
                .as_ref()
                .map(|value| value.sampled_at.elapsed().as_secs()),
        })
        .collect::<Vec<_>>();
    // Retain the pre-0.1 singular field for clients while exposing the full
    // multi-session authority object beside it.
    let agent = observations
        .iter()
        .flatten()
        .next()
        .map(|observation| wire::Agent {
            session_id: observation.report.session_id,
            agent_version: observation.report.agent_version.clone(),
            protocol_version: observation.report.protocol_version,
            age_s: observation.sampled_at.elapsed().as_secs(),
        });
    let usable_count = observations.iter().filter(|value| value.is_some()).count();
    let applicable_count = presence.applicable_session_ids.len();
    let last_rejection = state.last_agent_rejection();
    let last_rejection = last_rejection.map(|(reason, age)| wire::AgentRejection {
        reason: reason.as_str().into(),
        age_s: age.as_secs(),
    });
    wire::Status {
        schema_version: wire::SCHEMA_VERSION,
        daemon: wire::DaemonIdentity {
            name: "vacancyd".into(),
            version: env!("CARGO_PKG_VERSION").into(),
        },
        ready: readiness_failures.is_empty(),
        readiness_failures,
        lending_enabled: snapshot.lending_enabled,
        owner_veto: wire::OwnerVeto {
            active: snapshot.veto,
            cause: snapshot.veto_cause.clone(),
            exempt_upstreams: crate::config::veto_exempt_upstreams(&state.config)
                .map(|upstream| upstream.id.clone())
                .collect(),
        },
        presence: wire::Presence {
            session: match presence.session {
                crate::plugin::presence::SessionState::Active => "active",
                crate::plugin::presence::SessionState::Locked => "locked",
                crate::plugin::presence::SessionState::Disconnected => "disconnected",
                crate::plugin::presence::SessionState::LoggedOut => "logged_out",
                crate::plugin::presence::SessionState::Unknown => "unknown",
            }
            .into(),
            session_id: presence.session_id,
            applicable_session_ids: presence.applicable_session_ids,
            session_authority: authority_name(presence.session_authority).into(),
            foreground: presence.foreground,
            owner_app: presence.owner_app,
            idle_seconds: presence.idle_seconds,
            media_error: match &presence.media {
                crate::plugin::presence::MediaState::Unknown(error) => Some(error.clone()),
                _ => None,
            },
            media: match presence.media {
                crate::plugin::presence::MediaState::NotChecked => wire::Media::NotChecked,
                crate::plugin::presence::MediaState::Idle => wire::Media::Idle,
                crate::plugin::presence::MediaState::Playing(name) => wire::Media::Playing(name),
                crate::plugin::presence::MediaState::Unknown(_) => wire::Media::Unknown,
            },
            basis: presence.basis,
            sessions: presence
                .sessions
                .into_iter()
                .map(|session| wire::PresenceSession {
                    id: session.id,
                    class: session.class,
                    kind: session.kind,
                    remote: session.remote,
                    seat: session.seat,
                    state: session.state,
                    active: session.active,
                    locked: session.locked,
                    counted: session.counted,
                    reason: session.reason,
                })
                .collect(),
        },
        presence_backend: state
            .presence_kind
            .get()
            .copied()
            .unwrap_or("(pending)")
            .into(),
        agent,
        agent_channel: wire::AgentChannel {
            state: if cfg!(windows) {
                if state.agent_channel_listening.load(Ordering::Relaxed) {
                    "listening"
                } else {
                    "unavailable"
                }
            } else {
                "not_supported"
            }
            .into(),
            protocol_version: crate::agent::AGENT_PROTOCOL_VERSION,
            report_ttl_s: state.agent_report_fresh_for().as_secs(),
            active_connections: state.metrics.agent_active_connections.get(),
            connection_limit: crate::agent::AGENT_CONNECTION_LIMIT,
            last_rejection,
        },
        session_authority: wire::SessionAuthority {
            state: authority_name(evidence.authority).into(),
            policy: "all_active_sessions".into(),
            applicable_count,
            usable_count,
            sessions: agent_sessions,
        },
        resources,
        upstreams,
        uptime_s: state.started.elapsed().as_secs(),
        work,
        config_revision: state.active_config_revision.clone(),
        logging: Some(state.log.status()),
        admin_channel: Some(wire::AdminChannel {
            state: if cfg!(any(windows, target_os = "linux")) {
                if state.admin_channel_listening.load(Ordering::Relaxed) {
                    "listening"
                } else {
                    "unavailable"
                }
            } else {
                "not_supported"
            }
            .into(),
        }),
    }
}

fn owner_scope_name(scope: crate::config::OwnerScope) -> &'static str {
    match scope {
        crate::config::OwnerScope::Host => wire::OWNER_SCOPE_HOST,
        crate::config::OwnerScope::Resource => wire::OWNER_SCOPE_RESOURCE,
    }
}

fn authority_name(authority: crate::plugin::presence::SessionAuthority) -> &'static str {
    match authority {
        crate::plugin::presence::SessionAuthority::Ready => "ready",
        crate::plugin::presence::SessionAuthority::NotRequired => "not_required",
        crate::plugin::presence::SessionAuthority::Missing => "missing",
        crate::plugin::presence::SessionAuthority::Ambiguous => "ambiguous",
    }
}

fn observation_status<T>(observation: &crate::state::ObservationView<T>) -> wire::Observation {
    wire::Observation {
        quality: match observation.quality {
            ObservationQuality::Unknown => wire::Quality::Unknown,
            ObservationQuality::Ready => wire::Quality::Ready,
            ObservationQuality::Stale => wire::Quality::Stale,
        },
        age_s: observation.age.map(|age| age.as_secs()),
        last_error: observation.last_error.clone(),
    }
}

async fn metrics(State(state): State<Arc<AppState>>) -> impl IntoResponse {
    // Refresh switch gauges at scrape time so they are correct even between polls.
    state
        .metrics
        .lending_enabled
        .set(state.lending_enabled.load(Ordering::Relaxed) as i64);
    refresh_resource_readiness_metrics(&state);
    (
        [("Content-Type", "text/plain; version=0.0.4")],
        state.metrics.render(),
    )
}

fn refresh_resource_readiness_metrics(state: &AppState) {
    for rc in &state.config.resources {
        let Some(observation) = state.capacity_view(&rc.id) else {
            continue;
        };
        state
            .metrics
            .resource_observation_age_seconds
            .with_label_values(&[rc.id.as_str(), "capacity"])
            .set(
                observation
                    .age
                    .map(|age| age.as_secs() as i64)
                    .unwrap_or(-1),
            );
        for quality in [
            ObservationQuality::Unknown,
            ObservationQuality::Ready,
            ObservationQuality::Stale,
        ] {
            state
                .metrics
                .resource_readiness
                .with_label_values(&[rc.id.as_str(), "capacity", quality.as_str()])
                .set((observation.quality == quality) as i64);
        }

        if rc.kind == "nvidia-gpu" && state.config.owner.gpu_pressure.enabled {
            if let Some(mapping) = state.adapter_mapping_view(&rc.id) {
                state
                    .metrics
                    .resource_observation_age_seconds
                    .with_label_values(&[rc.id.as_str(), "adapter_mapping"])
                    .set(mapping.age.map(|age| age.as_secs() as i64).unwrap_or(-1));
                for quality in [
                    ObservationQuality::Unknown,
                    ObservationQuality::Ready,
                    ObservationQuality::Stale,
                ] {
                    state
                        .metrics
                        .resource_readiness
                        .with_label_values(&[rc.id.as_str(), "adapter_mapping", quality.as_str()])
                        .set((mapping.quality == quality) as i64);
                }
            }
            let Some(pressure) = state.pressure_view(&rc.id) else {
                continue;
            };
            state
                .metrics
                .resource_observation_age_seconds
                .with_label_values(&[rc.id.as_str(), "pressure"])
                .set(pressure.age.map(|age| age.as_secs() as i64).unwrap_or(-1));
            for quality in [
                ObservationQuality::Unknown,
                ObservationQuality::Ready,
                ObservationQuality::Stale,
            ] {
                state
                    .metrics
                    .resource_readiness
                    .with_label_values(&[rc.id.as_str(), "pressure", quality.as_str()])
                    .set((pressure.quality == quality) as i64);
            }
        }
    }
}

#[derive(serde::Deserialize)]
struct ReleaseWork {
    upstream_stopped: bool,
}

async fn release_work(
    State(state): State<Arc<AppState>>,
    Path(id): Path<u64>,
    Json(request): Json<ReleaseWork>,
) -> impl IntoResponse {
    if !request.upstream_stopped {
        return (
            StatusCode::BAD_REQUEST,
            "confirm upstream_stopped after stopping the job upstream",
        )
            .into_response();
    }
    match release_held_work(&state, id) {
        Ok(()) => StatusCode::NO_CONTENT.into_response(),
        Err(reason) => (StatusCode::CONFLICT, reason).into_response(),
    }
}

fn release_held_work(state: &AppState, id: u64) -> Result<(), &'static str> {
    let result = {
        let mut ledger = state.admission.lock().unwrap();
        let result = ledger.release(id);
        ledger.sync_active_metrics(state);
        result
    };
    if result.is_ok() {
        info!(
            entry = id,
            "administrator released held work after confirming upstream stopped"
        );
    }
    result
}

async fn set_lending(
    State(state): State<Arc<AppState>>,
    Extension(caller): Extension<Caller>,
    Path(action): Path<String>,
) -> impl IntoResponse {
    let on = match action.as_str() {
        "enable" => true,
        "disable" => false,
        _ => return (StatusCode::NOT_FOUND, "enable|disable").into_response(),
    };
    match change_lending(&state, on, &caller) {
        Ok(on) => Json(json!({ "lending_enabled": on })).into_response(),
        Err(reason) => local_refusal_response(reason),
    }
}

/// Pause or resume lending for `caller`, deciding and applying under one
/// lock so the record of who paused cannot change in between.
///
/// A second pause keeps the first pauser's record: otherwise pausing again
/// would hand the right to resume to whoever asked last.
pub(crate) fn change_lending(
    state: &AppState,
    on: bool,
    caller: &Caller,
) -> Result<bool, RefusalReason> {
    let mut paused = state.paused_by.lock().unwrap();
    let action = if on { Action::Resume } else { Action::Pause };
    vacancyd::local_admin::permits(state.local_admin_policy, action, caller, paused.as_ref())?;
    if on {
        *paused = None;
    } else if paused.is_none() {
        *paused = Some(PausedBy::from_caller(caller));
    }
    state.lending_enabled.store(on, Ordering::Relaxed);
    info!(
        lending = on,
        by = caller_label(caller),
        "global lending switch flipped"
    );
    Ok(on)
}

fn caller_label(caller: &Caller) -> &str {
    match caller {
        Caller::AdminToken => "admin token",
        Caller::Loopback => "loopback",
        Caller::Account { account, .. } => account,
    }
}

async fn set_resource(
    State(state): State<Arc<AppState>>,
    Path((id, action)): Path<(String, String)>,
) -> impl IntoResponse {
    let on = match action.as_str() {
        "enable" => true,
        "disable" => false,
        _ => return (StatusCode::NOT_FOUND, "enable|disable").into_response(),
    };
    if switch_resource(&state, &id, on) {
        Json(json!({ "resource": id, "enabled": on })).into_response()
    } else {
        (StatusCode::NOT_FOUND, format!("no resource '{id}'")).into_response()
    }
}

fn switch_resource(state: &AppState, id: &str, on: bool) -> bool {
    let Some(resource) = state.resources.get(id) else {
        return false;
    };
    resource.enabled.store(on, Ordering::Relaxed);
    info!(resource = id, enabled = on, "resource switch flipped");
    true
}

/// Carry out one admin-pipe or admin-socket request for a caller whose
/// account the channel verified. Lending is decided inside
/// [`change_lending`]; every other action is decided here, before anything
/// changes.
#[cfg_attr(not(any(windows, target_os = "linux")), allow(dead_code))]
pub(crate) fn perform_local_admin(
    state: &AppState,
    caller: &Caller,
    action: RequestedAction<serde_json::Value>,
) -> Response {
    let refused = |reason| Response::Refused { reason };
    let kind = action.action();
    if !matches!(kind, Action::Pause | Action::Resume) {
        if let Err(reason) =
            vacancyd::local_admin::permits(state.local_admin_policy, kind, caller, None)
        {
            warn!(
                action = kind.as_str(),
                by = caller_label(caller),
                reason = reason.as_str(),
                "local admin request denied"
            );
            return refused(reason);
        }
    }
    info!(
        action = kind.as_str(),
        by = caller_label(caller),
        "local admin request"
    );
    match action {
        RequestedAction::Pause {} | RequestedAction::Resume {} => {
            match change_lending(state, kind == Action::Resume, caller) {
                Ok(lending_enabled) => Response::Lending { lending_enabled },
                Err(reason) => refused(reason),
            }
        }
        RequestedAction::SetResource { resource, enabled } => {
            if switch_resource(state, &resource, enabled) {
                Response::Resource { resource, enabled }
            } else {
                refused(RefusalReason::NotFound)
            }
        }
        RequestedAction::ReleaseWork {
            upstream_stopped: false,
            ..
        } => refused(RefusalReason::Validation),
        RequestedAction::ReleaseWork { id, .. } => match release_held_work(state, id) {
            Ok(()) => Response::Released,
            Err(_) => refused(RefusalReason::Conflict),
        },
        RequestedAction::SaveSettings { settings } => match apply_settings(state, settings) {
            Ok(settings) => Response::Saved {
                restart_required: settings.application.restart_required,
                settings: Box::new(settings),
            },
            Err(SettingsFailure::Malformed | SettingsFailure::Invalid) => {
                refused(RefusalReason::Validation)
            }
            Err(SettingsFailure::Stale) => refused(RefusalReason::StaleRevision),
            Err(SettingsFailure::ConfigInvalid) => refused(RefusalReason::ConfigInvalid),
            Err(SettingsFailure::CredentialUnresolved) => {
                refused(RefusalReason::CredentialUnresolved)
            }
            Err(SettingsFailure::Persistence) => refused(RefusalReason::Persistence),
        },
        RequestedAction::StartDebug { minutes } => match open_debug_window(state, Some(minutes)) {
            Ok(opened) => Response::Logging {
                logging: opened.logging,
            },
            Err(_) => refused(RefusalReason::Validation),
        },
        RequestedAction::EndDebug {} => Response::Logging {
            logging: state.log.close_debug_window(),
        },
    }
}

/// The body of `POST /admin/debug/enable`. Empty, or naming the window's
/// length.
#[derive(Debug, Default, serde::Deserialize)]
#[serde(deny_unknown_fields)]
struct DebugRequest {
    minutes: Option<u64>,
}

async fn set_debug(
    State(state): State<Arc<AppState>>,
    Path(action): Path<String>,
    body: axum::body::Bytes,
) -> axum::response::Response {
    match action.as_str() {
        "enable" => {
            let request = if body.iter().all(u8::is_ascii_whitespace) {
                DebugRequest::default()
            } else {
                match serde_json::from_slice::<DebugRequest>(&body) {
                    Ok(request) => request,
                    Err(_) => return debug_validation_error(),
                }
            };
            match open_debug_window(&state, request.minutes) {
                Ok(opened) => Json(DebugOpened {
                    requested_minutes: request
                        .minutes
                        .unwrap_or(crate::log_control::DEBUG_WINDOW_DEFAULT.as_secs() / 60),
                    granted_minutes: opened.granted.as_secs() / 60,
                    limited: opened.limited,
                    logging: opened.logging,
                })
                .into_response(),
                Err(_) => debug_validation_error(),
            }
        }
        "disable" => Json(state.log.close_debug_window()).into_response(),
        _ => (StatusCode::NOT_FOUND, "enable|disable").into_response(),
    }
}

/// The answer to `POST /admin/debug/enable`: the logging state, and whether
/// the log budget shortened the window.
#[derive(serde::Serialize)]
struct DebugOpened {
    #[serde(flatten)]
    logging: vacancyd::status::Logging,
    requested_minutes: u64,
    granted_minutes: u64,
    limited: bool,
}

fn debug_validation_error() -> axum::response::Response {
    settings_error(
        StatusCode::BAD_REQUEST,
        "validation",
        "Send no body, or {\"minutes\": n} with n from 1 to 240.",
    )
}

/// Open a debug window of `minutes`, or of the default length.
fn open_debug_window(
    state: &AppState,
    minutes: Option<u64>,
) -> Result<crate::log_control::Opened, crate::log_control::InvalidDuration> {
    let duration = minutes.map_or(crate::log_control::DEBUG_WINDOW_DEFAULT, |minutes| {
        std::time::Duration::from_secs(minutes.saturating_mul(60))
    });
    state.log.open_debug_window(duration)
}

const MAX_CONFIG_BYTES: usize = 1024 * 1024;

#[derive(serde::Serialize)]
struct SavedSettings {
    #[serde(flatten)]
    settings: SettingsSnapshot,
    restart_required: bool,
}

type StrictSettingsUpdate = SettingsUpdate;

async fn read_settings(State(state): State<Arc<AppState>>) -> axum::response::Response {
    match load_runtime_settings(&state) {
        Ok(snapshot) => Json(snapshot).into_response(),
        Err(_) => settings_error(
            StatusCode::INTERNAL_SERVER_ERROR,
            "persistence",
            "Unable to read settings.",
        ),
    }
}

async fn save_settings(
    State(state): State<Arc<AppState>>,
    Json(raw): Json<serde_json::Value>,
) -> axum::response::Response {
    match apply_settings(&state, raw) {
        Ok(settings) => Json(SavedSettings {
            restart_required: settings.application.restart_required,
            settings,
        })
        .into_response(),
        Err(SettingsFailure::Malformed) => settings_error(
            StatusCode::BAD_REQUEST,
            "validation",
            "Settings request is invalid.",
        ),
        Err(SettingsFailure::Stale) => settings_error(
            StatusCode::CONFLICT,
            "stale_revision",
            "Settings changed; refresh and try again.",
        ),
        Err(SettingsFailure::Invalid) => settings_error(
            StatusCode::BAD_REQUEST,
            "validation",
            "Settings values are invalid.",
        ),
        Err(SettingsFailure::ConfigInvalid) => settings_error(
            StatusCode::INTERNAL_SERVER_ERROR,
            "config_invalid",
            "The proposed settings conflict with the configuration. Nothing was saved. Check the service log.",
        ),
        Err(SettingsFailure::CredentialUnresolved) => settings_error(
            StatusCode::INTERNAL_SERVER_ERROR,
            "credential_unresolved",
            "A saved settings value could not resolve its credential. Check the service log.",
        ),
        Err(SettingsFailure::Persistence) => settings_error(
            StatusCode::INTERNAL_SERVER_ERROR,
            "persistence",
            "Unable to save settings.",
        ),
    }
}

/// Validate and save one settings document, as sent to either the HTTP API
/// or the admin pipe.
fn apply_settings(
    state: &AppState,
    raw: serde_json::Value,
) -> Result<SettingsSnapshot, SettingsFailure> {
    let update = serde_json::from_value::<StrictSettingsUpdate>(raw)
        .map_err(|_| SettingsFailure::Malformed)?;
    let _write = state.settings_write.lock().unwrap();
    let (mut saved, config) =
        persist_settings_with_config(&state.config_path, update, Some(&state.config))?;
    // No filesystem or credential work under the admission lock. Keep the
    // settings-write guard through publication so later saves cannot overtake it.
    let mut ledger = state.admission.lock().unwrap();
    ledger.publish_capacity(&config, saved.revision.clone());
    saved.application = settings_application(state, &ledger, &config);
    Ok(saved)
}

fn settings_error(
    status: StatusCode,
    kind: &'static str,
    message: &'static str,
) -> axum::response::Response {
    (
        status,
        Json(json!({ "error": { "type": kind, "message": message } })),
    )
        .into_response()
}

#[derive(Debug, Clone, Copy)]
enum SettingsFailure {
    /// The request is not a complete settings document.
    Malformed,
    Stale,
    Invalid,
    /// The staged file no longer parses/validates as a config, even though
    /// the request passed `validate_update`. Valid individual fields can
    /// still conflict with each other or with protected configuration, such
    /// as differing owner scopes sharing one upstream server.
    ConfigInvalid,
    /// The staged config parses, but a credential it names (an auth token or
    /// an upstream credential) could not be resolved: a missing env var or
    /// an unreadable `value_file`.
    CredentialUnresolved,
    Persistence,
}

/// Server log only: the failure's root cause, without the outer
/// `with_context` layers that name the config file path, and bounded so a
/// pathological message cannot flood the log.
fn bounded_cause(error: &anyhow::Error) -> String {
    error.root_cause().to_string().chars().take(200).collect()
}

fn config_bytes(path: &FsPath) -> Result<Vec<u8>, SettingsFailure> {
    use std::io::Read;
    let file =
        crate::secure_file::open_regular_read(path).map_err(|_| SettingsFailure::Persistence)?;
    if file
        .metadata()
        .map_err(|_| SettingsFailure::Persistence)?
        .len()
        > MAX_CONFIG_BYTES as u64
    {
        return Err(SettingsFailure::Persistence);
    }
    let mut bytes = Vec::new();
    file.take(MAX_CONFIG_BYTES as u64 + 1)
        .read_to_end(&mut bytes)
        .map_err(|_| SettingsFailure::Persistence)?;
    (bytes.len() <= MAX_CONFIG_BYTES)
        .then_some(bytes)
        .ok_or(SettingsFailure::Persistence)
}

pub(crate) fn saved_settings_revision(path: &FsPath) -> Option<String> {
    config_bytes(path).ok().map(|bytes| revision(&bytes))
}

pub(crate) fn revision(bytes: &[u8]) -> String {
    use sha2::{Digest, Sha256};
    format!("{:x}", Sha256::digest(bytes))
}

/// The saved settings, for the admin socket's tests.
#[cfg(all(test, target_os = "linux"))]
pub(crate) fn current_settings(path: &FsPath) -> Option<SettingsSnapshot> {
    load_settings_snapshot(path).ok()
}

#[cfg(test)]
fn load_settings_snapshot(path: &FsPath) -> Result<SettingsSnapshot, SettingsFailure> {
    load_settings(path).map(|(snapshot, _)| snapshot)
}

fn load_settings(path: &FsPath) -> Result<(SettingsSnapshot, Config), SettingsFailure> {
    let bytes = config_bytes(path)?;
    let config = crate::config::Config::load(path).map_err(|error| {
        warn!(cause = %bounded_cause(&error), "settings read found the saved config invalid");
        SettingsFailure::Persistence
    })?;
    if config_bytes(path)? != bytes {
        return Err(SettingsFailure::Persistence);
    }
    let snapshot = snapshot_from_config(&config, revision(&bytes));
    if serde_json::to_vec(&snapshot)
        .map_err(|_| SettingsFailure::Persistence)?
        .len()
        > vacancyd::local_admin::MAX_RESPONSE_BYTES - 512
    {
        return Err(SettingsFailure::Persistence);
    }
    Ok((snapshot, config))
}

fn load_runtime_settings(state: &AppState) -> Result<SettingsSnapshot, SettingsFailure> {
    let _read = state.settings_write.lock().unwrap();
    let (mut saved, config) = load_settings(&state.config_path)?;
    let ledger = state.admission.lock().unwrap();
    saved.application = settings_application(state, &ledger, &config);
    Ok(saved)
}

fn settings_application(
    state: &AppState,
    ledger: &crate::admission::Ledger,
    saved: &Config,
) -> SettingsApplication {
    let mut comparison = saved.clone();
    // Compare complete parsed configuration, including protected references.
    // Only capacity is live; never mark another changed group active just
    // because that group has no editable renderer fields.
    for upstream in &mut comparison.upstreams {
        if let Some(original) = state
            .config
            .upstreams
            .iter()
            .find(|item| item.id == upstream.id)
        {
            upstream.default_min_free_mib = original.default_min_free_mib;
            upstream.models = original.models.clone();
        }
    }
    SettingsApplication {
        capacity_revision: ledger
            .capacity_revision()
            .map(str::to_owned)
            .or_else(|| state.active_config_revision.clone()),
        restart_required: state.config != comparison,
    }
}

pub(crate) fn snapshot_from_config(
    config: &crate::config::Config,
    revision: String,
) -> SettingsSnapshot {
    SettingsSnapshot {
        capabilities: SettingsCapabilities {
            auth_tokens_configured: config.auth.tokens.len() as u64,
            upstream_credentials_configured: config
                .upstreams
                .iter()
                .filter(|item| item.credential.is_some())
                .count() as u64,
            trust_loopback: config.auth.trust_loopback,
            allow_unauthenticated_remote: config.auth.allow_unauthenticated_remote,
            local_admin_channel: if cfg!(windows) {
                "windows-account"
            } else if cfg!(target_os = "linux") {
                "unix-account"
            } else {
                "unsupported"
            }
            .into(),
            local_admin_group_configured: cfg!(target_os = "linux")
                && !config.server.admin_group.is_empty(),
            custom_admission_storage: !config.server.admission_state_dir.is_empty(),
            custom_pressure_cache: !config.owner.gpu_pressure.state_file.is_empty(),
            custom_admin_socket: cfg!(target_os = "linux")
                && !config.server.admin_socket.is_empty(),
        },
        poll_interval_ms: config.poll.interval_ms,
        server_listen: config.server.listen.clone(),
        server_limits: ServerLimitsSettings {
            header_read_timeout_s: config.server.limits.header_read_timeout_s,
            body_read_timeout_s: config.server.limits.body_read_timeout_s,
            body_idle_timeout_s: config.server.limits.body_idle_timeout_s,
            write_stall_timeout_s: config.server.limits.write_stall_timeout_s,
            max_connections: config.server.limits.max_connections as u64,
            max_request_body_mib: config.server.limits.max_request_body_mib as u64,
            max_buffered_body_mib: config
                .server
                .limits
                .max_buffered_body_mib
                .map(|value| value as u64),
            tunnel_idle_timeout_s: config.server.limits.tunnel_idle_timeout_s,
        },
        application: SettingsApplication {
            capacity_revision: None,
            restart_required: true,
        },
        revision,
        owner: OwnerSettings {
            unlocked_session_is_presence: config.owner.unlocked_session_is_presence,
            idle_lends_after_s: config.owner.idle_lends_after_s,
            media_is_presence: config.owner.media_is_presence,
            release_after_s: config.owner.release_after_s,
            processes: config.owner.processes.clone(),
            media_ignore: config.owner.media_ignore.clone(),
            sign_in_screen_is_presence: config.owner.sign_in_screen_is_presence,
            presence: match config.owner.presence {
                crate::config::PresenceMode::Auto => "auto",
                crate::config::PresenceMode::Headless => "headless",
            }
            .into(),
            media_poll_ms: config.owner.media_poll_ms,
            gpu_pressure: GpuPressureSettings {
                enabled: config.owner.gpu_pressure.enabled,
                // A retired `adaptive` runs as calibrated and is offered as
                // such, so a save rewrites it.
                mode: config
                    .owner
                    .gpu_pressure
                    .mode
                    .effective()
                    .as_str()
                    .to_owned(),
                threshold_pct: config.owner.gpu_pressure.threshold_pct,
                sustain_s: config.owner.gpu_pressure.sustain_s,
                ignore: config.owner.gpu_pressure.ignore.clone(),
                cap_pct: config.owner.gpu_pressure.cap_pct,
                margin_pct: config.owner.gpu_pressure.margin_pct,
                floor_pct: config.owner.gpu_pressure.floor_pct,
                learn_quantile: config.owner.gpu_pressure.learn_quantile,
                learn_half_life_h: config.owner.gpu_pressure.learn_half_life_h,
                raise_pct_per_h: config.owner.gpu_pressure.raise_pct_per_h,
                warmup_samples: config.owner.gpu_pressure.warmup_samples as u64,
                poll_ms: config.owner.gpu_pressure.poll_ms,
            },
        },
        resources: config
            .resources
            .iter()
            .map(|resource| ResourceSetting {
                id: resource.id.clone(),
                kind: resource.kind.clone(),
                enabled: resource.enabled,
                reserve: resource.reserve.map(|value| value.to_string()),
                selector: resource.selector.clone(),
                adapter: resource.adapter.map(|value| value.to_string()),
            })
            .collect(),
        upstreams: config
            .upstreams
            .iter()
            .map(|upstream| UpstreamSetting {
                id: upstream.id.clone(),
                kind: upstream.kind.clone(),
                yield_on_veto: upstream.yield_on_veto,
                on_owner_return: match upstream.on_owner_return {
                    crate::config::ReclaimMode::Cancel => "cancel",
                    crate::config::ReclaimMode::Drain => "drain",
                    crate::config::ReclaimMode::Finish => "finish",
                }
                .to_owned(),
                drain_timeout_s: upstream.drain_timeout_s as u64,
                owner_scope: match upstream.owner_scope {
                    crate::config::OwnerScope::Host => "host",
                    crate::config::OwnerScope::Resource => "resource",
                }
                .to_owned(),
                completion_timeout_s: upstream.completion_timeout_s as u64,
                forward_authorization: upstream.forward_authorization,
                listen: upstream.listen.clone(),
                target: upstream.target.as_str().into(),
                resource: upstream.resource.clone(),
                also_requires: upstream.also_requires.clone(),
                default_min_free_mib: upstream.default_min_free_mib.to_string(),
                models: upstream
                    .models
                    .iter()
                    .map(|(name, requirement)| {
                        let (mode, min_free_mib, resources) = match &requirement.load {
                            crate::config::LoadRequirement::Scalar(mib) => {
                                ("scalar", mib.to_string(), vec![])
                            }
                            crate::config::LoadRequirement::PerResource(values) => (
                                "per_resource",
                                "0".into(),
                                values
                                    .iter()
                                    .map(|(id, mib)| ModelResourceSetting {
                                        id: id.clone(),
                                        min_free_mib: mib.to_string(),
                                    })
                                    .collect(),
                            ),
                        };
                        ModelSetting {
                            name: name.clone(),
                            mode: mode.into(),
                            min_free_mib,
                            resident_min_free_mib: requirement.resident_min_free_mib.to_string(),
                            resources,
                        }
                    })
                    .collect(),
                management: match upstream.management {
                    crate::config::ManagementPolicy::Deny => "deny",
                    crate::config::ManagementPolicy::Allow => "allow",
                }
                .to_owned(),
            })
            .collect(),
    }
}

#[cfg(test)]
fn persist_settings(
    path: &FsPath,
    update: SettingsUpdate,
) -> Result<SettingsSnapshot, SettingsFailure> {
    persist_settings_with_config(path, update, None).map(|(snapshot, _)| snapshot)
}

fn persist_settings_with_config(
    path: &FsPath,
    mut update: SettingsUpdate,
    active: Option<&crate::config::Config>,
) -> Result<(SettingsSnapshot, crate::config::Config), SettingsFailure> {
    validate_update(&mut update)?;
    let source = config_bytes(path)?;
    if revision(&source) != update.revision {
        return Err(SettingsFailure::Stale);
    }
    let source_text = std::str::from_utf8(&source).map_err(|_| SettingsFailure::Persistence)?;
    let config = crate::config::Config::load(path).map_err(|_| SettingsFailure::Persistence)?;
    check_membership(&config, &update)?;
    let mut document = source_text
        .parse::<toml_edit::Document>()
        .map_err(|_| SettingsFailure::Persistence)?;
    patch_document(&mut document, &update)?;
    let staged = document.to_string().into_bytes();
    // A successful save must remain readable by the settings API. Patching
    // can grow a valid source, including through TOML string escaping.
    if staged.len() > MAX_CONFIG_BYTES {
        return Err(SettingsFailure::Invalid);
    }
    #[cfg(windows)]
    refuse_unprotected_config(path)?;
    let stage_path = staging_path(path)?;
    let mut stage = StageFile::create(stage_path)?;
    {
        let file = stage.file.as_mut().ok_or(SettingsFailure::Persistence)?;
        file.write_all(&staged)
            .and_then(|_| file.sync_all())
            .map_err(|_| SettingsFailure::Persistence)?;
    }
    drop(stage.file.take());
    #[cfg(windows)]
    crate::windows_acl::protect_file(&stage.path).map_err(|_| SettingsFailure::Persistence)?;
    let staged_config = crate::config::Config::load(&stage.path).map_err(|error| {
        warn!(cause = %bounded_cause(&error), "settings save produced a config that failed to reload");
        SettingsFailure::ConfigInvalid
    })?;
    staged_config.resolve_credentials().map_err(|error| {
        warn!(cause = %bounded_cause(&error), "settings save produced a credential that failed to resolve");
        SettingsFailure::CredentialUnresolved
    })?;
    let snapshot = snapshot_from_config(&staged_config, revision(&staged));
    if serde_json::to_vec(&snapshot)
        .map_err(|_| SettingsFailure::Invalid)?
        .len()
        > vacancyd::local_admin::MAX_RESPONSE_BYTES - 512
    {
        return Err(SettingsFailure::Invalid);
    }
    if let Some(active) = active {
        // A saved dependency edit is restart-only. Capacity must also be valid
        // against the running dependencies before claiming it applies live.
        let mut effective = active.clone();
        for upstream in &mut effective.upstreams {
            if let Some(saved) = staged_config
                .upstreams
                .iter()
                .find(|saved| saved.id == upstream.id)
            {
                upstream.default_min_free_mib = saved.default_min_free_mib;
                upstream.models = saved.models.clone();
            }
        }
        effective
            .validate()
            .map_err(|_| SettingsFailure::ConfigInvalid)?;
    }
    if config_bytes(path)? != source {
        return Err(SettingsFailure::Stale);
    }
    replace_config(&stage.path, path)?;
    stage.keep = true;
    Ok((
        snapshot_from_config(&staged_config, revision(&staged)),
        staged_config,
    ))
}

/// On Windows the service saves settings as LocalSystem, staging a file
/// beside the configuration and replacing it, so the configuration's
/// directory must pass the same check as every other directory it writes
/// in: inside the protected `ProgramData\vacancyd` folder, through no
/// reparse point, and private to Administrators and LocalSystem. The caller
/// sees an ordinary save failure; the reason goes to the service log.
#[cfg(windows)]
fn refuse_unprotected_config(path: &FsPath) -> Result<(), SettingsFailure> {
    let directory = std::path::absolute(path)
        .ok()
        .and_then(|absolute| absolute.parent().map(FsPath::to_path_buf))
        .ok_or(SettingsFailure::Persistence)?;
    let reason = match crate::secure_file::replaceable_path(&directory) {
        Ok(None) => return Ok(()),
        Ok(Some(reason)) => reason,
        Err(error) => format!("the configuration directory could not be checked: {error}"),
    };
    warn!(
        path = %path.display(),
        reason,
        "settings not saved: the configuration directory did not pass the Windows path and permission checks"
    );
    Err(SettingsFailure::Persistence)
}

/// Replace the configuration durably. A failure after the rename cannot be
/// reported as "not saved": the new file is already the visible one. It is
/// still a persistence failure, because the save was not confirmed durable,
/// and the owner is told to refresh, which shows whichever revision is now
/// current.
fn replace_config(staged: &FsPath, destination: &FsPath) -> Result<(), SettingsFailure> {
    crate::durable::replace(staged, destination).map_err(|error| {
        warn!(%error, "settings save could not be confirmed durable");
        SettingsFailure::Persistence
    })
}

/// Canonical decimal capacity, lossless in JavaScript and representable in TOML.
fn decimal_capacity(value: &str) -> Result<i64, SettingsFailure> {
    if value.is_empty()
        || (value.len() > 1 && value.starts_with('0'))
        || !value.bytes().all(|byte| byte.is_ascii_digit())
    {
        return Err(SettingsFailure::Invalid);
    }
    value.parse::<i64>().map_err(|_| SettingsFailure::Invalid)
}

fn validate_tuning(update: &SettingsUpdate) -> Result<(), SettingsFailure> {
    const MAX_SAFE_INTEGER: u64 = 9_007_199_254_740_991;
    let pressure = &update.owner.gpu_pressure;
    let limits = &update.server_limits;
    let integers = [
        update.poll_interval_ms,
        update.owner.media_poll_ms,
        pressure.warmup_samples,
        pressure.poll_ms,
        limits.header_read_timeout_s,
        limits.body_read_timeout_s,
        limits.body_idle_timeout_s,
        limits.write_stall_timeout_s,
        limits.max_connections,
        limits.max_request_body_mib,
        limits.max_buffered_body_mib.unwrap_or(1),
        limits.tunnel_idle_timeout_s,
    ];
    if integers
        .iter()
        .any(|value| *value == 0 || *value > MAX_SAFE_INTEGER)
        || !matches!(update.owner.presence.as_str(), "auto" | "headless")
        || update.server_listen.len() > 256
        || [
            pressure.cap_pct,
            pressure.margin_pct,
            pressure.floor_pct,
            pressure.learn_quantile,
            pressure.learn_half_life_h,
            pressure.raise_pct_per_h,
        ]
        .iter()
        .any(|value| !value.is_finite())
    {
        return Err(SettingsFailure::Invalid);
    }
    for resource in &update.resources {
        if let Some(value) = &resource.reserve {
            decimal_capacity(value)?;
        }
        if resource
            .selector
            .as_ref()
            .is_some_and(|value| value.len() > 256)
            || resource
                .adapter
                .as_ref()
                .is_some_and(|value| value.len() > 32)
        {
            return Err(SettingsFailure::Invalid);
        }
    }
    for upstream in &update.upstreams {
        decimal_capacity(&upstream.default_min_free_mib)?;
        if upstream.completion_timeout_s > u32::MAX as u64
            || upstream.target.len() > 2048
            || upstream.listen.len() > 256
            || upstream.also_requires.len() > 64
            || upstream.models.len() > 128
        {
            return Err(SettingsFailure::Invalid);
        }
        let mut names = std::collections::BTreeSet::new();
        for model in &upstream.models {
            let load = decimal_capacity(&model.min_free_mib)?;
            let resident = decimal_capacity(&model.resident_min_free_mib)?;
            if model.name.is_empty()
                || model.name.len() > 256
                || !names.insert(&model.name)
                || model.resources.len() > 64
            {
                return Err(SettingsFailure::Invalid);
            }
            match model.mode.as_str() {
                "scalar" if model.resources.is_empty() && resident <= load => {}
                "per_resource" if load == 0 => {}
                _ => return Err(SettingsFailure::Invalid),
            }
            let mut resources = std::collections::BTreeSet::new();
            for resource in &model.resources {
                let load = decimal_capacity(&resource.min_free_mib)?;
                if !resources.insert(&resource.id)
                    || resident > load
                    || !update.resources.iter().any(|item| item.id == resource.id)
                    || (resource.id != upstream.resource
                        && !upstream.also_requires.contains(&resource.id))
                {
                    return Err(SettingsFailure::Invalid);
                }
            }
        }
    }
    Ok(())
}

fn validate_update(update: &mut SettingsUpdate) -> Result<(), SettingsFailure> {
    validate_tuning(update)?;
    if update.owner.idle_lends_after_s > 604_800
        || update.owner.release_after_s > 86_400
        || update.owner.gpu_pressure.sustain_s > 86_400
    {
        return Err(SettingsFailure::Invalid);
    }
    if !matches!(
        update.owner.gpu_pressure.mode.as_str(),
        "calibrated" | "fixed"
    ) {
        return Err(SettingsFailure::Invalid);
    }
    // A new save must not write the floor/cap combinations that only load
    // for compatibility. Fixed mode does not use them, and its console form
    // cannot edit them, so an older file stays saveable there.
    let pressure = &update.owner.gpu_pressure;
    if pressure.mode == "calibrated"
        && !(pressure.floor_pct <= pressure.cap_pct
            && pressure.cap_pct <= crate::plugin::pressure::MAX_CAP_PCT)
    {
        return Err(SettingsFailure::Invalid);
    }
    if !update.owner.gpu_pressure.threshold_pct.is_finite()
        || !(0.0..=100.0).contains(&update.owner.gpu_pressure.threshold_pct)
        || (update.owner.gpu_pressure.mode == "fixed"
            && update.owner.gpu_pressure.threshold_pct <= 0.0)
        || (update.owner.gpu_pressure.mode != "fixed"
            && update.owner.gpu_pressure.threshold_pct != 0.0)
        || update.upstreams.iter().any(|upstream| {
            upstream.drain_timeout_s > 86_400
                || !matches!(upstream.owner_scope.as_str(), "host" | "resource")
                || !matches!(upstream.management.as_str(), "deny" | "allow")
                || !matches!(
                    upstream.on_owner_return.as_str(),
                    "cancel" | "drain" | "finish"
                )
        })
    {
        return Err(SettingsFailure::Invalid);
    }
    for list in [
        &mut update.owner.processes,
        &mut update.owner.media_ignore,
        &mut update.owner.gpu_pressure.ignore,
    ] {
        if list.len() > 64 {
            return Err(SettingsFailure::Invalid);
        }
        for item in list.iter_mut() {
            *item = item.trim().to_owned();
            if item.is_empty() || item.chars().count() > 128 {
                return Err(SettingsFailure::Invalid);
            }
        }
    }
    Ok(())
}

fn check_membership(
    config: &crate::config::Config,
    update: &SettingsUpdate,
) -> Result<(), SettingsFailure> {
    let resources: std::collections::BTreeSet<_> = config
        .resources
        .iter()
        .map(|item| (item.id.as_str(), item.kind.as_str()))
        .collect();
    let upstreams: std::collections::BTreeSet<_> = config
        .upstreams
        .iter()
        .map(|item| (item.id.as_str(), item.kind.as_str()))
        .collect();
    let requested_resources: std::collections::BTreeSet<_> = update
        .resources
        .iter()
        .map(|item| (item.id.as_str(), item.kind.as_str()))
        .collect();
    let requested_upstreams: std::collections::BTreeSet<_> = update
        .upstreams
        .iter()
        .map(|item| (item.id.as_str(), item.kind.as_str()))
        .collect();
    if resources.len() != update.resources.len()
        || upstreams.len() != update.upstreams.len()
        || resources != requested_resources
        || upstreams != requested_upstreams
    {
        return Err(SettingsFailure::Invalid);
    }
    Ok(())
}

fn strings(values: &[String]) -> toml_edit::Array {
    let mut result = toml_edit::Array::new();
    for value in values {
        result.push(value.as_str());
    }
    result
}

/// Preserve surrounding comments and formatting when a whitelisted value changes.
fn set_setting(destination: &mut toml_edit::Item, mut proposed: toml_edit::Item) {
    if let (Some(old), Some(new)) = (destination.as_value(), proposed.as_value_mut()) {
        *new.decor_mut() = old.decor().clone();
        if old.to_string() == new.to_string()
            || old.as_integer().is_some_and(|value| {
                (-100..=100).contains(&value) && new.as_float() == Some(value as f64)
            })
        {
            return;
        }
    }
    *destination = proposed;
}

fn insert_setting(table: &mut dyn toml_edit::TableLike, key: &str, proposed: toml_edit::Item) {
    if let Some(destination) = table.get_mut(key) {
        set_setting(destination, proposed);
    } else {
        table.insert(key, proposed);
    }
}

/// Remove a setting without losing the comment above it: the key's leading
/// decor usually documents a neighbour too (an older file's calibration
/// paragraph sits above `baseline_window_s` but describes `warmup_samples`),
/// so it moves to the key that follows.
fn remove_setting(table: &mut dyn toml_edit::TableLike, key: &str) {
    let keys: Vec<String> = table.iter().map(|(name, _)| name.to_owned()).collect();
    let Some(index) = keys.iter().position(|name| name == key) else {
        return;
    };
    let leading = table
        .key_decor(key)
        .and_then(|decor| decor.prefix())
        .and_then(|prefix| prefix.as_str())
        .map(str::to_owned);
    table.remove(key);
    if let (Some(leading), Some(next)) = (leading, keys.get(index + 1)) {
        if let Some(decor) = table.key_decor_mut(next) {
            let own = decor
                .prefix()
                .and_then(|prefix| prefix.as_str())
                .unwrap_or_default();
            let joined = format!("{leading}{own}");
            decor.set_prefix(joined);
        }
    }
}

/// Insert new settings, each under a one-line comment, before `anchor` (or
/// at the end without it). A table keeps insertion order, so the keys from
/// `anchor` on are taken out with their formatting and put back after.
fn insert_settings_before(
    table: &mut dyn toml_edit::TableLike,
    anchor: &str,
    added: Vec<(&str, &str, toml_edit::Item)>,
) {
    if added.is_empty() {
        return;
    }
    let keys: Vec<String> = table.iter().map(|(name, _)| name.to_owned()).collect();
    let tail: Vec<(toml_edit::Key, toml_edit::Item)> = keys
        .iter()
        .skip_while(|name| name.as_str() != anchor)
        .filter_map(|name| {
            let (key, item) = table.get_key_value(name)?;
            Some((key.clone(), item.clone()))
        })
        .collect();
    for (key, _) in &tail {
        table.remove(key.get());
    }
    for (index, (name, comment, item)) in added.into_iter().enumerate() {
        let mut key = toml_edit::Key::new(name);
        // A blank line sets the group apart from the setting before it.
        let gap = if index == 0 { "\n" } else { "" };
        key.decor_mut().set_prefix(format!("{gap}# {comment}\n"));
        table.entry_format(&key).or_insert(item);
    }
    for (key, item) in tail {
        table.entry_format(&key).or_insert(item);
    }
}

fn patch_document(
    document: &mut toml_edit::Document,
    update: &SettingsUpdate,
) -> Result<(), SettingsFailure> {
    use toml_edit::{table, value};
    if document.get("poll").is_none() {
        document["poll"] = table();
    }
    set_setting(
        &mut document["poll"]["interval_ms"],
        value(update.poll_interval_ms as i64),
    );
    set_setting(
        &mut document["server"]["listen"],
        value(update.server_listen.as_str()),
    );
    if document["server"].get("limits").is_none() {
        document["server"]["limits"] = table();
    }
    let limits = document["server"]["limits"]
        .as_table_like_mut()
        .ok_or(SettingsFailure::Persistence)?;
    insert_setting(
        limits,
        "header_read_timeout_s",
        value(update.server_limits.header_read_timeout_s as i64),
    );
    insert_setting(
        limits,
        "body_read_timeout_s",
        value(update.server_limits.body_read_timeout_s as i64),
    );
    insert_setting(
        limits,
        "body_idle_timeout_s",
        value(update.server_limits.body_idle_timeout_s as i64),
    );
    insert_setting(
        limits,
        "write_stall_timeout_s",
        value(update.server_limits.write_stall_timeout_s as i64),
    );
    insert_setting(
        limits,
        "max_connections",
        value(update.server_limits.max_connections as i64),
    );
    insert_setting(
        limits,
        "max_request_body_mib",
        value(update.server_limits.max_request_body_mib as i64),
    );
    insert_setting(
        limits,
        "tunnel_idle_timeout_s",
        value(update.server_limits.tunnel_idle_timeout_s as i64),
    );
    if let Some(value_mib) = update.server_limits.max_buffered_body_mib {
        insert_setting(limits, "max_buffered_body_mib", value(value_mib as i64));
    } else {
        limits.remove("max_buffered_body_mib");
    }
    if document.get("owner").is_none() {
        document["owner"] = table();
    }
    let owner = document
        .get_mut("owner")
        .and_then(toml_edit::Item::as_table_like_mut)
        .ok_or(SettingsFailure::Persistence)?;
    insert_setting(
        owner,
        "unlocked_session_is_presence",
        value(update.owner.unlocked_session_is_presence),
    );
    insert_setting(
        owner,
        "idle_lends_after_s",
        value(update.owner.idle_lends_after_s as i64),
    );
    insert_setting(
        owner,
        "media_is_presence",
        value(update.owner.media_is_presence),
    );
    insert_setting(
        owner,
        "media_poll_ms",
        value(update.owner.media_poll_ms as i64),
    );
    insert_setting(owner, "presence", value(update.owner.presence.as_str()));
    insert_setting(
        owner,
        "release_after_s",
        value(update.owner.release_after_s as i64),
    );
    insert_setting(owner, "processes", value(strings(&update.owner.processes)));
    insert_setting(
        owner,
        "media_ignore",
        value(strings(&update.owner.media_ignore)),
    );
    insert_setting(
        owner,
        "sign_in_screen_is_presence",
        value(update.owner.sign_in_screen_is_presence),
    );
    if owner.get("gpu_pressure").is_none() {
        insert_setting(owner, "gpu_pressure", table());
    }
    let pressure = owner
        .get_mut("gpu_pressure")
        .and_then(toml_edit::Item::as_table_like_mut)
        .ok_or(SettingsFailure::Persistence)?;
    insert_setting(
        pressure,
        "enabled",
        value(update.owner.gpu_pressure.enabled),
    );
    insert_setting(
        pressure,
        "mode",
        value(update.owner.gpu_pressure.mode.as_str()),
    );
    insert_setting(
        pressure,
        "threshold_pct",
        value(update.owner.gpu_pressure.threshold_pct),
    );
    insert_setting(
        pressure,
        "sustain_s",
        value(update.owner.gpu_pressure.sustain_s as i64),
    );
    insert_setting(
        pressure,
        "ignore",
        value(strings(&update.owner.gpu_pressure.ignore)),
    );
    insert_setting(
        pressure,
        "cap_pct",
        value(update.owner.gpu_pressure.cap_pct),
    );
    insert_setting(
        pressure,
        "margin_pct",
        value(update.owner.gpu_pressure.margin_pct),
    );
    insert_setting(
        pressure,
        "floor_pct",
        value(update.owner.gpu_pressure.floor_pct),
    );
    // Deprecated and ignored since the threshold became learned; a save
    // cleans them out of an older file instead of carrying them forward.
    for deprecated in ["baseline_window_s", "baseline_quantile"] {
        remove_setting(pressure, deprecated);
    }
    insert_setting(
        pressure,
        "warmup_samples",
        value(update.owner.gpu_pressure.warmup_samples as i64),
    );
    insert_setting(
        pressure,
        "poll_ms",
        value(update.owner.gpu_pressure.poll_ms as i64),
    );
    let learning = [
        (
            "learn_quantile",
            "Quantile of the learned load the threshold is built on, between 0 and 1.",
            value(update.owner.gpu_pressure.learn_quantile),
        ),
        (
            "learn_half_life_h",
            "Observed hours after which a learned reading counts half as much.",
            value(update.owner.gpu_pressure.learn_half_life_h),
        ),
        (
            "raise_pct_per_h",
            "Most the threshold may rise per observed hour, in percentage points.",
            value(update.owner.gpu_pressure.raise_pct_per_h),
        ),
    ];
    let mut added = Vec::new();
    for (key, comment, proposed) in learning {
        match pressure.get_mut(key) {
            Some(destination) => set_setting(destination, proposed),
            None => added.push((key, comment, proposed)),
        }
    }
    insert_settings_before(pressure, "warmup_samples", added);

    // A configuration may have no `[[upstream]]` (the MSI starter has none);
    // `check_membership` has already required the update to list the same
    // entries, so an absent table has nothing to patch.
    let resources = match document.get_mut("resource") {
        Some(item) => item
            .as_array_of_tables_mut()
            .ok_or(SettingsFailure::Persistence)?,
        None if update.resources.is_empty() => return patch_upstreams(document, update),
        None => return Err(SettingsFailure::Persistence),
    };
    for item in resources.iter_mut() {
        let id = item
            .get("id")
            .and_then(toml_edit::Item::as_str)
            .ok_or(SettingsFailure::Persistence)?;
        if let Some(update) = update.resources.iter().find(|update| update.id == id) {
            set_setting(&mut item["enabled"], value(update.enabled));
            if let Some(reserve) = &update.reserve {
                set_setting(&mut item["reserve"], value(decimal_capacity(reserve)?));
            } else {
                item.remove("reserve");
            }
            for (key, text) in [("selector", &update.selector), ("adapter", &update.adapter)] {
                if let Some(text) = text {
                    set_setting(&mut item[key], value(text.as_str()));
                } else {
                    item.remove(key);
                }
            }
        }
    }
    patch_upstreams(document, update)
}

fn patch_upstreams(
    document: &mut toml_edit::Document,
    update: &SettingsUpdate,
) -> Result<(), SettingsFailure> {
    use toml_edit::value;
    let upstreams = match document.get_mut("upstream") {
        Some(item) => item
            .as_array_of_tables_mut()
            .ok_or(SettingsFailure::Persistence)?,
        None if update.upstreams.is_empty() => return Ok(()),
        None => return Err(SettingsFailure::Persistence),
    };
    for item in upstreams.iter_mut() {
        let id = item
            .get("id")
            .and_then(toml_edit::Item::as_str)
            .ok_or(SettingsFailure::Persistence)?;
        if let Some(update) = update.upstreams.iter().find(|update| update.id == id) {
            set_setting(&mut item["yield_on_veto"], value(update.yield_on_veto));
            set_setting(
                &mut item["on_owner_return"],
                value(update.on_owner_return.as_str()),
            );
            set_setting(
                &mut item["drain_timeout_s"],
                value(update.drain_timeout_s as i64),
            );
            set_setting(&mut item["owner_scope"], value(update.owner_scope.as_str()));
            set_setting(&mut item["management"], value(update.management.as_str()));
            set_setting(
                &mut item["completion_timeout_s"],
                value(update.completion_timeout_s as i64),
            );
            set_setting(
                &mut item["forward_authorization"],
                value(update.forward_authorization),
            );
            set_setting(&mut item["listen"], value(update.listen.as_str()));
            set_setting(&mut item["target"], value(update.target.as_str()));
            set_setting(&mut item["resource"], value(update.resource.as_str()));
            set_setting(
                &mut item["also_requires"],
                value(strings(&update.also_requires)),
            );
            set_setting(
                &mut item["default_min_free_mib"],
                value(decimal_capacity(&update.default_min_free_mib)?),
            );
            let mut models = toml_edit::Table::new();
            for model in &update.models {
                let mut requirement = toml_edit::InlineTable::new();
                requirement.insert(
                    "resident_min_free_mib",
                    decimal_capacity(&model.resident_min_free_mib)?.into(),
                );
                if model.mode == "scalar" {
                    requirement.insert(
                        "min_free_mib",
                        decimal_capacity(&model.min_free_mib)?.into(),
                    );
                } else {
                    for resource in &model.resources {
                        requirement.insert(
                            &resource.id,
                            decimal_capacity(&resource.min_free_mib)?.into(),
                        );
                    }
                }
                models.insert(&model.name, value(requirement));
            }
            item["models"] = toml_edit::Item::Table(models);
        }
    }
    Ok(())
}

struct StageFile {
    path: PathBuf,
    file: Option<std::fs::File>,
    keep: bool,
}
impl StageFile {
    fn create(path: PathBuf) -> Result<Self, SettingsFailure> {
        let file =
            crate::secure_file::create_new(&path).map_err(|_| SettingsFailure::Persistence)?;
        Ok(Self {
            path,
            file: Some(file),
            keep: false,
        })
    }
}
impl Drop for StageFile {
    fn drop(&mut self) {
        if !self.keep {
            let _ = std::fs::remove_file(&self.path);
        }
    }
}
fn staging_path(path: &FsPath) -> Result<PathBuf, SettingsFailure> {
    let name = path
        .file_name()
        .and_then(|name| name.to_str())
        .ok_or(SettingsFailure::Persistence)?;
    Ok(path.with_file_name(format!(
        ".{name}.settings-{}-{}.tmp",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map_err(|_| SettingsFailure::Persistence)?
            .as_nanos()
    )))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::Config;
    use crate::plugin::pressure::PressureVerdict;
    use crate::plugin::probe::Capacity;
    use crate::state::{AdapterMapping, AdapterMappingSource};
    use axum::body::Body;
    use axum::http::{HeaderMap, HeaderValue, Method, Request};
    use tower::ServiceExt;

    struct TestDirectory(PathBuf);
    impl Drop for TestDirectory {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    fn settings_file(extra_auth: &str) -> (TestDirectory, PathBuf) {
        let directory = crate::secure_file::test_root().join(format!(
            "vacancyd-settings-test-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        std::fs::create_dir(&directory).unwrap();
        let path = directory.join("vacancyd.toml");
        let source = concat!(
            "# preserved heading\n[server]\nlisten = \"127.0.0.1:9811\" # preserve\n\n",
            "# preserved owner comment\n[owner]\nunlocked_session_is_presence = true\n",
            "idle_lends_after_s = 0\nmedia_is_presence = true\nrelease_after_s = 30\n",
            "processes = [\"game\"]\nmedia_ignore = []\n\n[owner.gpu_pressure]\n",
            "enabled = true\nmode = \"calibrated\"\nthreshold_pct = 0\nsustain_s = 5\n",
            "ignore = []\ncap_pct = 70 # outside the whitelist\n\n[[resource]]\n",
            "id = \"gpu0\"\nkind = \"nvidia-gpu\"\nenabled = true\nselector = \"0\"\n\n",
            "[[upstream]]\nid = \"ollama\"\nkind = \"ollama\"\nlisten = \"127.0.0.1:11435\"\n",
            "target = \"http://127.0.0.1:11434\" # outside the whitelist\nresource = \"gpu0\"\n",
            "yield_on_veto = false\non_owner_return = \"drain\"\ndrain_timeout_s = 30\n",
            "default_min_free_mib = 8000\n"
        );
        std::fs::write(&path, format!("{source}{extra_auth}")).unwrap();
        (TestDirectory(directory), path)
    }

    fn settings_update(snapshot: SettingsSnapshot) -> SettingsUpdate {
        let mut update: SettingsUpdate = snapshot.into();
        update.owner.unlocked_session_is_presence = false;
        update.owner.idle_lends_after_s = 60;
        update.owner.media_is_presence = false;
        update.owner.release_after_s = 45;
        update.owner.processes = vec!["  blender  ".into()];
        update.owner.media_ignore = vec![" player ".into()];
        update.owner.sign_in_screen_is_presence = true;
        update.owner.gpu_pressure.enabled = true;
        update.owner.gpu_pressure.mode = "fixed".into();
        update.owner.gpu_pressure.threshold_pct = 60.0;
        update.owner.gpu_pressure.sustain_s = 8;
        update.owner.gpu_pressure.ignore = vec![" overlay ".into()];
        update.resources[0].enabled = false;
        update.upstreams[0].yield_on_veto = true;
        update.upstreams[0].on_owner_return = "cancel".into();
        update.upstreams[0].drain_timeout_s = 12;
        update
    }

    fn unchanged_update(path: &FsPath) -> SettingsUpdate {
        load_settings_snapshot(path).unwrap().into()
    }

    #[test]
    fn capacity_only_save_publishes_after_durable_write_without_marking_restart_policy_active() {
        let (_directory, path) = settings_file("");
        let state = AppState::new_with_config_path(Config::load(&path).unwrap(), &path).unwrap();
        let startup_revision = state.active_config_revision.clone();
        let mut update = unchanged_update(&path);
        update.upstreams[0].default_min_free_mib = "2800".into();
        let saved = apply_settings(&state, serde_json::to_value(update).unwrap()).unwrap();
        assert!(!saved.application.restart_required);
        assert_eq!(
            saved.application.capacity_revision.as_deref(),
            Some(saved.revision.as_str())
        );
        assert_eq!(revision(&std::fs::read(&path).unwrap()), saved.revision);
        assert_eq!(state.active_config_revision, startup_revision);
        let ledger = state.admission.lock().unwrap();
        assert_eq!(
            ledger
                .effective_upstream(&state.config.upstreams[0])
                .default_min_free_mib,
            2800
        );
    }

    #[cfg(unix)]
    #[test]
    fn a_failed_staging_write_does_not_publish_capacity() {
        let (_directory, original_path) = settings_file("");
        // The source fits the filesystem's filename limit, but its staging
        // suffix does not. Unlike directory permissions, root cannot bypass
        // this creation failure.
        let path = original_path.with_file_name("c".repeat(255));
        std::fs::rename(&original_path, &path).unwrap();
        let state = AppState::new_with_config_path(Config::load(&path).unwrap(), &path).unwrap();
        let before = std::fs::read(&path).unwrap();
        let mut update = unchanged_update(&path);
        update.upstreams[0].default_min_free_mib = "2800".into();
        let parent = path.parent().unwrap();
        let result = apply_settings(&state, serde_json::to_value(update).unwrap());
        assert!(matches!(result, Err(SettingsFailure::Persistence)));
        assert_eq!(std::fs::read(&path).unwrap(), before);
        assert!(state
            .admission
            .lock()
            .unwrap()
            .capacity_revision()
            .is_none());
        assert_eq!(std::fs::read_dir(parent).unwrap().count(), 1);
    }

    #[test]
    fn status_verdict_uses_the_published_capacity_snapshot() {
        let mut config = test_state().config.clone();
        config.owner.gpu_pressure.enabled = false;
        let state = AppState::new(config).unwrap();
        state.mark_presence_sampled();
        state.resources["gpu0"]
            .capacity
            .write()
            .unwrap()
            .record_success(crate::plugin::probe::Capacity {
                free_bytes: 7000 * 1024 * 1024,
                ..Default::default()
            });
        assert!(!status_body(&state).upstreams[0].vacancy);
        let mut changed = state.config.clone();
        changed.upstreams[0].default_min_free_mib = 2800;
        state
            .admission
            .lock()
            .unwrap()
            .publish_capacity(&changed, "capacity-only".into());
        assert!(status_body(&state).upstreams[0].vacancy);
        assert_eq!(state.config.upstreams[0].default_min_free_mib, 8000);
    }

    /// The service saves settings as LocalSystem, so on Windows a
    /// configuration outside the protected folder is not staged or replaced,
    /// even when everything else about the save is valid.
    #[cfg(windows)]
    #[test]
    fn windows_settings_saves_refuse_a_configuration_outside_the_protected_folder() {
        let (directory, inside) = settings_file("");
        let outside = crate::secure_file::protected_directory_outside_test_root();
        for entry in std::fs::read_dir(&directory.0).unwrap() {
            let entry = entry.unwrap();
            std::fs::copy(entry.path(), outside.join(entry.file_name())).unwrap();
        }
        let path = outside.join(inside.file_name().unwrap());
        let before = std::fs::read(&path).unwrap();
        let names = |directory: &FsPath| {
            let mut names: Vec<_> = std::fs::read_dir(directory)
                .unwrap()
                .map(|entry| entry.unwrap().file_name())
                .collect();
            names.sort();
            names
        };
        let files = names(&outside);
        // The same save succeeds inside the protected folder.
        persist_settings(&inside, unchanged_update(&inside)).unwrap();

        let log = crate::test_log::capture();
        let refused = persist_settings(&path, unchanged_update(&path));
        let logged = log.contents();
        drop(log);

        assert!(matches!(refused, Err(SettingsFailure::Persistence)));
        assert!(logged.contains("is not inside"), "{logged}");
        assert_eq!(std::fs::read(&path).unwrap(), before);
        assert_eq!(names(&outside), files, "a staging file was left or made");
        std::fs::remove_dir_all(outside).unwrap();
    }

    #[test]
    fn reserved_model_metadata_cannot_be_supplied_as_a_resource_budget() {
        let (_directory, path) = settings_file("");
        let mut update = unchanged_update(&path);
        update.upstreams[0].models = vec![ModelSetting {
            name: "model".into(),
            mode: "per_resource".into(),
            min_free_mib: "0".into(),
            resident_min_free_mib: "0".into(),
            resources: vec![ModelResourceSetting {
                id: "resident_min_free_mib".into(),
                min_free_mib: "100".into(),
            }],
        }];
        assert!(matches!(
            persist_settings(&path, update),
            Err(SettingsFailure::Invalid)
        ));
    }

    #[test]
    fn protected_reference_changes_are_restart_only_without_revealing_paths() {
        let (_directory, path) = settings_file("");
        let before = std::fs::read_to_string(&path).unwrap().replace(
            "[owner.gpu_pressure]",
            "[owner.gpu_pressure]\nstate_file = \"first-cache.json\"",
        );
        std::fs::write(&path, &before).unwrap();
        let state = AppState::new_with_config_path(Config::load(&path).unwrap(), &path).unwrap();
        std::fs::write(
            &path,
            before.replace("first-cache.json", "second-cache.json"),
        )
        .unwrap();
        let saved = load_runtime_settings(&state).unwrap();
        assert!(saved.application.restart_required);
        let wire = serde_json::to_string(&saved).unwrap();
        assert!(!wire.contains("first-cache.json"));
        assert!(!wire.contains("second-cache.json"));
    }

    #[test]
    fn mixed_save_keeps_reclaim_and_permissions_at_startup_values() {
        let (_directory, path) = settings_file("");
        let state = AppState::new_with_config_path(Config::load(&path).unwrap(), &path).unwrap();
        let mut update = unchanged_update(&path);
        update.owner.release_after_s = 70;
        update.upstreams[0].owner_scope = "resource".into();
        update.upstreams[0].default_min_free_mib = "2800".into();
        let saved = apply_settings(&state, serde_json::to_value(update).unwrap()).unwrap();
        assert!(saved.application.restart_required);
        let effective = state
            .admission
            .lock()
            .unwrap()
            .effective_upstream(&state.config.upstreams[0]);
        assert_eq!(effective.default_min_free_mib, 2800);
        assert_eq!(effective.owner_scope, crate::config::OwnerScope::Host);
        assert_eq!(
            effective.on_owner_return,
            state.config.upstreams[0].on_owner_return
        );
    }

    #[test]
    fn failed_save_cannot_publish_capacity_or_leave_a_staging_file() {
        let (_directory, path) = settings_file("");
        let state = AppState::new_with_config_path(Config::load(&path).unwrap(), &path).unwrap();
        let before = std::fs::read(&path).unwrap();
        let mut update = unchanged_update(&path);
        update.upstreams[0].default_min_free_mib = "2800".into();
        update.server_limits.max_connections = 0;
        assert!(apply_settings(&state, serde_json::to_value(update).unwrap()).is_err());
        assert!(state
            .admission
            .lock()
            .unwrap()
            .capacity_revision()
            .is_none());
        assert_eq!(std::fs::read(&path).unwrap(), before);
        assert_eq!(
            std::fs::read_dir(path.parent().unwrap()).unwrap().count(),
            1
        );
    }

    #[test]
    fn concurrent_saves_have_one_winner_and_its_durable_revision_is_published() {
        let (_directory, path) = settings_file("");
        let state =
            Arc::new(AppState::new_with_config_path(Config::load(&path).unwrap(), &path).unwrap());
        let barrier = Arc::new(std::sync::Barrier::new(3));
        let initial = unchanged_update(&path);
        let threads: Vec<_> = [2800, 3200]
            .into_iter()
            .map(|mib| {
                let state = state.clone();
                let barrier = barrier.clone();
                let mut update = initial.clone();
                update.upstreams[0].default_min_free_mib = mib.to_string();
                std::thread::spawn(move || {
                    barrier.wait();
                    apply_settings(&state, serde_json::to_value(update).unwrap())
                })
            })
            .collect();
        barrier.wait();
        let results: Vec<_> = threads
            .into_iter()
            .map(|thread| thread.join().unwrap())
            .collect();
        assert_eq!(results.iter().filter(|result| result.is_ok()).count(), 1);
        assert_eq!(
            results
                .iter()
                .filter(|result| matches!(result, Err(SettingsFailure::Stale)))
                .count(),
            1
        );
        let saved = load_runtime_settings(&state).unwrap();
        assert_eq!(
            saved.application.capacity_revision.as_deref(),
            Some(saved.revision.as_str())
        );
        assert_eq!(saved.revision, revision(&std::fs::read(&path).unwrap()));
    }

    #[test]
    fn settings_capacity_strings_reject_lossy_and_noncanonical_values_without_writing() {
        let (_directory, path) = settings_file("");
        let before = std::fs::read(&path).unwrap();
        for value in [
            "",
            "-1",
            "1.5",
            "01",
            " 1",
            "+1",
            "1e3",
            "9223372036854775808",
        ] {
            let mut update = unchanged_update(&path);
            update.upstreams[0].default_min_free_mib = value.into();
            assert!(
                matches!(
                    persist_settings(&path, update),
                    Err(SettingsFailure::Invalid)
                ),
                "{value}"
            );
            assert_eq!(std::fs::read(&path).unwrap(), before);
        }
        let mut update = unchanged_update(&path);
        update.upstreams[0].default_min_free_mib = "9007199254740993".into();
        let saved = persist_settings(&path, update).unwrap();
        assert_eq!(saved.upstreams[0].default_min_free_mib, "9007199254740993");
        assert_eq!(
            Config::load(&path).unwrap().upstreams[0].default_min_free_mib,
            9_007_199_254_740_993
        );
    }

    #[test]
    fn models_reject_duplicates_incompatible_modes_and_resident_overflow() {
        let (_directory, path) = settings_file("");
        let before = std::fs::read(&path).unwrap();
        let model = ModelSetting {
            name: "example:latest".into(),
            mode: "scalar".into(),
            min_free_mib: "13000".into(),
            resident_min_free_mib: "300".into(),
            resources: vec![],
        };
        let mut candidates = vec![];
        let mut duplicate = unchanged_update(&path);
        duplicate.upstreams[0].models = vec![model.clone(), model.clone()];
        candidates.push(duplicate);
        let mut excessive = unchanged_update(&path);
        excessive.upstreams[0].models = vec![ModelSetting {
            resident_min_free_mib: "13001".into(),
            ..model.clone()
        }];
        candidates.push(excessive);
        let mut mixed = unchanged_update(&path);
        mixed.upstreams[0].models = vec![ModelSetting {
            resources: vec![ModelResourceSetting {
                id: "gpu0".into(),
                min_free_mib: "13000".into(),
            }],
            ..model.clone()
        }];
        candidates.push(mixed);
        let mut unknown = unchanged_update(&path);
        unknown.upstreams[0].models = vec![ModelSetting {
            mode: "per_resource".into(),
            min_free_mib: "0".into(),
            resources: vec![ModelResourceSetting {
                id: "unknown".into(),
                min_free_mib: "13000".into(),
            }],
            ..model
        }];
        candidates.push(unknown);
        for update in candidates {
            assert!(persist_settings(&path, update).is_err());
            assert_eq!(std::fs::read(&path).unwrap(), before);
            assert_eq!(
                std::fs::read_dir(path.parent().unwrap()).unwrap().count(),
                1
            );
        }
    }

    #[test]
    fn zero_completion_timeout_round_trips_the_existing_immediate_expiry_policy() {
        let (_directory, path) = settings_file("");
        let mut update = unchanged_update(&path);
        update.upstreams[0].completion_timeout_s = 0;
        assert_eq!(
            persist_settings(&path, update).unwrap().upstreams[0].completion_timeout_s,
            0
        );
    }

    #[test]
    fn wider_tuning_round_trips_units_binding_and_server_limits() {
        let (_directory, path) = settings_file("");
        let mut update = unchanged_update(&path);
        update.owner.media_poll_ms = 7000;
        update.owner.gpu_pressure.cap_pct = 80.0;
        update.owner.gpu_pressure.margin_pct = 10.0;
        update.owner.gpu_pressure.learn_quantile = 0.9;
        update.owner.gpu_pressure.learn_half_life_h = 24.0;
        update.owner.gpu_pressure.raise_pct_per_h = 2.5;
        update.poll_interval_ms = 5000;
        update.server_limits.max_buffered_body_mib = Some(512);
        update.upstreams[0].completion_timeout_s = 600;
        update.resources[0].selector = Some("GPU-0".into());
        update.resources[0].adapter = Some("0000:01:00.0".into());
        let saved = persist_settings(&path, update).unwrap();
        assert_eq!(saved.owner.media_poll_ms, 7000);
        assert_eq!(saved.owner.gpu_pressure.cap_pct, 80.0);
        assert_eq!(saved.owner.gpu_pressure.learn_quantile, 0.9);
        assert_eq!(saved.owner.gpu_pressure.learn_half_life_h, 24.0);
        assert_eq!(saved.owner.gpu_pressure.raise_pct_per_h, 2.5);
        assert_eq!(saved.poll_interval_ms, 5000);
        assert_eq!(saved.server_limits.max_buffered_body_mib, Some(512));
        assert_eq!(saved.upstreams[0].completion_timeout_s, 600);
        assert_eq!(saved.resources[0].adapter.as_deref(), Some("0000:01:00.0"));
    }

    #[test]
    fn a_settings_save_removes_deprecated_pressure_keys_and_the_retired_mode() {
        let (_directory, path) = settings_file("");
        let old = std::fs::read_to_string(&path).unwrap().replace(
            "mode = \"calibrated\"\n",
            "mode = \"adaptive\"\nbaseline_window_s = 21600 # deprecated\nbaseline_quantile = 0.25\n",
        );
        std::fs::write(&path, old).unwrap();
        let snapshot = load_settings_snapshot(&path).unwrap();
        // Offered as what runs.
        assert_eq!(snapshot.owner.gpu_pressure.mode, "calibrated");
        assert_eq!(snapshot.owner.gpu_pressure.learn_quantile, 0.75);
        let mut retired = SettingsUpdate::from(snapshot.clone());
        retired.owner.gpu_pressure.mode = "adaptive".into();
        assert!(persist_settings(&path, retired).is_err());

        persist_settings(&path, snapshot.into()).unwrap();
        let text = std::fs::read_to_string(&path).unwrap();
        assert!(!text.contains("baseline_window_s"), "{text}");
        assert!(!text.contains("baseline_quantile"), "{text}");
        assert!(text.contains("mode = \"calibrated\""), "{text}");
        assert!(text.contains("learn_quantile = 0.75"), "{text}");
        assert!(text.contains("learn_half_life_h = 72.0"), "{text}");
        assert!(text.contains("raise_pct_per_h = 1.0"), "{text}");
        let saved = Config::load(&path).unwrap().owner.gpu_pressure;
        assert_eq!(saved.baseline_quantile, None);
        assert_eq!(saved.baseline_window_s, None);
    }

    /// `[owner.gpu_pressure]` as the example configuration shipped it before
    /// the learned threshold, comments and all.
    const V1_PRESSURE_BLOCK: &str = r#"[owner.gpu_pressure]
enabled = true

# Calibrate each card while quiet, then freeze its baseline. Lending waits for
# calibration of the current attendance profile. "adaptive" is experimental:
# it calibrates first and may subsequently LOWER, never raise, the threshold.
# "fixed" skips learning and requires an explicit threshold_pct > 0.
mode = "calibrated"
threshold_pct = 0
# A candidate baseline above this is rejected as workload, not learned as idle.
# This is a safety bound, not proof that lower utilisation is owner-free.
cap_pct = 40

# Learned threshold = observed baseline + margin, never below the floor. The
# floor matters on a very quiet machine: without it the baseline approaches
# zero and ordinary desktop noise starts reading as the owner returning.
margin_pct = 20
# Baseline + margin must remain below 100%; floor must be between 0 and 100.
floor_pct = 25

# Seconds of sustained load before the resource refuses. Stops a thumbnail render
# or a scrolled page from evicting a model that takes half a minute to reload.
# The release side is `release_after_s` above.
sustain_s = 15

# Calibration requires warmup_samples successful readings. Elapsed time depends
# on the actual poll cadence, probe duration, and failed-read gaps; it is not a
# fixed timer. Leave owner GPU work stopped during that phase. Locked and
# unlocked sessions calibrate separately; missing readings reset partial work.
# baseline_window_s bounds the experimental adaptive window after calibration.
baseline_window_s = 21600
baseline_quantile = 0.25
warmup_samples = 60

# Its own cadence: the read spans about a thousand counter instances.
poll_ms = 5000

# Processes whose GPU use never counts as pressure: a wallpaper engine, a
# monitoring overlay, a second inference server you run outside vacancyd.
# Case-insensitive substring match, like media_ignore.
ignore = []

# Completed calibration is cached only for UUID-selected GPUs. Restore checks
# the observed PCI identity, the ignore list and the estimator settings (mode,
# baseline_quantile, baseline_window_s, warmup_samples); changing the margin,
# floor, cap or timings keeps it. A schema 2 cache is migrated; older formats
# are ignored. Empty means vacancyd-pressure.json beside this file. A
# hardened systemd service cannot write beside /etc/vacancyd/vacancyd.toml;
# set this to /var/lib/vacancyd/pressure.json when using the shipped unit. It
# is a cache. To recalibrate, stop vacancyd, move this file aside, then start it
# with the GPUs quiet. Lending waits for calibration again. See docs/calibration.md.
state_file = """#;

    #[test]
    fn a_save_of_a_v1_pressure_block_keeps_its_comments_and_places_the_new_keys() {
        let (_directory, path) = settings_file("");
        let source = std::fs::read_to_string(&path).unwrap();
        let start = source.find("[owner.gpu_pressure]").unwrap();
        let end = source.find("[[resource]]").unwrap();
        let old = format!(
            "{}{V1_PRESSURE_BLOCK}\n\n{}",
            &source[..start],
            &source[end..]
        );
        std::fs::write(&path, old).unwrap();
        persist_settings(&path, load_settings_snapshot(&path).unwrap().into()).unwrap();
        let text = std::fs::read_to_string(&path).unwrap();
        let block =
            &text[text.find("[owner.gpu_pressure]").unwrap()..text.find("[[resource]]").unwrap()];
        assert!(!block.contains("baseline_window_s ="), "{block}");
        assert!(!block.contains("baseline_quantile ="), "{block}");
        // The paragraph that sat above the removed keys now introduces
        // warmup_samples directly.
        let paragraph = "# Calibration requires warmup_samples successful readings.";
        let comment = block.find(paragraph).unwrap_or_else(|| panic!("{block}"));
        let warmup = block.find("warmup_samples = 60").unwrap();
        assert!(comment < warmup, "{block}");
        assert!(!block[comment..warmup].contains(" = "), "{block}");
        // The new keys sit between sustain_s and that paragraph, each under
        // its own comment.
        let sustain = block.find("sustain_s = 15").unwrap();
        let mut previous = sustain;
        for key in ["learn_quantile", "learn_half_life_h", "raise_pct_per_h"] {
            let at = block
                .find(&format!("{key} = "))
                .unwrap_or_else(|| panic!("{block}"));
            assert!(previous < at && at < comment, "{key}: {block}");
            let line_above = block[..at].trim_end().lines().last().unwrap();
            assert!(line_above.starts_with("# "), "{key}: {block}");
            previous = at;
        }
        // Every other comment survives where it was.
        for kept in [
            "# Its own cadence: the read spans about a thousand counter instances.\npoll_ms",
            "# Completed calibration is cached only for UUID-selected GPUs.",
            "# Seconds of sustained load before the resource refuses.",
        ] {
            assert!(block.contains(kept), "{kept}: {block}");
        }
        Config::load(&path).unwrap();
    }

    #[test]
    fn a_new_calibrated_save_keeps_the_floor_at_most_the_cap_and_the_cap_at_most_90() {
        let (_directory, path) = settings_file("");
        let snapshot = load_settings_snapshot(&path).unwrap();
        for (floor, cap, mode, accepted) in [
            (25.0, 25.0, "calibrated", true),
            (25.0, 90.0, "calibrated", true),
            (30.0, 25.0, "calibrated", false),
            (25.0, 90.5, "calibrated", false),
            (30.0, 95.0, "fixed", true),
        ] {
            let mut update = SettingsUpdate::from(snapshot.clone());
            update.owner.gpu_pressure.floor_pct = floor;
            update.owner.gpu_pressure.cap_pct = cap;
            update.owner.gpu_pressure.mode = mode.into();
            update.owner.gpu_pressure.threshold_pct = if mode == "fixed" { 50.0 } else { 0.0 };
            assert_eq!(
                validate_update(&mut update).is_ok(),
                accepted,
                "{floor} {cap} {mode}"
            );
        }
    }

    #[test]
    fn settings_save_works_for_a_config_without_upstreams_or_owner_tables() {
        // The MSI starter and a minimal Linux config have no [[upstream]]
        // and may omit [owner]; saving used to fail with `persistence`.
        let directory = crate::secure_file::test_root().join(format!(
            "vacancyd-settings-minimal-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        std::fs::create_dir(&directory).unwrap();
        let _cleanup = TestDirectory(directory.clone());
        let path = directory.join("vacancyd.toml");
        std::fs::write(
            &path,
            "[server]\nlisten = \"127.0.0.1:9811\"\n\n[[resource]]\nid = \"memory\"\nkind = \"system-memory\"\nreserve = 0\n",
        )
        .unwrap();
        let as_update = |snapshot: SettingsSnapshot| SettingsUpdate::from(snapshot);
        let mut update = as_update(load_settings_snapshot(&path).unwrap());
        assert!(update.upstreams.is_empty());
        update.owner.release_after_s = 77;
        update.owner.gpu_pressure.enabled = false;
        update.resources[0].enabled = false;
        let saved = persist_settings(&path, update).unwrap();
        assert_eq!(saved.owner.release_after_s, 77);
        assert!(!saved.resources[0].enabled);
        assert!(saved.upstreams.is_empty());
        let reloaded = Config::load(&path).unwrap();
        assert_eq!(reloaded.owner.release_after_s, 77);
        assert!(reloaded.upstreams.is_empty());

        // An update naming an upstream the file does not have is refused
        // before anything is written.
        let before = std::fs::read(&path).unwrap();
        let mut invented = as_update(load_settings_snapshot(&path).unwrap());
        invented.upstreams.push(UpstreamSetting {
            id: "ollama".into(),
            kind: "ollama".into(),
            yield_on_veto: false,
            on_owner_return: "drain".into(),
            drain_timeout_s: 30,
            owner_scope: "host".into(),
            management: "deny".into(),
            ..serde_json::from_str::<SettingsSnapshot>(include_str!(
                "../tests/fixtures/settings/default.json"
            ))
            .unwrap()
            .upstreams
            .remove(0)
        });
        assert!(matches!(
            persist_settings(&path, invented),
            Err(SettingsFailure::Invalid)
        ));
        assert_eq!(std::fs::read(&path).unwrap(), before);
    }

    #[test]
    fn settings_save_works_on_the_unmodified_msi_starter() {
        // The exact starter the MSI installs has no [[upstream]]; the owner
        // console's first Save used to fail with `persistence`.
        let directory = crate::secure_file::test_root().join(format!(
            "vacancyd-settings-starter-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        std::fs::create_dir(&directory).unwrap();
        let _cleanup = TestDirectory(directory.clone());
        let path = directory.join("vacancyd.toml");
        let starter = include_str!("../packaging/vacancyd.starter.toml");
        std::fs::write(&path, starter).unwrap();
        let snapshot = load_settings_snapshot(&path).unwrap();
        assert!(snapshot.upstreams.is_empty());
        let mut update = SettingsUpdate::from(snapshot);
        update.owner.release_after_s = 300;
        update.owner.processes = vec!["game".into()];
        let saved = persist_settings(&path, update).unwrap();
        assert_eq!(saved.owner.release_after_s, 300);
        let text = std::fs::read_to_string(&path).unwrap();
        // Comments and the placeholder resource survive.
        assert!(text.contains("# Safe first-run configuration installed by the Windows MSI."));
        assert!(text.contains("id = \"configure-me\""));
        let reloaded = Config::load(&path).unwrap();
        assert_eq!(reloaded.owner.processes, ["game"]);
        assert!(reloaded.upstreams.is_empty());
    }

    #[test]
    fn settings_save_preserves_comments_and_fields_outside_the_whitelist() {
        let (_directory, path) = settings_file("");
        let saved = persist_settings(
            &path,
            settings_update(load_settings_snapshot(&path).unwrap()),
        )
        .unwrap();
        let bytes = std::fs::read(&path).unwrap();
        let text = std::str::from_utf8(&bytes).unwrap();
        assert_eq!(saved.revision, revision(&bytes));
        assert_eq!(saved.owner.processes, ["blender"]);
        assert!(!saved.resources[0].enabled);
        assert_eq!(saved.upstreams[0].on_owner_return, "cancel");
        assert!(saved.owner.sign_in_screen_is_presence);
        assert!(text.contains("sign_in_screen_is_presence = true"));
        assert!(
            Config::load(&path)
                .unwrap()
                .owner
                .presence_options()
                .sign_in_screen_is_presence
        );
        assert!(text.contains("# preserved owner comment"));
        assert!(text.contains("target = \"http://127.0.0.1:11434\" # outside the whitelist"));
        assert!(text.contains("cap_pct = 70 # outside the whitelist"));
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            assert_eq!(
                std::fs::metadata(&path).unwrap().permissions().mode() & 0o777,
                0o600
            );
        }
    }

    #[test]
    fn settings_round_trip_both_upstream_safety_policies() {
        let (_directory, path) = settings_file("");
        for owner_scope in ["resource", "host"] {
            for management in ["allow", "deny"] {
                let mut update = settings_update(load_settings_snapshot(&path).unwrap());
                update.upstreams[0].owner_scope = owner_scope.into();
                update.upstreams[0].management = management.into();
                let saved = persist_settings(&path, update).unwrap();
                assert_eq!(saved.upstreams[0].owner_scope, owner_scope);
                assert_eq!(saved.upstreams[0].management, management);
                assert_eq!(load_settings_snapshot(&path).unwrap(), saved);
                let config = Config::load(&path).unwrap();
                assert_eq!(
                    config.upstreams[0].owner_scope == crate::config::OwnerScope::Resource,
                    owner_scope == "resource"
                );
                assert_eq!(
                    config.upstreams[0].management == crate::config::ManagementPolicy::Allow,
                    management == "allow"
                );
            }
        }
    }

    #[test]
    fn settings_refuses_conflicting_owner_scopes_on_a_shared_server() {
        let (_directory, path) = settings_file(concat!(
            "\n[[upstream]]\nid = \"second\"\nkind = \"ollama\"\n",
            "listen = \"127.0.0.1:11436\"\ntarget = \"http://127.0.0.1:11434\"\n",
            "resource = \"gpu0\"\ndefault_min_free_mib = 8000\n"
        ));
        let snapshot = load_settings_snapshot(&path).unwrap();
        let mut update = SettingsUpdate::from(snapshot);
        update.upstreams[0].owner_scope = "resource".into();
        let before = std::fs::read(&path).unwrap();
        assert!(matches!(
            persist_settings(&path, update),
            Err(SettingsFailure::ConfigInvalid)
        ));
        assert_eq!(std::fs::read(&path).unwrap(), before);
        assert_eq!(
            std::fs::read_dir(path.parent().unwrap()).unwrap().count(),
            1
        );
    }

    #[test]
    fn settings_save_refuses_growth_beyond_the_readable_config_limit() {
        let (_directory, path) = settings_file("");
        let mut source = std::fs::read_to_string(&path).unwrap();
        source.push('#');
        source.push_str(&"x".repeat(MAX_CONFIG_BYTES - source.len() - 1));
        source.push('\n');
        assert_eq!(source.len(), MAX_CONFIG_BYTES);
        std::fs::write(&path, &source).unwrap();
        let update = settings_update(load_settings_snapshot(&path).unwrap());

        let result = persist_settings(&path, update);
        assert!(
            matches!(result, Err(SettingsFailure::Invalid)),
            "{result:?}"
        );
        assert_eq!(std::fs::read(&path).unwrap(), source.as_bytes());
        assert!(load_settings_snapshot(&path).is_ok());
        assert_eq!(
            std::fs::read_dir(path.parent().unwrap()).unwrap().count(),
            1
        );
    }

    #[test]
    fn settings_safety_policies_are_required_and_reject_invalid_values_without_writing() {
        let (_directory, path) = settings_file("");
        let before = std::fs::read(&path).unwrap();
        let snapshot = load_settings_snapshot(&path).unwrap();
        for field in ["owner_scope", "management"] {
            let document = serde_json::to_value(settings_update(snapshot.clone())).unwrap();
            let mut missing = document.clone();
            missing["upstreams"][0]
                .as_object_mut()
                .unwrap()
                .remove(field);
            assert!(serde_json::from_value::<StrictSettingsUpdate>(missing).is_err());
            for invalid in [
                serde_json::Value::Null,
                serde_json::json!(true),
                serde_json::json!(1),
            ] {
                let mut malformed = document.clone();
                malformed["upstreams"][0][field] = invalid;
                assert!(serde_json::from_value::<StrictSettingsUpdate>(malformed).is_err());
            }
            for invalid in ["", "ALLOW", "HOST", "resource ", " allow", "all", "none"] {
                let mut malformed = document.clone();
                malformed["upstreams"][0][field] = serde_json::json!(invalid);
                let strict = serde_json::from_value::<StrictSettingsUpdate>(malformed).unwrap();
                assert!(matches!(
                    persist_settings(&path, strict),
                    Err(SettingsFailure::Invalid)
                ));
                assert_eq!(std::fs::read(&path).unwrap(), before);
                assert_eq!(
                    std::fs::read_dir(path.parent().unwrap()).unwrap().count(),
                    1
                );
            }
        }
    }

    #[test]
    fn settings_save_round_trips_at_the_config_byte_limit() {
        for size in [MAX_CONFIG_BYTES - 1, MAX_CONFIG_BYTES] {
            let (_directory, path) = settings_file("");
            persist_settings(
                &path,
                settings_update(load_settings_snapshot(&path).unwrap()),
            )
            .unwrap();
            let mut source = std::fs::read_to_string(&path).unwrap();
            source.push('#');
            source.push_str(&"x".repeat(size - source.len() - 1));
            source.push('\n');
            std::fs::write(&path, &source).unwrap();
            let update = settings_update(load_settings_snapshot(&path).unwrap());
            let saved = persist_settings(&path, update).unwrap();
            assert_eq!(std::fs::read(&path).unwrap().len(), size);
            assert_eq!(load_settings_snapshot(&path).unwrap(), saved);
        }
    }

    #[test]
    fn settings_rejects_an_already_oversized_config_without_writing() {
        let (_directory, path) = settings_file("");
        let mut update = settings_update(load_settings_snapshot(&path).unwrap());
        let mut source = std::fs::read_to_string(&path).unwrap();
        source.push('#');
        source.push_str(&"x".repeat(MAX_CONFIG_BYTES - source.len()));
        source.push('\n');
        assert_eq!(source.len(), MAX_CONFIG_BYTES + 1);
        std::fs::write(&path, &source).unwrap();
        update.revision = revision(source.as_bytes());
        assert!(matches!(
            load_settings_snapshot(&path),
            Err(SettingsFailure::Persistence)
        ));
        assert!(matches!(
            persist_settings(&path, update),
            Err(SettingsFailure::Persistence)
        ));
        assert_eq!(std::fs::read(&path).unwrap(), source.as_bytes());
        assert_eq!(
            std::fs::read_dir(path.parent().unwrap()).unwrap().count(),
            1
        );
    }

    #[test]
    fn settings_stale_or_failed_staging_never_changes_source_bytes() {
        let (_directory, path) = settings_file("");
        let original = std::fs::read(&path).unwrap();
        let mut stale = settings_update(load_settings_snapshot(&path).unwrap());
        stale.revision = "0".repeat(64);
        assert!(matches!(
            persist_settings(&path, stale),
            Err(SettingsFailure::Stale)
        ));
        assert_eq!(std::fs::read(&path).unwrap(), original);

        let mut duplicate = settings_update(load_settings_snapshot(&path).unwrap());
        duplicate.resources.push(duplicate.resources[0].clone());
        assert!(matches!(
            persist_settings(&path, duplicate),
            Err(SettingsFailure::Invalid)
        ));
        assert_eq!(std::fs::read(&path).unwrap(), original);

        let mut unknown =
            serde_json::to_value(settings_update(load_settings_snapshot(&path).unwrap())).unwrap();
        unknown["unexpected"] = serde_json::Value::Bool(true);
        assert!(serde_json::from_value::<StrictSettingsUpdate>(unknown).is_err());
        for invalid in [
            serde_json::Value::Null,
            serde_json::Value::String("true".into()),
        ] {
            let mut update =
                serde_json::to_value(settings_update(load_settings_snapshot(&path).unwrap()))
                    .unwrap();
            update["owner"]["sign_in_screen_is_presence"] = invalid;
            assert!(serde_json::from_value::<StrictSettingsUpdate>(update).is_err());
        }
        let mut missing =
            serde_json::to_value(settings_update(load_settings_snapshot(&path).unwrap())).unwrap();
        missing["owner"]
            .as_object_mut()
            .unwrap()
            .remove("sign_in_screen_is_presence");
        assert!(serde_json::from_value::<StrictSettingsUpdate>(missing).is_err());

        // Passes validate_update (the submitted values are fine) but the
        // staged config's own token cannot resolve, a distinct failure from
        // a bad submission: see SET-1.
        let auth = "[auth]\ntrust_loopback = false\n[[auth.token]]\nname = \"owner\"\nscopes = [\"admin\"]\nvalue_file = \"missing-token\"\n";
        let (_credential_directory, credential_path) = settings_file(auth);
        let before = std::fs::read(&credential_path).unwrap();
        let update = settings_update(load_settings_snapshot(&credential_path).unwrap());
        assert!(matches!(
            persist_settings(&credential_path, update),
            Err(SettingsFailure::CredentialUnresolved)
        ));
        assert_eq!(std::fs::read(&credential_path).unwrap(), before);
    }

    /// The loopback trust of platforms without an admin channel (macOS).
    const LOOPBACK_ADMIN: vacancyd::local_admin::Policy = vacancyd::local_admin::Policy {
        loopback_is_administrator: true,
    };
    /// Windows and Linux: loopback keeps read and pause only.
    const LOOPBACK_READ_AND_PAUSE: vacancyd::local_admin::Policy = vacancyd::local_admin::Policy {
        loopback_is_administrator: false,
    };

    #[tokio::test]
    async fn settings_routes_require_intent_and_return_restart_required() {
        let (_directory, path) = settings_file("");
        let mut inner =
            AppState::new_with_config_path(Config::load(&path).unwrap(), &path).unwrap();
        inner.local_admin_policy = LOOPBACK_ADMIN;
        let state = Arc::new(inner);
        let get = routes(state.clone())
            .oneshot(
                Request::builder()
                    .uri("/admin/settings")
                    .extension(ConnectInfo(
                        "127.0.0.1:12345".parse::<SocketAddr>().unwrap(),
                    ))
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(get.status(), StatusCode::FORBIDDEN);
        let post = routes(state)
            .oneshot(
                Request::builder()
                    .method(Method::POST)
                    .uri("/admin/settings")
                    .header(ADMIN_INTENT_HEADER, ADMIN_INTENT_VALUE)
                    .header("content-type", "application/json")
                    .extension(ConnectInfo(
                        "127.0.0.1:12345".parse::<SocketAddr>().unwrap(),
                    ))
                    .body(Body::from(
                        serde_json::to_vec(&settings_update(
                            load_settings_snapshot(&path).unwrap(),
                        ))
                        .unwrap(),
                    ))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(post.status(), StatusCode::OK);
        let body = axum::body::to_bytes(post.into_body(), 4096).await.unwrap();
        assert_eq!(
            serde_json::from_slice::<serde_json::Value>(&body).unwrap()["restart_required"],
            true
        );
    }

    fn test_state_with_auth(auth: &str) -> AppState {
        let source = format!(
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
            default_min_free_mib = 8000
            {auth}
            "#
        );
        let config: Config = toml::from_str(&source).unwrap();
        AppState::new(config).unwrap()
    }

    fn test_state() -> AppState {
        test_state_with_auth("")
    }

    #[tokio::test]
    async fn status_route_emits_the_shared_contract_and_keeps_health_detail_free() {
        let state = Arc::new(test_state());
        for (path, expected) in [("/healthz", "ok"), ("/readyz", "not ready")] {
            let response = routes(state.clone())
                .oneshot(
                    Request::builder()
                        .uri(path)
                        .extension(ConnectInfo(
                            "127.0.0.1:12345".parse::<SocketAddr>().unwrap(),
                        ))
                        .body(Body::empty())
                        .unwrap(),
                )
                .await
                .unwrap();
            let body = axum::body::to_bytes(response.into_body(), 1024)
                .await
                .unwrap();
            assert_eq!(body.as_ref(), expected.as_bytes());
        }
        let response = routes(state)
            .oneshot(
                Request::builder()
                    .uri("/status")
                    .extension(ConnectInfo(
                        "127.0.0.1:12345".parse::<SocketAddr>().unwrap(),
                    ))
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::OK);
        let bytes = axum::body::to_bytes(response.into_body(), wire::MAX_STATUS_BYTES)
            .await
            .unwrap();
        let status = wire::decode(&bytes).unwrap();
        assert_eq!(status.daemon.version, env!("CARGO_PKG_VERSION"));
        assert_eq!(status.schema_version, wire::SCHEMA_VERSION);
        assert!(!status.ready);
        assert!(!status.upstreams[0].vacancy);
    }

    #[cfg(any(windows, target_os = "linux"))]
    #[test]
    fn status_says_whether_the_local_admin_channel_is_listening() {
        let state = test_state();
        let channel = |state: &AppState| status_body(state).admin_channel.unwrap().state;
        assert_eq!(channel(&state), "unavailable");
        let listening = crate::agent::supervise::Listening::mark(&state.admin_channel_listening);
        assert_eq!(channel(&state), "listening");
        drop(listening);
        assert_eq!(channel(&state), "unavailable", "a channel that stopped");
    }

    #[test]
    fn status_names_the_upstreams_the_owner_veto_spares() {
        let config: Config = toml::from_str(
            r#"
            [server]
            listen = "127.0.0.1:0"
            [owner.gpu_pressure]
            enabled = false
            [[resource]]
            id = "gpu0"
            kind = "nvidia-gpu"
            selector = "GPU-0"
            [[resource]]
            id = "gpu1"
            kind = "nvidia-gpu"
            selector = "GPU-1"
            [[upstream]]
            id = "ollama"
            listen = "127.0.0.1:0"
            target = "http://127.0.0.1:11434"
            kind = "ollama"
            resource = "gpu0"
            [[upstream]]
            id = "helper"
            listen = "127.0.0.1:0"
            target = "http://127.0.0.1:11436"
            kind = "ollama"
            resource = "gpu1"
            owner_scope = "resource"
            "#,
        )
        .unwrap();
        let state = AppState::new(config).unwrap();
        state.mark_presence_sampled();
        for id in ["gpu0", "gpu1"] {
            state.resources[id]
                .capacity
                .write()
                .unwrap()
                .record_success(Capacity {
                    free_bytes: 16 * 1024 * 1024 * 1024,
                    total_bytes: 16 * 1024 * 1024 * 1024,
                    utilization_pct: Some(0),
                    label: "test gpu".to_string(),
                });
        }

        let clear = status_body(&state);
        assert!(!clear.owner_veto.active);
        assert_eq!(clear.owner_veto.exempt_upstreams, ["helper"]);
        assert_eq!(clear.upstreams[0].owner_scope, "host");
        assert_eq!(clear.upstreams[1].owner_scope, "resource");
        assert!(clear.upstreams.iter().all(|upstream| upstream.vacancy));

        state.veto.observe(Some("Marathon".into()), 120);
        let vetoed = status_body(&state);
        assert!(vetoed.owner_veto.active);
        assert_eq!(vetoed.owner_veto.exempt_upstreams, ["helper"]);
        let host = &vetoed.upstreams[0];
        assert!(!host.vacancy);
        assert_eq!(host.refusal.as_ref().unwrap().reason, "owner_active");
        assert!(vetoed.upstreams[1].vacancy);
        // The shared decoder the console and tray use accepts the exempt
        // vacancy instead of reporting a changing state on every poll.
        let decoded = wire::decode(&serde_json::to_vec(&vetoed).unwrap()).unwrap();
        assert_eq!(decoded.exempt_serving(), ["helper"]);

        let metrics = state.metrics.render();
        assert!(metrics.contains("vacancyd_upstream_owner_veto_exempt{upstream=\"helper\"} 1"));
        assert!(metrics.contains("vacancyd_upstream_owner_veto_exempt{upstream=\"ollama\"} 0"));
    }

    #[test]
    fn status_owns_required_unused_and_listener_semantics() {
        let config: Config = toml::from_str(
            r#"
            [server]
            listen = "127.0.0.1:0"
            [[resource]]
            id = "mem"
            kind = "system-memory"
            [[resource]]
            id = "unused"
            kind = "system-memory"
            [[upstream]]
            id = "llama"
            listen = "127.0.0.1:0"
            target = "http://127.0.0.1:11434"
            kind = "llamacpp"
            resource = "mem"
            "#,
        )
        .unwrap();
        let state = AppState::new(config).unwrap();
        state.upstreams["llama"]
            .listener
            .write()
            .unwrap()
            .record_success(crate::probes::listeners::ListenerState::LoopbackOnly);
        let status = status_body(&state);
        let required = status.resources.iter().find(|r| r.id == "mem").unwrap();
        let unused = status.resources.iter().find(|r| r.id == "unused").unwrap();
        assert!(required.required);
        assert_eq!(required.required_by, vec!["llama".to_string()]);
        assert!(!unused.required);
        assert!(unused.required_by.is_empty());
        assert!(!unused.lendable);
        assert_eq!(status.upstreams[0].listener.state, "loopback_only");
        assert_eq!(
            status.upstreams[0].listener.observation.quality,
            wire::Quality::Ready
        );

        state.upstreams["llama"]
            .listener
            .write()
            .unwrap()
            .record_failure("listener table unavailable");
        let failed = status_body(&state);
        assert_eq!(failed.upstreams[0].listener.state, "unknown");
        assert_eq!(
            failed.upstreams[0]
                .listener
                .observation
                .last_error
                .as_deref(),
            Some("listener table unavailable")
        );
    }

    #[test]
    fn admin_intent_requires_the_exact_non_safelisted_header() {
        let mut headers = HeaderMap::new();
        assert_eq!(admin_intent_denial_reason(&headers), Some("missing_intent"));

        headers.insert(ADMIN_INTENT_HEADER, HeaderValue::from_static("true"));
        assert_eq!(admin_intent_denial_reason(&headers), Some("missing_intent"));

        headers.insert(
            ADMIN_INTENT_HEADER,
            HeaderValue::from_static(ADMIN_INTENT_VALUE),
        );
        assert_eq!(admin_intent_denial_reason(&headers), None);

        headers.insert(
            axum::http::header::ORIGIN,
            HeaderValue::from_static("https://attacker.example"),
        );
        assert_eq!(admin_intent_denial_reason(&headers), Some("browser_origin"));
    }

    fn admin_request(peer: SocketAddr, intent: Option<&str>, token: Option<&str>) -> Request<Body> {
        let mut builder = Request::builder()
            .method(Method::POST)
            .uri("/admin/lending/disable")
            .extension(ConnectInfo(peer));
        if let Some(intent) = intent {
            builder = builder.header(ADMIN_INTENT_HEADER, intent);
        }
        if let Some(token) = token {
            builder = builder.header(axum::http::header::AUTHORIZATION, token);
        }
        builder.body(Body::empty()).unwrap()
    }

    #[tokio::test]
    async fn work_release_requires_admin_intent_and_explicit_upstream_confirmation() {
        let mut inner = test_state();
        inner.local_admin_policy = LOOPBACK_ADMIN;
        inner.config.owner.gpu_pressure.enabled = false;
        let state = Arc::new(inner);
        state.mark_presence_sampled();
        state.resources["gpu0"]
            .capacity
            .write()
            .unwrap()
            .record_success(Capacity {
                free_bytes: 16 * 1024 * 1024 * 1024,
                ..Capacity::default()
            });
        let ticket = crate::admission::reserve(
            &state,
            &state.config.upstreams[0],
            None,
            crate::plugin::adapter::Completion::Unknown,
        )
        .unwrap();
        drop(ticket);
        let id = state.admission.lock().unwrap().snapshot()[0].id;
        for (intent, confirmed, expected) in [
            (false, true, StatusCode::FORBIDDEN),
            (true, false, StatusCode::BAD_REQUEST),
            (true, true, StatusCode::NO_CONTENT),
        ] {
            let mut request = Request::builder()
                .method(Method::POST)
                .uri(format!("/admin/work/{id}/release"))
                .extension(ConnectInfo("127.0.0.1:1234".parse::<SocketAddr>().unwrap()))
                .header("content-type", "application/json");
            if intent {
                request = request.header(ADMIN_INTENT_HEADER, ADMIN_INTENT_VALUE);
            }
            let response = routes(state.clone())
                .oneshot(
                    request
                        .body(Body::from(format!(r#"{{"upstream_stopped":{confirmed}}}"#)))
                        .unwrap(),
                )
                .await
                .unwrap();
            assert_eq!(response.status(), expected);
        }
        assert!(state.admission.lock().unwrap().snapshot().is_empty());
        assert!(crate::gate::admit(&state, &state.config.upstreams[0], None).is_err());
    }

    #[tokio::test]
    async fn admin_route_requires_intent_before_mutating_loopback_state() {
        let state = Arc::new(test_state());
        assert!(state.lending_enabled.load(Ordering::Relaxed));

        for intent in [None, Some("true")] {
            let response = routes(state.clone())
                .oneshot(admin_request(
                    "127.0.0.1:12345".parse().unwrap(),
                    intent,
                    None,
                ))
                .await
                .unwrap();
            assert_eq!(response.status(), StatusCode::FORBIDDEN);
            assert!(state.lending_enabled.load(Ordering::Relaxed));
        }

        let response = routes(state.clone())
            .oneshot(admin_request(
                "127.0.0.1:12345".parse().unwrap(),
                Some(ADMIN_INTENT_VALUE),
                None,
            ))
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::OK);
        assert!(!state.lending_enabled.load(Ordering::Relaxed));
    }

    #[tokio::test]
    async fn remote_admin_requires_both_intent_and_scoped_token() {
        let state = Arc::new(test_state_with_auth(
            r#"
            [auth]
            trust_loopback = false
            [[auth.token]]
            name = "operator"
            scopes = ["admin"]
            value = "0123456789abcdef"
            "#,
        ));
        let remote = "192.0.2.20:12345".parse().unwrap();

        let missing_intent = routes(state.clone())
            .oneshot(admin_request(remote, None, Some("Bearer 0123456789abcdef")))
            .await
            .unwrap();
        assert_eq!(missing_intent.status(), StatusCode::FORBIDDEN);

        let missing_token = routes(state.clone())
            .oneshot(admin_request(remote, Some(ADMIN_INTENT_VALUE), None))
            .await
            .unwrap();
        assert_eq!(missing_token.status(), StatusCode::UNAUTHORIZED);

        let allowed = routes(state.clone())
            .oneshot(admin_request(
                remote,
                Some(ADMIN_INTENT_VALUE),
                Some("Bearer 0123456789abcdef"),
            ))
            .await
            .unwrap();
        assert_eq!(allowed.status(), StatusCode::OK);
        assert!(!state.lending_enabled.load(Ordering::Relaxed));
    }

    fn loopback_admin(method: Method, uri: &str, token: Option<&str>, body: Body) -> Request<Body> {
        let mut builder = Request::builder()
            .method(method)
            .uri(uri)
            .header(ADMIN_INTENT_HEADER, ADMIN_INTENT_VALUE)
            .header("content-type", "application/json")
            .extension(ConnectInfo(
                "127.0.0.1:12345".parse::<SocketAddr>().unwrap(),
            ));
        if let Some(token) = token {
            builder = builder.header(axum::http::header::AUTHORIZATION, token);
        }
        builder.body(body).unwrap()
    }

    #[tokio::test]
    async fn windows_loopback_may_read_settings_and_pause_but_change_nothing_else() {
        // A loopback connection does not show which account
        // made it, so on Windows it must not rewrite the Administrators-only
        // config, resume lending, or switch resources for a standard user.
        let (_directory, path) = settings_file(
            "\n[[auth.token]]\nname = \"operator\"\nscopes = [\"admin\"]\nvalue = \"0123456789abcdef\"\n",
        );
        let mut inner =
            AppState::new_with_config_path(Config::load(&path).unwrap(), &path).unwrap();
        inner.local_admin_policy = LOOPBACK_READ_AND_PAUSE;
        let state = Arc::new(inner);
        let original = std::fs::read(&path).unwrap();
        let send = |request| routes(state.clone()).oneshot(request);

        let read = send(loopback_admin(
            Method::GET,
            "/admin/settings",
            None,
            Body::empty(),
        ))
        .await
        .unwrap();
        assert_eq!(read.status(), StatusCode::OK);

        let pause = send(loopback_admin(
            Method::POST,
            "/admin/lending/disable",
            None,
            Body::empty(),
        ))
        .await
        .unwrap();
        assert_eq!(pause.status(), StatusCode::OK);
        assert!(!state.lending_enabled.load(Ordering::Relaxed));
        assert_eq!(
            *state.paused_by.lock().unwrap(),
            Some(PausedBy::Unattributed)
        );

        let update =
            serde_json::to_vec(&settings_update(load_settings_snapshot(&path).unwrap())).unwrap();
        for (uri, body) in [
            ("/admin/lending/enable", Body::empty()),
            ("/admin/resources/gpu0/disable", Body::empty()),
            (
                "/admin/work/1/release",
                Body::from(r#"{"upstream_stopped":true}"#),
            ),
            ("/admin/settings", Body::from(update.clone())),
            ("/admin/not-yet-invented", Body::empty()),
        ] {
            let refused = send(loopback_admin(Method::POST, uri, None, body))
                .await
                .unwrap();
            assert_eq!(refused.status(), StatusCode::FORBIDDEN, "{uri}");
            let body = axum::body::to_bytes(refused.into_body(), 4096)
                .await
                .unwrap();
            assert_eq!(
                serde_json::from_slice::<serde_json::Value>(&body).unwrap()["error"]["type"],
                "administrator_required",
                "{uri}"
            );
        }
        assert!(!state.lending_enabled.load(Ordering::Relaxed));
        assert!(state.resource_enabled("gpu0"));
        assert_eq!(std::fs::read(&path).unwrap(), original);

        // An admin-scoped token still works on loopback.
        let token = Some("Bearer 0123456789abcdef");
        let saved = send(loopback_admin(
            Method::POST,
            "/admin/settings",
            token,
            Body::from(update),
        ))
        .await
        .unwrap();
        assert_eq!(saved.status(), StatusCode::OK);
        let resumed = send(loopback_admin(
            Method::POST,
            "/admin/lending/enable",
            token,
            Body::empty(),
        ))
        .await
        .unwrap();
        assert_eq!(resumed.status(), StatusCode::OK);
        assert!(state.lending_enabled.load(Ordering::Relaxed));
        assert_eq!(*state.paused_by.lock().unwrap(), None);
    }

    #[cfg(target_os = "linux")]
    #[tokio::test]
    async fn linux_loopback_defaults_to_read_and_pause_only() {
        // The daemon's own default on Linux, with nothing
        // overridden. Changes other than pausing arrive over the admin
        // socket, which identifies the account.
        let (_directory, path) = settings_file("");
        let state =
            Arc::new(AppState::new_with_config_path(Config::load(&path).unwrap(), &path).unwrap());
        assert_eq!(state.local_admin_policy, LOOPBACK_READ_AND_PAUSE);
        let original = std::fs::read(&path).unwrap();
        let send = |request| routes(state.clone()).oneshot(request);

        let read = send(loopback_admin(
            Method::GET,
            "/admin/settings",
            None,
            Body::empty(),
        ))
        .await
        .unwrap();
        assert_eq!(read.status(), StatusCode::OK);
        let pause = send(loopback_admin(
            Method::POST,
            "/admin/lending/disable",
            None,
            Body::empty(),
        ))
        .await
        .unwrap();
        assert_eq!(pause.status(), StatusCode::OK);
        let update =
            serde_json::to_vec(&settings_update(load_settings_snapshot(&path).unwrap())).unwrap();
        for (uri, body) in [
            ("/admin/lending/enable", Body::empty()),
            ("/admin/resources/gpu0/disable", Body::empty()),
            (
                "/admin/work/1/release",
                Body::from(r#"{"upstream_stopped":true}"#),
            ),
            ("/admin/settings", Body::from(update)),
        ] {
            let refused = send(loopback_admin(Method::POST, uri, None, body))
                .await
                .unwrap();
            assert_eq!(refused.status(), StatusCode::FORBIDDEN, "{uri}");
            let body = axum::body::to_bytes(refused.into_body(), 4096)
                .await
                .unwrap();
            let body: serde_json::Value = serde_json::from_slice(&body).unwrap();
            assert_eq!(body["error"]["type"], "administrator_required", "{uri}");
            assert!(
                body["error"]["message"]
                    .as_str()
                    .unwrap()
                    .contains("vacancyd admin-request"),
                "{uri}"
            );
        }
        assert!(!state.lending_enabled.load(Ordering::Relaxed));
        assert!(state.resource_enabled("gpu0"));
        assert_eq!(std::fs::read(&path).unwrap(), original);
    }

    #[test]
    fn pipe_callers_are_judged_by_their_verified_account() {
        let (_directory, path) = settings_file("");
        let mut inner =
            AppState::new_with_config_path(Config::load(&path).unwrap(), &path).unwrap();
        inner.local_admin_policy = LOOPBACK_READ_AND_PAUSE;
        let state = inner;
        let alice = Caller::Account {
            account: "S-1-5-21-1-1001".into(),
            administrator: false,
        };
        let bob = Caller::Account {
            account: "S-1-5-21-1-1002".into(),
            administrator: false,
        };
        let administrator = Caller::Account {
            account: "S-1-5-21-1-500".into(),
            administrator: true,
        };
        let needs_admin = Response::Refused {
            reason: RefusalReason::AdministratorRequired,
        };
        let perform = |caller: &Caller, action| perform_local_admin(&state, caller, action);
        let off = Response::Lending {
            lending_enabled: false,
        };
        let on = Response::Lending {
            lending_enabled: true,
        };

        // Any signed-in user may pause; only the pauser or an administrator
        // may resume, and pausing again does not take over the pause.
        assert_eq!(perform(&alice, RequestedAction::Pause {}), off);
        assert_eq!(perform(&bob, RequestedAction::Resume {}), needs_admin);
        assert_eq!(perform(&bob, RequestedAction::Pause {}), off);
        assert_eq!(perform(&bob, RequestedAction::Resume {}), needs_admin);
        assert!(!state.lending_enabled.load(Ordering::Relaxed));
        assert_eq!(perform(&alice, RequestedAction::Resume {}), on);
        assert!(state.lending_enabled.load(Ordering::Relaxed));

        // A pause made over loopback names no account.
        change_lending(&state, false, &Caller::Loopback).unwrap();
        assert_eq!(perform(&alice, RequestedAction::Resume {}), needs_admin);
        assert_eq!(perform(&administrator, RequestedAction::Resume {}), on);

        // Policy-changing actions need an administrator.
        let update =
            serde_json::to_value(settings_update(load_settings_snapshot(&path).unwrap())).unwrap();
        let original = std::fs::read(&path).unwrap();
        for action in [
            RequestedAction::SetResource {
                resource: "gpu0".into(),
                enabled: false,
            },
            RequestedAction::ReleaseWork {
                id: 1,
                upstream_stopped: true,
            },
            RequestedAction::SaveSettings {
                settings: update.clone(),
            },
        ] {
            assert_eq!(perform(&alice, action), needs_admin);
        }
        assert!(state.resource_enabled("gpu0"));
        assert_eq!(std::fs::read(&path).unwrap(), original);

        // An administrator gets the settings API's own answers.
        let Response::Saved {
            settings,
            restart_required: true,
        } = perform(
            &administrator,
            RequestedAction::SaveSettings {
                settings: update.clone(),
            },
        )
        else {
            panic!("an administrator's save should succeed");
        };
        assert_eq!(settings.revision, revision(&std::fs::read(&path).unwrap()));
        assert_eq!(
            perform(
                &administrator,
                RequestedAction::SaveSettings {
                    settings: update.clone()
                }
            ),
            Response::Refused {
                reason: RefusalReason::StaleRevision
            }
        );
        let mut unknown_field = update;
        unknown_field["unexpected"] = serde_json::Value::Bool(true);
        assert_eq!(
            perform(
                &administrator,
                RequestedAction::SaveSettings {
                    settings: unknown_field
                }
            ),
            Response::Refused {
                reason: RefusalReason::Validation
            }
        );
        assert_eq!(
            perform(
                &administrator,
                RequestedAction::SetResource {
                    resource: "gpu0".into(),
                    enabled: false
                }
            ),
            Response::Resource {
                resource: "gpu0".into(),
                enabled: false
            }
        );
        assert!(!state.resource_enabled("gpu0"));
        assert_eq!(
            perform(
                &administrator,
                RequestedAction::SetResource {
                    resource: "gpu9".into(),
                    enabled: true
                }
            ),
            Response::Refused {
                reason: RefusalReason::NotFound
            }
        );
        for (upstream_stopped, reason) in [
            (false, RefusalReason::Validation),
            (true, RefusalReason::Conflict),
        ] {
            assert_eq!(
                perform(
                    &administrator,
                    RequestedAction::ReleaseWork {
                        id: 404,
                        upstream_stopped
                    }
                ),
                Response::Refused { reason }
            );
        }
    }

    /// Send one raw HTTP/1.1 request to `address` and return the status line.
    async fn raw_status_line(address: SocketAddr, request: String) -> String {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};
        let mut stream = tokio::net::TcpStream::connect(address).await.unwrap();
        stream.write_all(request.as_bytes()).await.unwrap();
        let mut response = Vec::new();
        tokio::time::timeout(
            std::time::Duration::from_secs(5),
            stream.read_to_end(&mut response),
        )
        .await
        .unwrap()
        .unwrap();
        String::from_utf8_lossy(&response)
            .lines()
            .next()
            .unwrap_or_default()
            .to_string()
    }

    async fn serve_for_test(state: Arc<AppState>) -> SocketAddr {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let limits = state.config.server.limits.clone();
        tokio::spawn(async move { crate::listener::serve(listener, routes(state), &limits).await });
        address
    }

    #[tokio::test]
    async fn a_dns_rebinding_page_cannot_read_presence_or_settings() {
        // Same-origin reads from a page whose host name was rebound to
        // 127.0.0.1 arrive from loopback, with no Origin, carrying the
        // page's host name. They must not be served on loopback trust.
        let (_directory, path) = settings_file("");
        let state =
            Arc::new(AppState::new_with_config_path(Config::load(&path).unwrap(), &path).unwrap());
        let address = serve_for_test(state).await;
        let port = address.port();
        for (target, intent) in [
            ("/status", ""),
            ("/metrics", ""),
            ("/admin/settings", "X-Vacancyd-Admin: 1\r\n"),
        ] {
            let rebound = raw_status_line(
                address,
                format!("GET {target} HTTP/1.1\r\nHost: rebind.attacker.example:{port}\r\n{intent}Connection: close\r\n\r\n"),
            )
            .await;
            assert!(rebound.contains(" 403 "), "{target}: {rebound}");
            // The owner's own tools name the port by a loopback address.
            let local = raw_status_line(
                address,
                format!("GET {target} HTTP/1.1\r\nHost: 127.0.0.1:{port}\r\n{intent}Connection: close\r\n\r\n"),
            )
            .await;
            assert!(local.contains(" 200 "), "{target}: {local}");
        }
    }

    #[tokio::test]
    async fn a_same_host_reverse_proxy_does_not_pass_on_loopback_admin() {
        // A TLS proxy on the same machine forwards a LAN client with no token.
        let state = Arc::new(test_state_with_auth(
            r#"
            [[auth.token]]
            name = "operator"
            scopes = ["admin"]
            value = "0123456789abcdef"
            "#,
        ));
        let address = serve_for_test(state.clone()).await;
        let port = address.port();
        let proxied = raw_status_line(
            address,
            format!("POST /admin/lending/disable HTTP/1.1\r\nHost: 127.0.0.1:{port}\r\nX-Vacancyd-Admin: 1\r\nX-Forwarded-For: 203.0.113.9\r\nContent-Length: 0\r\nConnection: close\r\n\r\n"),
        )
        .await;
        assert!(proxied.contains(" 401 "), "{proxied}");
        assert!(state.lending_enabled.load(Ordering::Relaxed));
        let with_token = raw_status_line(
            address,
            format!("POST /admin/lending/disable HTTP/1.1\r\nHost: 127.0.0.1:{port}\r\nX-Vacancyd-Admin: 1\r\nX-Forwarded-For: 203.0.113.9\r\nAuthorization: Bearer 0123456789abcdef\r\nContent-Length: 0\r\nConnection: close\r\n\r\n"),
        )
        .await;
        assert!(with_token.contains(" 200 "), "{with_token}");
        assert!(!state.lending_enabled.load(Ordering::Relaxed));
    }

    #[test]
    fn status_does_not_present_unknown_capacity_as_zero() {
        let state = test_state();
        state.mark_presence_sampled();
        let body = serde_json::to_value(status_body(&state)).unwrap();
        let resource = &body["resources"][0];
        assert_eq!(resource["capacity"]["quality"], "unknown");
        assert!(resource["free_mib"].is_null());
        assert!(resource["total_mib"].is_null());
        assert!(resource["utilization_pct"].is_null());
        assert_eq!(resource["pressure"]["quality"], "unknown");
        assert!(resource["pressure"]["non_lender_pct"].is_null());
        assert_eq!(resource["adapter_mapping"]["quality"], "unknown");
        assert!(resource["adapter_mapping"]["pci"].is_null());

        let refusal = &body["upstreams"][0]["refusal"];
        assert_eq!(refusal["reason"], "resource_unmeasured");
        assert_eq!(refusal["resource"], "gpu0");
    }

    #[test]
    fn status_keeps_valid_memory_when_utilization_is_unavailable() {
        let state = test_state();
        let mut observation = state.resources["gpu0"].capacity.write().unwrap();
        observation.record_success(Capacity {
            free_bytes: 4 * 1024 * 1024 * 1024,
            total_bytes: 8 * 1024 * 1024 * 1024,
            utilization_pct: None,
            label: "test gpu".to_string(),
        });
        drop(observation);
        let body = serde_json::to_value(status_body(&state)).unwrap();
        let resource = &body["resources"][0];
        assert_eq!(resource["capacity"]["quality"], "ready");
        assert_eq!(resource["free_mib"], 4096);
        assert_eq!(resource["total_mib"], 8192);
        assert!(resource["utilization_pct"].is_null());
    }

    #[test]
    fn status_does_not_publish_byte_capacity_for_a_cpu() {
        let config: Config = toml::from_str(
            r#"
            [server]
            listen = "127.0.0.1:0"
            [[resource]]
            id = "cpu"
            kind = "system-cpu"
            [[upstream]]
            id = "u"
            listen = "127.0.0.1:0"
            target = "http://127.0.0.1:1"
            kind = "openai"
            resource = "cpu"
            "#,
        )
        .unwrap();
        let state = AppState::new(config).unwrap();
        state.resources["cpu"]
            .capacity
            .write()
            .unwrap()
            .record_success(Capacity {
                free_bytes: 0,
                total_bytes: 0,
                utilization_pct: Some(12),
                label: "8 logical cpus".to_string(),
            });
        let body = serde_json::to_value(status_body(&state)).unwrap();
        let resource = &body["resources"][0];
        assert_eq!(resource["capacity"]["quality"], "ready");
        assert!(resource["free_mib"].is_null(), "{resource}");
        assert!(resource["total_mib"].is_null(), "{resource}");
        assert_eq!(resource["utilization_pct"], 12);
    }

    #[test]
    fn status_does_not_present_last_good_capacity_after_probe_failure() {
        let state = test_state();
        let mut observation = state.resources["gpu0"].capacity.write().unwrap();
        observation.record_success(Capacity {
            free_bytes: 12 * 1024 * 1024 * 1024,
            total_bytes: 16 * 1024 * 1024 * 1024,
            utilization_pct: Some(25),
            label: "test gpu".to_string(),
        });
        observation.record_failure("probe failed".to_string());
        drop(observation);

        let body = serde_json::to_value(status_body(&state)).unwrap();
        let resource = &body["resources"][0];
        assert_eq!(resource["capacity"]["quality"], "unknown");
        assert_eq!(resource["capacity"]["last_error"], "probe failed");
        assert!(resource["label"].is_null());
        assert!(resource["free_mib"].is_null());
        assert!(resource["total_mib"].is_null());
        assert!(resource["utilization_pct"].is_null());
    }

    #[test]
    fn status_does_not_present_last_good_pressure_after_attribution_failure() {
        let state = test_state();
        let mut observation = state.resources["gpu0"].pressure.write().unwrap();
        observation.record_success(PressureVerdict {
            non_lender_pct: 8.0,
            engaged: false,
            threshold_pct: 34.0,
            baseline_pct: Some(25.0),
            target_pct: Some(40.0),
            learned_samples: 37_440,
            learned_s: 187_200,
            mode: Default::default(),
            calibrated: true,
            calibration_samples: 60,
            calibration_required: 60,
            calibration_hint: None,
            above_for_s: 0u64,
        });
        drop(observation);

        // The threshold in force, the target it rises toward, and how much
        // the histogram has seen are published together.
        let body = serde_json::to_value(status_body(&state)).unwrap();
        let pressure = &body["resources"][0]["pressure"];
        assert_eq!(pressure["threshold_pct"], 34.0);
        assert_eq!(pressure["target_pct"], 40.0);
        assert_eq!(pressure["baseline_pct"], 25.0);
        assert_eq!(pressure["learned_samples"], 37_440);
        assert_eq!(pressure["learned_s"], 187_200);

        state.resources["gpu0"]
            .pressure
            .write()
            .unwrap()
            .record_failure("adapter missing from batch");
        let body = serde_json::to_value(status_body(&state)).unwrap();
        let pressure = &body["resources"][0]["pressure"];
        assert_eq!(pressure["quality"], "unknown");
        assert_eq!(pressure["last_error"], "adapter missing from batch");
        assert!(pressure["non_lender_pct"].is_null());
        assert!(pressure["engaged"].is_null());
        assert!(pressure["threshold_pct"].is_null());
        assert!(pressure["baseline_pct"].is_null());
        assert!(pressure["target_pct"].is_null());
        assert!(pressure["learned_s"].is_null());
    }

    #[test]
    fn status_exposes_exact_adapter_mapping_and_provenance() {
        let state = test_state();
        state.resources["gpu0"]
            .adapter_mapping
            .write()
            .unwrap()
            .record_success(AdapterMapping {
                pci: "0000:0e:00.0".parse().unwrap(),
                luid: "0x00000000_0x00013fa9".into(),
                source: AdapterMappingSource::Probe,
            });

        let body = serde_json::to_value(status_body(&state)).unwrap();
        let mapping = &body["resources"][0]["adapter_mapping"];
        assert_eq!(mapping["quality"], "ready");
        assert_eq!(mapping["pci"], "0000:0e:00.0");
        assert_eq!(mapping["luid"], "0x00000000_0x00013fa9");
        assert_eq!(mapping["source"], "probe");
    }

    #[test]
    fn status_explains_the_presence_basis_and_which_sessions_count() {
        let state = test_state();
        *state.presence.write().unwrap() = crate::plugin::presence::PresenceSignal {
            session: crate::plugin::presence::SessionState::LoggedOut,
            basis: Some("logind: no local or graphical user session".into()),
            sessions: vec![crate::plugin::presence::SessionView {
                id: "951".into(),
                class: "user".into(),
                kind: "tty".into(),
                remote: true,
                seat: None,
                state: "active".into(),
                active: true,
                locked: false,
                counted: false,
                reason: "remote non-graphical login (SSH)".into(),
            }],
            ..Default::default()
        };

        let body = serde_json::to_value(status_body(&state)).unwrap();
        let presence = &body["presence"];
        assert_eq!(presence["session"], "logged_out");
        assert_eq!(
            presence["basis"],
            "logind: no local or graphical user session"
        );
        assert_eq!(presence["sessions"][0]["type"], "tty");
        assert_eq!(presence["sessions"][0]["counted"], false);
        assert!(presence["sessions"][0].get("seat").is_none());
        let decoded = wire::decode(&serde_json::to_vec(&body).unwrap()).unwrap();
        assert_eq!(decoded.presence.sessions.len(), 1);
        assert_eq!(body["session_authority"]["state"], "not_required");
    }

    #[test]
    fn status_and_metrics_expose_bounded_agent_rejection_state() {
        let state = test_state();
        *state.presence.write().unwrap() = crate::plugin::presence::PresenceSignal {
            session: crate::plugin::presence::SessionState::Active,
            session_id: Some(7),
            applicable_session_ids: vec![7],
            session_authority: crate::plugin::presence::SessionAuthority::Missing,
            session_requirements: vec![crate::plugin::presence::SessionRequirement {
                session_id: 7,
                fresh_after: std::time::Instant::now(),
            }],
            foreground: None,
            owner_app: None,
            idle_seconds: None,
            media: crate::plugin::presence::MediaState::Idle,
            basis: None,
            sessions: Vec::new(),
        };
        state.record_agent_rejection(crate::agent::AgentRejectReason::VersionMismatch);

        let body = serde_json::to_value(status_body(&state)).unwrap();
        assert_eq!(body["session_authority"]["state"], "missing");
        assert_eq!(
            body["agent_channel"]["last_rejection"]["reason"],
            "version_mismatch"
        );
        let metrics = state.metrics.render();
        assert!(metrics.contains("vacancyd_agent_rejections_total{reason=\"version_mismatch\"} 1"));
    }

    #[test]
    fn readiness_metrics_are_evaluated_when_scraped() {
        let state = test_state();
        refresh_resource_readiness_metrics(&state);
        let unknown = state.metrics.render();
        assert!(unknown.contains("vacancyd_resource_readiness"));
        assert!(unknown.contains("state=\"unknown\"} 1"));
        assert!(unknown.contains("vacancyd_resource_observation_age_seconds"));
        assert!(unknown.contains("input=\"pressure\""));
        assert!(unknown.contains("input=\"adapter_mapping\""));

        state.resources["gpu0"]
            .capacity
            .write()
            .unwrap()
            .record_success(Capacity::default());
        refresh_resource_readiness_metrics(&state);
        let ready = state.metrics.render();
        assert!(ready.contains("state=\"ready\"} 1"));
        assert!(ready.contains("state=\"unknown\"} 0"));

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
        state.resources["gpu0"]
            .adapter_mapping
            .write()
            .unwrap()
            .record_success(AdapterMapping {
                pci: "0000:0e:00.0".parse().unwrap(),
                luid: "0x00000000_0x00013fa9".into(),
                source: AdapterMappingSource::Probe,
            });
        refresh_resource_readiness_metrics(&state);
        let pressure_ready = state.metrics.render();
        assert!(pressure_ready.contains("input=\"pressure\",resource=\"gpu0\",state=\"ready\"} 1"));
        assert!(pressure_ready
            .contains("input=\"adapter_mapping\",resource=\"gpu0\",state=\"ready\"} 1"));
    }

    #[test]
    fn service_readiness_requires_listeners_and_fresh_mandatory_inputs() {
        let state = test_state();
        let startup = state.readiness_failures();
        assert!(startup.iter().any(|reason| reason.contains("presence")));
        assert!(startup
            .iter()
            .any(|reason| reason.contains("not listening")));
        assert!(startup.iter().any(|reason| reason.contains("capacity")));
        assert!(startup.iter().any(|reason| reason.contains("pressure")));
        assert!(startup
            .iter()
            .any(|reason| reason.contains("adapter mapping")));
        assert!(!status_body(&state).ready);

        state.mark_presence_sampled();
        state
            .ready_upstreams
            .write()
            .unwrap()
            .insert("ollama".into());
        state.resources["gpu0"]
            .capacity
            .write()
            .unwrap()
            .record_success(Capacity::default());
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
        state.resources["gpu0"]
            .adapter_mapping
            .write()
            .unwrap()
            .record_success(AdapterMapping {
                pci: "0000:0e:00.0".parse().unwrap(),
                luid: "0x00000000_0x00013fa9".into(),
                source: AdapterMappingSource::Probe,
            });

        assert!(state.readiness_failures().is_empty());
        assert!(status_body(&state).ready);
        assert_eq!(scope_for("/readyz"), None);

        let mut tracker = crate::plugin::pressure::PressureTracker::new(Default::default());
        state.resources["gpu0"]
            .pressure
            .write()
            .unwrap()
            .record_success(tracker.observe(0, 8.0, false));
        let body = serde_json::to_value(status_body(&state)).unwrap();
        assert_eq!(body["ready"], false);
        let pressure = &body["resources"][0]["pressure"];
        assert_eq!(
            pressure["quality"], "ready",
            "a readable observation can still be uncalibrated"
        );
        assert_eq!(pressure["calibrated"], false);
        assert_eq!(pressure["calibration_samples"], 1);
        assert!(pressure["calibration_hint"]
            .as_str()
            .unwrap()
            .contains("collecting samples"));
        assert_eq!(pressure["learned_samples"], 1);
        assert_eq!(pressure["target_pct"], serde_json::Value::Null);
    }

    async fn json_body(response: axum::response::Response) -> serde_json::Value {
        let bytes = axum::body::to_bytes(response.into_body(), 1 << 20)
            .await
            .unwrap();
        serde_json::from_slice(&bytes).unwrap()
    }

    #[tokio::test]
    async fn a_debug_window_needs_an_administrator_and_a_bounded_length() {
        let mut inner = test_state_with_auth(
            "\n[[auth.token]]\nname = \"operator\"\nscopes = [\"admin\"]\nvalue = \"0123456789abcdef\"\n",
        );
        inner.local_admin_policy = LOOPBACK_READ_AND_PAUSE;
        let state = Arc::new(inner);
        let send = |request| routes(state.clone()).oneshot(request);
        let token = Some("Bearer 0123456789abcdef");

        let refused = send(loopback_admin(
            Method::POST,
            "/admin/debug/enable",
            None,
            Body::empty(),
        ))
        .await
        .unwrap();
        assert_eq!(refused.status(), StatusCode::FORBIDDEN);
        assert_eq!(
            json_body(refused).await["error"]["type"],
            "administrator_required"
        );

        let mut without_intent =
            loopback_admin(Method::POST, "/admin/debug/enable", token, Body::empty());
        without_intent.headers_mut().remove(ADMIN_INTENT_HEADER);
        assert_eq!(
            send(without_intent).await.unwrap().status(),
            StatusCode::FORBIDDEN
        );
        assert_eq!(state.log.status().source, "config");

        for invalid in [
            r#"{"minutes":0}"#,
            r#"{"minutes":241}"#,
            r#"{"minutes":30,"level":"trace"}"#,
            "30",
        ] {
            let response = send(loopback_admin(
                Method::POST,
                "/admin/debug/enable",
                token,
                Body::from(invalid),
            ))
            .await
            .unwrap();
            assert_eq!(response.status(), StatusCode::BAD_REQUEST, "{invalid}");
        }
        assert_eq!(state.log.status().debug_until, None);

        let opened = send(loopback_admin(
            Method::POST,
            "/admin/debug/enable",
            token,
            Body::empty(),
        ))
        .await
        .unwrap();
        assert_eq!(opened.status(), StatusCode::OK);
        let opened = json_body(opened).await;
        assert_eq!(opened["level"], "debug");
        assert_eq!(opened["source"], "debug_window");
        assert_eq!(opened["debug_remaining_s"], 30 * 60);
        assert_eq!(opened["requested_minutes"], 30);
        assert_eq!(opened["granted_minutes"], 30);
        assert_eq!(opened["limited"], false);

        let shortened = send(loopback_admin(
            Method::POST,
            "/admin/debug/enable",
            token,
            Body::from(r#"{"minutes":5}"#),
        ))
        .await
        .unwrap();
        assert_eq!(json_body(shortened).await["debug_remaining_s"], 5 * 60);

        let status = serde_json::to_value(status_body(&state)).unwrap();
        assert_eq!(status["logging"]["source"], "debug_window");
        assert!(status["logging"]["debug_until"].is_string());

        let ended = send(loopback_admin(
            Method::POST,
            "/admin/debug/disable",
            token,
            Body::empty(),
        ))
        .await
        .unwrap();
        assert_eq!(ended.status(), StatusCode::OK);
        let ended = json_body(ended).await;
        assert_eq!(ended["source"], "config");
        assert!(ended.get("debug_until").is_none());

        let unknown = send(loopback_admin(
            Method::POST,
            "/admin/debug/toggle",
            token,
            Body::empty(),
        ))
        .await
        .unwrap();
        assert_eq!(unknown.status(), StatusCode::NOT_FOUND);
    }

    #[test]
    fn pipe_debug_windows_follow_the_administrator_rule() {
        let mut inner = test_state();
        inner.local_admin_policy = LOOPBACK_READ_AND_PAUSE;
        let state = inner;
        let user = Caller::Account {
            account: "uid:1000".into(),
            administrator: false,
        };
        let administrator = Caller::Account {
            account: "uid:0".into(),
            administrator: true,
        };
        let start = || RequestedAction::StartDebug { minutes: 30 };
        assert_eq!(
            perform_local_admin(&state, &user, start()),
            Response::Refused {
                reason: RefusalReason::AdministratorRequired
            }
        );
        assert_eq!(state.log.status().debug_until, None);
        for minutes in [0, 241] {
            assert_eq!(
                perform_local_admin(
                    &state,
                    &administrator,
                    RequestedAction::StartDebug { minutes }
                ),
                Response::Refused {
                    reason: RefusalReason::Validation
                },
                "{minutes}"
            );
        }
        let opened = perform_local_admin(&state, &administrator, start());
        assert!(opened.acknowledges(&start()), "{opened:?}");
        assert_eq!(
            perform_local_admin(&state, &user, RequestedAction::EndDebug {}),
            Response::Refused {
                reason: RefusalReason::AdministratorRequired
            }
        );
        let ended = perform_local_admin(&state, &administrator, RequestedAction::EndDebug {});
        assert!(
            ended.acknowledges(&RequestedAction::<serde_json::Value>::EndDebug {}),
            "{ended:?}"
        );
    }

    #[tokio::test]
    async fn any_local_reader_may_export_diagnostics_with_explicit_intent() {
        let mut inner = test_state();
        inner.local_admin_policy = LOOPBACK_READ_AND_PAUSE;
        let state = Arc::new(inner);
        let send = |request| routes(state.clone()).oneshot(request);

        let exported = send(loopback_admin(
            Method::GET,
            "/admin/diagnostics",
            None,
            Body::empty(),
        ))
        .await
        .unwrap();
        assert_eq!(exported.status(), StatusCode::OK);
        let document = json_body(exported).await;
        assert_eq!(
            document["schema_version"],
            crate::diagnostics::SCHEMA_VERSION
        );
        assert_eq!(document["status"]["daemon"]["name"], "vacancyd");
        assert!(document["pressure_history"]["gpu0"]["samples"].is_array());

        let mut without_intent =
            loopback_admin(Method::GET, "/admin/diagnostics", None, Body::empty());
        without_intent.headers_mut().remove(ADMIN_INTENT_HEADER);
        assert_eq!(
            send(without_intent).await.unwrap().status(),
            StatusCode::FORBIDDEN
        );
        let mut from_a_page =
            loopback_admin(Method::GET, "/admin/diagnostics", None, Body::empty());
        from_a_page.headers_mut().insert(
            axum::http::header::ORIGIN,
            HeaderValue::from_static("http://example.com"),
        );
        assert_eq!(
            send(from_a_page).await.unwrap().status(),
            StatusCode::FORBIDDEN
        );
        // Reading is not changing: a POST to the export is an unknown admin
        // action, which only an administrator may attempt.
        let posted = send(loopback_admin(
            Method::POST,
            "/admin/diagnostics",
            None,
            Body::empty(),
        ))
        .await
        .unwrap();
        assert_eq!(posted.status(), StatusCode::FORBIDDEN);
    }
}
