//! Independent sampling loops for capacity, host presence, and attributed
//! pressure. Slow vendor and OS tools run on blocking workers and cannot
//! delay the presence decision that gives the machine back to its owner.
//!
//! Measurement stance worth keeping: on Windows, per-process VRAM for
//! *graphics* processes is not reliably readable (NVML reports N/A, and the
//! WDDM per-process counters over-report by a factor of three or more).
//! Total free VRAM from NVML is accurate, and session state answers "is the
//! owner here". Together they are sufficient, cheap, and they allocate
//! nothing on the hardware being measured.

use std::sync::atomic::Ordering;
use std::sync::Arc;
use std::time::Duration;

use tracing::{debug, info, warn};

pub mod health;
pub mod pressure;
pub mod unseen_work;

use crate::plugin::presence;
use crate::state::AppState;
use crate::yield_control;

pub async fn run(state: Arc<AppState>, config_path: std::path::PathBuf) {
    let interval = Duration::from_millis(state.config.poll.interval_ms);
    tokio::join!(
        capacity_loop(state.clone(), interval),
        presence_loop(state.clone(), interval),
        health::run(state.clone()),
        crate::lifecycle::run(state.clone()),
        pressure_loop(state, config_path),
    );
}

/// Floor between sampling passes started by job completions. A GPU sample
/// spawns `nvidia-smi`, and a borrower retrying against a stopped upstream
/// ends a job on every attempt.
const WAKE_SPACING: Duration = Duration::from_millis(250);

async fn capacity_loop(state: Arc<AppState>, interval: Duration) {
    loop {
        let started = tokio::time::Instant::now();
        sample_capacity(&state).await;
        // A wake that arrives while sampling is kept as a permit, so the end
        // of a job always gets a sample that started after it.
        let woken = async {
            state.capacity_wake.notified().await;
            tokio::time::sleep_until(started + WAKE_SPACING).await;
        };
        tokio::select! {
            _ = tokio::time::sleep(interval) => {}
            _ = woken => {}
        }
    }
}

async fn sample_capacity(state: &Arc<AppState>) {
    for rc in &state.config.resources {
        let id = rc.id.clone();
        let worker_id = id.clone();
        let worker_state = state.clone();
        let reserve = rc.reserve;
        let sampled_at = std::time::Instant::now();
        let sampled = tokio::task::spawn_blocking(move || {
            let mut probe = worker_state.resources[&worker_id].probe.lock().unwrap();
            let sampled = probe.sample();
            worker_state.metrics.gpu.record(
                &worker_id,
                sampled.as_ref().ok().and_then(|_| probe.gpu_telemetry()),
            );
            let capacity = sampled?;
            let lendable = probe.lendable(&capacity, reserve);
            Ok::<_, anyhow::Error>((capacity, lendable))
        })
        .await
        .expect("capacity probe worker panicked");
        let rs = &state.resources[&id];
        let cap = match sampled {
            Ok((capacity, lendable)) => {
                let mut observation = rs.capacity.write().unwrap();
                if let Some(previous) = observation.last_error() {
                    info!(resource = id, previous, "probe recovered");
                }
                rs.probe_lendable.store(lendable, Ordering::Relaxed);
                observation.record_success_at(capacity.clone(), sampled_at);
                capacity
            }
            Err(error) => {
                let error = error.to_string();
                let mut observation = rs.capacity.write().unwrap();
                // Capacity is sampled every few seconds: a lasting failure is
                // reported when it starts or changes, not on every poll.
                if observation.last_error() == Some(error.as_str()) {
                    debug!(resource = id, error, "probe still failing");
                } else {
                    warn!(resource = id, error, "probe failed");
                }
                rs.probe_lendable.store(false, Ordering::Relaxed);
                observation.record_failure(error);
                continue;
            }
        };
        let metrics = &state.metrics;
        // CPU capacity is a percentage, not bytes. Do not create plausible
        // zero-valued byte series for a unit that does not exist.
        if rc.kind != "system-cpu" {
            metrics
                .resource_free_bytes
                .with_label_values(&[&id])
                .set(cap.free_bytes as i64);
            metrics
                .resource_total_bytes
                .with_label_values(&[&id])
                .set(cap.total_bytes as i64);
        }
        metrics
            .resource_utilization_pct
            .with_label_values(&[&id])
            .set(cap.utilization_pct.map_or(-1, i64::from));
        metrics
            .resource_enabled
            .with_label_values(&[&id])
            .set(rs.enabled.load(Ordering::Relaxed) as i64);
    }
}

async fn presence_loop(state: Arc<AppState>, interval: Duration) {
    let mut presence_src = presence::for_platform(state.config.owner.presence_options());
    let presence_kind = presence_src.kind();
    let fresh_for = state.presence_fresh_for();
    // Start time of the reading in force; the first one is due a bound after
    // the loop starts.
    let mut in_force = std::time::Instant::now();
    loop {
        let processes = state.config.owner.processes.clone();
        // The sample describes the session no later than when it started.
        let sampled_at = std::time::Instant::now();
        let mut sampling = tokio::task::spawn_blocking(move || {
            let signal = presence_src.sample(&processes);
            (presence_src, signal)
        });
        // A blocking sample cannot be cancelled, and the backend comes back
        // with its result. If the reading in force expires first, engage the
        // owner veto, then keep waiting. A fresh reading is still subject to
        // the normal release hold.
        let expires = tokio::time::Instant::from_std(in_force + fresh_for);
        let (returned, signal) = match tokio::time::timeout_at(expires, &mut sampling).await {
            Ok(joined) => joined.expect("presence probe worker panicked"),
            Err(_) => {
                presence_expired(&state);
                sampling.await.expect("presence probe worker panicked")
            }
        };
        presence_src = returned;
        // A sample that took longer than the bound describes a session that
        // may have changed since; it must not release the veto or teach
        // pressure an attendance profile.
        if sampled_at.elapsed() > fresh_for {
            debug!(
                took_s = sampled_at.elapsed().as_secs(),
                "discarding a presence sample that finished after its freshness bound"
            );
            continue;
        }
        publish_presence(&state, signal, sampled_at).await;
        in_force = sampled_at;
        state.presence_kind.set(presence_kind).ok();
        tokio::time::sleep(interval).await;
    }
}

/// The reading in force has outlived its freshness bound and no new one has
/// arrived. Engage the owner veto as a sample showing the owner would, so
/// work admitted on the old reading is reclaimed under the configured
/// policy, not only refused from now on.
fn presence_expired(state: &Arc<AppState>) {
    warn!(
        waited_s = state.presence_fresh_for().as_secs(),
        "presence sampling has not finished; host-scoped work requires a fresh presence reading"
    );
    let rising = state.veto.observe(
        Some(STALE_PRESENCE_CAUSE.into()),
        state.config.owner.release_after_s,
    );
    state.metrics.owner_veto_active.set(1);
    if rising {
        tokio::spawn(yield_control::on_veto_engaged(state.clone()));
    }
}

/// The cause recorded when a presence reading expires. It remains until
/// another cause replaces it or the veto releases.
const STALE_PRESENCE_CAUSE: &str = "presence reading expired";

async fn publish_presence(
    state: &Arc<AppState>,
    mut signal: presence::PresenceSignal,
    sampled_at: std::time::Instant,
) {
    let owner = &state.config.owner;

    // Read every applicable session from one map snapshot. Idle becomes the
    // minimum only when all active sessions have a fresh post-transition
    // report; one missing reporter keeps the conservative veto engaged.
    let observations = state.agent_reports_for_requirements(&signal.session_requirements);
    let all_ready = !observations.is_empty() && observations.iter().all(Option::is_some);
    if all_ready {
        signal.session_authority = presence::SessionAuthority::Ready;
        let reports = observations
            .into_iter()
            .flatten()
            .map(|observation| observation.report)
            .collect::<Vec<_>>();
        signal.idle_seconds = reports.iter().map(|report| report.idle_seconds).min();
        if signal.foreground.is_none() {
            signal.foreground = reports.iter().find_map(|report| report.foreground.clone());
        }
        signal.owner_app = reports.iter().find_map(|report| {
            report.foreground.as_ref().and_then(|foreground| {
                owner
                    .processes
                    .iter()
                    .any(|process| foreground.eq_ignore_ascii_case(process))
                    .then(|| foreground.clone())
            })
        });
    } else if !signal.session_requirements.is_empty()
        && signal.session_authority != presence::SessionAuthority::Ambiguous
    {
        signal.session_authority = presence::SessionAuthority::Missing;
        signal.idle_seconds = None;
    }
    state.metrics.agent_connected.set(all_ready as i64);
    state
        .metrics
        .owner_idle_seconds
        .set(signal.idle_seconds.map(|s| s as i64).unwrap_or(-1));
    state.metrics.media_playing.set(match signal.media {
        presence::MediaState::Playing(_) => 1,
        presence::MediaState::Idle => 0,
        // Unknown and NotChecked are distinct causes with the same
        // observability need: the borrower should see that this input is not
        // being read, rather than see a confident zero.
        _ => -1,
    });
    let verdict = match presence::assess(
        &signal,
        owner.unlocked_session_is_presence,
        owner.idle_lends_after_s,
    ) {
        presence::Verdict::Present(reason) => Some(reason),
        presence::Verdict::Lends(release) => {
            // The backend's basis describes the session ("active and
            // unlocked"); say which policy lets it lend anyway.
            if let Some(release) = release {
                signal.basis = Some(match signal.basis.take() {
                    Some(basis) => format!("{basis}; {}", release.describe()),
                    None => release.describe(),
                });
            }
            None
        }
    };
    // The veto cause borrowers see names only the failed input; the basis,
    // local diagnostics, carries why it failed.
    if let presence::MediaState::Unknown(error) = &signal.media {
        let clause = format!("media detection unavailable: {error}");
        signal.basis = Some(match signal.basis.take() {
            Some(basis) => format!("{basis}; {clause}"),
            None => clause,
        });
    }
    let rising = state.veto.observe(verdict, owner.release_after_s);
    let (active, cause) = state.veto.status();

    // Log every transition, with capacity, at INFO. Without this the only
    // record of an overnight lending window is the yield that ends it — the
    // interesting part (when it opened, and how much was free while it was
    // open) is invisible after the fact. Costs one line per transition.
    {
        let mut last = state.last_logged_state.lock().unwrap();
        let now = (signal.session, active);
        if *last != Some(now) {
            tracing::info!(
                session = session_label(signal.session),
                veto = active,
                cause = cause.as_deref().unwrap_or(""),
                basis = signal.basis.as_deref().unwrap_or(""),
                free = capacity_summary(state),
                "presence transition"
            );
            *last = Some(now);
        }
    }
    state.metrics.owner_veto_active.set(active as i64);
    state
        .metrics
        .session_state
        .with_label_values(&[session_label(signal.session)])
        .set(1);
    for s in ["active", "locked", "disconnected", "logged_out", "unknown"] {
        if s != session_label(signal.session) {
            state.metrics.session_state.with_label_values(&[s]).set(0);
        }
    }
    *state.presence.write().unwrap() = signal;
    *state.presence_sampled_at.write().unwrap() = Some(sampled_at);
    debug!(
        veto = active,
        cause = cause.as_deref().unwrap_or(""),
        "poll"
    );

    if rising {
        tokio::spawn(yield_control::on_veto_engaged(state.clone()));
    }
}

async fn pressure_loop(state: Arc<AppState>, config_path: std::path::PathBuf) {
    let interval = Duration::from_millis(state.config.owner.gpu_pressure.poll_ms.max(1_000));
    let mut engine = pressure::PressureEngine::new(&state, &config_path);
    loop {
        // Do not put the first sample into the attended baseline merely
        // because the independent presence loop has not published yet.
        if state.presence_kind.get().is_none() {
            tokio::time::sleep(Duration::from_millis(50)).await;
            continue;
        }
        let worker_state = state.clone();
        let (returned, refused) = tokio::task::spawn_blocking(move || {
            let refused = engine.poll(&worker_state);
            (engine, refused)
        })
        .await
        .expect("pressure probe worker panicked");
        engine = returned;
        if !refused.is_empty() {
            let vetoed = state.veto.status().0;
            tokio::spawn(yield_control::on_resources_refused(
                state.clone(),
                refused,
                vetoed,
            ));
        }
        tokio::time::sleep(interval).await;
    }
}

/// Capacity as the presence-transition line reports it. A resource that has
/// never been sampled says so rather than reading as `unknown`, which means
/// a sample failed or went stale; before any sample at all the whole field
/// says so.
fn capacity_summary(state: &AppState) -> String {
    use crate::state::ObservationQuality;

    let mut sampled_any = false;
    let parts: Vec<String> = state
        .config
        .resources
        .iter()
        .filter(|rc| state.resource_enabled(&rc.id))
        .map(|rc| {
            let view = state
                .capacity_view(&rc.id)
                .expect("configured resource has runtime state");
            if view.value.is_none() && view.last_error.is_none() {
                return format!("{}=not_sampled", rc.id);
            }
            sampled_any = true;
            let reading = (view.quality == ObservationQuality::Ready)
                .then_some(view.value.as_ref())
                .flatten()
                .and_then(|cap| {
                    // Free MiB is meaningless for a CPU — its lendability
                    // test is load, so report the number actually being
                    // judged rather than a confident-looking zero.
                    if rc.kind == "system-cpu" {
                        cap.utilization_pct.map(|percent| format!("{percent}%busy"))
                    } else {
                        Some(format!("{}MiB", cap.free_bytes / (1024 * 1024)))
                    }
                });
            match reading {
                Some(reading) => format!("{}={reading}", rc.id),
                None => format!(
                    "{}={}",
                    rc.id,
                    match view.quality {
                        ObservationQuality::Stale => "stale",
                        _ => "unknown",
                    }
                ),
            }
        })
        .collect();
    if sampled_any || parts.is_empty() {
        parts.join(" ")
    } else {
        "not yet sampled".into()
    }
}

fn session_label(s: presence::SessionState) -> &'static str {
    use presence::SessionState as S;
    match s {
        S::Active => "active",
        S::Locked => "locked",
        S::Disconnected => "disconnected",
        S::LoggedOut => "logged_out",
        S::Unknown => "unknown",
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::plugin::presence::{MediaState, PresenceSignal, SessionState};
    use crate::plugin::probe::{Capacity, Probe};

    struct SlowProbe;

    fn report(session_id: u32, idle_seconds: u64) -> crate::agent::AgentReport {
        crate::agent::AgentReport {
            protocol_version: crate::agent::AGENT_PROTOCOL_VERSION,
            sequence: 1,
            session_id,
            idle_seconds,
            foreground: None,
            agent_version: env!("CARGO_PKG_VERSION").into(),
            sampled_at_unix_ms: crate::agent::unix_time_ms(),
        }
    }

    impl Probe for SlowProbe {
        fn kind(&self) -> &'static str {
            "slow"
        }

        fn sample(&mut self) -> anyhow::Result<Capacity> {
            std::thread::sleep(Duration::from_millis(400));
            Ok(Capacity::default())
        }
    }

    #[tokio::test]
    async fn an_expired_presence_reading_engages_the_veto_and_reclaims_host_scoped_work() {
        // The owner was away, borrowed work started, then the presence
        // sampler stalled while the owner may have come back.
        let config: crate::config::Config = toml::from_str(
            r#"
            [server]
            listen = "127.0.0.1:0"
            [owner.gpu_pressure]
            enabled = false
            [[resource]]
            id = "mem"
            kind = "system-memory"
            [[upstream]]
            id = "work"
            listen = "127.0.0.1:0"
            target = "http://127.0.0.1:11434"
            kind = "ollama"
            resource = "mem"
            on_owner_return = "cancel"
            "#,
        )
        .unwrap();
        let state = Arc::new(AppState::new(config).unwrap());
        state.mark_presence_sampled();
        let upstream = &state.config.upstreams[0];
        let ticket = Some(crate::work_cancellation::WorkCancellation::for_upstream(
            &state, upstream,
        ));

        presence_expired(&state);

        let (active, cause) = state.veto.status();
        assert!(active);
        assert_eq!(cause.as_deref(), Some(STALE_PRESENCE_CAUSE));
        tokio::time::timeout(
            Duration::from_secs(2),
            crate::work_cancellation::cancelled(&ticket),
        )
        .await
        .expect("work admitted on the old reading is cancelled");
        assert!(crate::work_cancellation::trigger(&ticket).unwrap().owner);
        // A second expiry while the veto holds raises no new edge.
        assert!(!state.veto.observe(Some(STALE_PRESENCE_CAUSE.into()), 0));
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn a_slow_capacity_probe_does_not_delay_presence_publication() {
        let config: crate::config::Config = toml::from_str(
            r#"
            [server]
            listen = "127.0.0.1:9811"
            [[resource]]
            id = "mem"
            kind = "system-memory"
            "#,
        )
        .unwrap();
        let mut state = AppState::new(config).unwrap();
        state.resources.get_mut("mem").unwrap().probe = std::sync::Mutex::new(Box::new(SlowProbe));
        let state = Arc::new(state);
        let worker_state = state.clone();
        let sampling = tokio::spawn(async move { sample_capacity(&worker_state).await });
        tokio::time::sleep(Duration::from_millis(25)).await;

        let started = std::time::Instant::now();
        publish_presence(
            &state,
            PresenceSignal {
                session: SessionState::Active,
                session_id: Some(1),
                applicable_session_ids: vec![1],
                session_authority: crate::plugin::presence::SessionAuthority::Missing,
                session_requirements: vec![crate::plugin::presence::SessionRequirement {
                    session_id: 1,
                    fresh_after: std::time::Instant::now(),
                }],
                foreground: None,
                owner_app: None,
                idle_seconds: Some(0),
                media: MediaState::Idle,
                basis: None,
                sessions: Vec::new(),
            },
            std::time::Instant::now(),
        )
        .await;

        assert!(started.elapsed() < Duration::from_millis(200));
        assert!(state.veto.status().0);
        sampling.await.unwrap();
    }

    fn memory_and_cpu(owner: &str) -> Arc<AppState> {
        let config: crate::config::Config = toml::from_str(&format!(
            r#"
            [server]
            listen = "127.0.0.1:9811"
            [owner]
            {owner}
            [[resource]]
            id = "mem"
            kind = "system-memory"
            [[resource]]
            id = "cpu"
            kind = "system-cpu"
            "#
        ))
        .unwrap();
        Arc::new(AppState::new(config).unwrap())
    }

    #[test]
    fn live_61_capacity_before_any_sample_says_not_yet_sampled() {
        let state = memory_and_cpu("");
        assert_eq!(capacity_summary(&state), "not yet sampled");

        state.resources["mem"]
            .capacity
            .write()
            .unwrap()
            .record_success(Capacity {
                free_bytes: 2048 * 1024 * 1024,
                total_bytes: 4096 * 1024 * 1024,
                ..Capacity::default()
            });
        assert_eq!(capacity_summary(&state), "mem=2048MiB cpu=not_sampled");

        // A failed sample is unknown, which is not the same as never read.
        state.resources["cpu"]
            .capacity
            .write()
            .unwrap()
            .record_failure("no logical CPUs were enumerated");
        assert_eq!(capacity_summary(&state), "mem=2048MiB cpu=unknown");
        state.resources["cpu"]
            .capacity
            .write()
            .unwrap()
            .record_success(Capacity {
                utilization_pct: Some(7),
                ..Capacity::default()
            });
        assert_eq!(capacity_summary(&state), "mem=2048MiB cpu=7%busy");
    }

    fn linux_desktop(idle_seconds: Option<u64>) -> PresenceSignal {
        PresenceSignal {
            session: SessionState::Active,
            session_authority: crate::plugin::presence::SessionAuthority::NotRequired,
            idle_seconds,
            media: MediaState::NotChecked,
            basis: Some("logind: session 2 active and unlocked".into()),
            ..PresenceSignal::default()
        }
    }

    #[tokio::test]
    async fn live_61_the_first_transition_line_and_an_idle_lend_explain_themselves() {
        let log = crate::test_log::capture();
        let state = memory_and_cpu("idle_lends_after_s = 60\nrelease_after_s = 0");

        // Before logind's view is confirmed, and before any capacity sample.
        publish_presence(
            &state,
            PresenceSignal {
                session: SessionState::Unknown,
                session_authority: crate::plugin::presence::SessionAuthority::Ambiguous,
                basis: Some(
                    "logind: session view not yet confirmed by a second identical read".into(),
                ),
                ..PresenceSignal::default()
            },
            std::time::Instant::now(),
        )
        .await;
        assert!(state.veto.status().0);

        // Unlocked, idle past the threshold: lending opens, and the basis
        // says why instead of only "active and unlocked".
        publish_presence(&state, linux_desktop(Some(70)), std::time::Instant::now()).await;
        assert!(!state.veto.status().0);
        let basis = state.presence.read().unwrap().basis.clone().unwrap();
        assert_eq!(
            basis,
            "logind: session 2 active and unlocked; idle lending: no input for 70 s (owner.idle_lends_after_s = 60)"
        );

        // Touched again: presence, and the basis is the backend's own.
        publish_presence(&state, linux_desktop(Some(0)), std::time::Instant::now()).await;
        assert!(state.veto.status().0);
        assert_eq!(
            state.presence.read().unwrap().basis.as_deref(),
            Some("logind: session 2 active and unlocked")
        );

        let logged = log.contents();
        let lines = logged
            .lines()
            .filter(|line| line.contains("presence transition"))
            .collect::<Vec<_>>();
        assert_eq!(lines.len(), 3, "{logged}");
        assert!(lines[0].contains("session=\"unknown\""), "{}", lines[0]);
        assert!(!lines[0].contains("session=\"active\""), "{}", lines[0]);
        assert!(
            lines[0].contains("free=\"not yet sampled\""),
            "{}",
            lines[0]
        );
        assert!(lines[0].contains("not yet confirmed"), "{}", lines[0]);
        assert!(lines[1].contains("veto=false"), "{}", lines[1]);
        assert!(lines[1].contains("idle lending"), "{}", lines[1]);
        assert!(lines[2].contains("veto=true"), "{}", lines[2]);
    }

    #[tokio::test]
    async fn lending_while_unlocked_is_named_in_the_basis() {
        let state = memory_and_cpu("unlocked_session_is_presence = false\nrelease_after_s = 0");
        publish_presence(&state, linux_desktop(None), std::time::Instant::now()).await;
        assert!(!state.veto.status().0);
        assert_eq!(
            state.presence.read().unwrap().basis.as_deref(),
            Some("logind: session 2 active and unlocked; lending while unlocked (owner.unlocked_session_is_presence = false)")
        );
    }

    #[tokio::test]
    async fn unreadable_media_refuses_and_the_basis_names_the_failure() {
        // Lending while unlocked must not lend while media
        // cannot be read. Borrowers see only the failed input; the local
        // basis and media_error say why.
        let state = memory_and_cpu("unlocked_session_is_presence = false\nrelease_after_s = 0");
        let mut signal = linux_desktop(None);
        signal.media = MediaState::Unknown(
            "reading power requests requires elevation, and this process is not elevated".into(),
        );
        publish_presence(&state, signal, std::time::Instant::now()).await;
        let (active, cause) = state.veto.status();
        assert!(active);
        assert_eq!(
            cause.as_deref(),
            Some(crate::plugin::presence::MEDIA_UNKNOWN_CAUSE)
        );
        assert_eq!(
            state.presence.read().unwrap().basis.as_deref(),
            Some("logind: session 2 active and unlocked; media detection unavailable: reading power requests requires elevation, and this process is not elevated")
        );

        // Once media reads as idle, the same session lends.
        let mut signal = linux_desktop(None);
        signal.media = MediaState::Idle;
        publish_presence(&state, signal, std::time::Instant::now()).await;
        assert!(!state.veto.status().0);
    }

    struct ScriptedProbe(Arc<std::sync::Mutex<Vec<Result<(), &'static str>>>>);

    struct CachedTelemetryProbe {
        sampled: bool,
        telemetry: crate::plugin::probe::GpuTelemetry,
    }

    impl Probe for CachedTelemetryProbe {
        fn kind(&self) -> &'static str {
            "test-telemetry"
        }

        fn sample(&mut self) -> anyhow::Result<Capacity> {
            if self.sampled {
                anyhow::bail!("driver failure");
            }
            self.sampled = true;
            Ok(Capacity {
                free_bytes: 1024,
                total_bytes: 2048,
                ..Capacity::default()
            })
        }

        fn gpu_telemetry(&self) -> Option<&crate::plugin::probe::GpuTelemetry> {
            Some(&self.telemetry)
        }
    }

    #[tokio::test]
    async fn a_failed_capacity_sample_clears_even_a_probes_retained_telemetry() {
        let mut state = memory_and_cpu("");
        let state_mut = Arc::get_mut(&mut state).unwrap();
        state_mut.resources.get_mut("mem").unwrap().probe =
            std::sync::Mutex::new(Box::new(CachedTelemetryProbe {
                sampled: false,
                telemetry: crate::plugin::probe::GpuTelemetry {
                    uuid: "GPU-11111111-2222-3333-4444-555555555555".into(),
                    adapter: "0000:01:00.0".parse().unwrap(),
                    name: "Example GPU".into(),
                    core_temperature_celsius: Some(42.0),
                    memory_temperature_celsius: None,
                    power_watts: None,
                    fan_speed_percent: None,
                    sm_clock_megahertz: None,
                    memory_clock_megahertz: None,
                    memory_utilization_percent: None,
                },
            }));
        sample_capacity(&state).await;
        assert!(state
            .metrics
            .render()
            .contains("vacancyd_gpu_core_temperature_celsius{resource=\"mem\"} 42"));
        assert_eq!(
            state
                .capacity_view("mem")
                .unwrap()
                .value
                .unwrap()
                .free_bytes,
            1024
        );
        sample_capacity(&state).await;
        assert!(!state.metrics.render().contains("vacancyd_gpu_"));
        assert_eq!(
            state.capacity_view("mem").unwrap().last_error.as_deref(),
            Some("driver failure")
        );
    }

    impl Probe for ScriptedProbe {
        fn kind(&self) -> &'static str {
            "scripted"
        }

        fn sample(&mut self) -> anyhow::Result<Capacity> {
            match self.0.lock().unwrap().remove(0) {
                Ok(()) => Ok(Capacity {
                    free_bytes: 1024,
                    total_bytes: 2048,
                    ..Capacity::default()
                }),
                Err(error) => Err(anyhow::anyhow!(error)),
            }
        }
    }

    #[tokio::test]
    async fn a_lasting_probe_failure_is_warned_once_per_change() {
        let log = crate::test_log::capture();
        let config: crate::config::Config = toml::from_str(
            r#"
            [server]
            listen = "127.0.0.1:9811"
            [[resource]]
            id = "mem"
            kind = "system-memory"
            "#,
        )
        .unwrap();
        let script = Arc::new(std::sync::Mutex::new(vec![
            Err("nvidia-smi timed out"),
            Err("nvidia-smi timed out"),
            Err("nvidia-smi timed out"),
            Err("nvidia-smi exited with 9"),
            Ok(()),
            Ok(()),
            Err("nvidia-smi timed out"),
        ]));
        let mut state = AppState::new(config).unwrap();
        state.resources.get_mut("mem").unwrap().probe =
            std::sync::Mutex::new(Box::new(ScriptedProbe(script.clone())));
        let state = Arc::new(state);
        for _ in 0..7 {
            sample_capacity(&state).await;
        }
        assert!(script.lock().unwrap().is_empty());
        // The failure is still recorded on every poll.
        assert_eq!(
            state.capacity_view("mem").unwrap().last_error.as_deref(),
            Some("nvidia-smi timed out")
        );
        let logged = log.contents();
        let warned = logged
            .lines()
            .filter(|line| line.contains("probe failed"))
            .collect::<Vec<_>>();
        assert_eq!(warned.len(), 3, "{logged}");
        assert!(warned[0].contains("timed out"), "{logged}");
        assert!(warned[1].contains("exited with 9"), "{logged}");
        assert!(warned[2].contains("timed out"), "{logged}");
        assert_eq!(logged.matches("probe recovered").count(), 1, "{logged}");
    }

    #[tokio::test]
    async fn cpu_capacity_does_not_publish_byte_series() {
        let config: crate::config::Config = toml::from_str(
            r#"
            [server]
            listen = "127.0.0.1:9811"
            [[resource]]
            id = "cpu"
            kind = "system-cpu"
            "#,
        )
        .unwrap();
        let state = Arc::new(AppState::new(config).unwrap());
        sample_capacity(&state).await;
        let metrics = state.metrics.render();
        assert!(!metrics.contains("vacancyd_resource_free_bytes{resource=\"cpu\"}"));
        assert!(!metrics.contains("vacancyd_resource_total_bytes{resource=\"cpu\"}"));
        assert!(metrics.contains("vacancyd_resource_utilization_pct{resource=\"cpu\"}"));
    }

    #[tokio::test]
    async fn a_finished_job_gets_its_capacity_sample_before_the_poll_interval() {
        let config: crate::config::Config = toml::from_str(
            r#"
            [server]
            listen = "127.0.0.1:9811"
            [owner.gpu_pressure]
            enabled = false
            [[resource]]
            id = "mem"
            kind = "system-memory"
            [[upstream]]
            id = "u"
            listen = "127.0.0.1:0"
            target = "http://127.0.0.1:1"
            kind = "ollama"
            resource = "mem"
            "#,
        )
        .unwrap();
        let state = Arc::new(AppState::new(config).unwrap());
        state.mark_presence_sampled();
        let sampler = tokio::spawn(capacity_loop(state.clone(), Duration::from_secs(60)));
        let upstream = state.config.upstreams[0].clone();
        let ticket = tokio::time::timeout(Duration::from_secs(5), async {
            loop {
                if let Ok(ticket) = crate::admission::reserve(
                    &state,
                    &upstream,
                    None,
                    crate::plugin::adapter::Completion::ResponseClose,
                ) {
                    return ticket;
                }
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
        })
        .await
        .expect("the first capacity sample never arrived");
        let finished = std::time::Instant::now();
        ticket.response_complete();
        tokio::time::timeout(Duration::from_secs(5), async {
            while !state.resources["mem"]
                .capacity
                .read()
                .unwrap()
                .sampled_after(finished)
            {
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
        })
        .await
        .expect("no capacity sample followed the job; the next borrow would wait a full interval");
        sampler.abort();
    }

    #[tokio::test]
    async fn report_arrival_order_cannot_select_another_session() {
        let config: crate::config::Config = toml::from_str(
            r#"
            [server]
            listen = "127.0.0.1:9811"
            "#,
        )
        .unwrap();
        let state = Arc::new(AppState::new(config).unwrap());
        let session_7 = state.begin_agent_connection();
        let session_8 = state.begin_agent_connection();
        assert!(state.record_agent_report(
            report(7, 600),
            session_7,
            Duration::from_secs(15),
            Duration::ZERO,
            std::time::Instant::now()
        ));
        assert!(state.record_agent_report(
            report(8, 5),
            session_8,
            Duration::from_secs(15),
            Duration::ZERO,
            std::time::Instant::now()
        ));

        publish_presence(
            &state,
            PresenceSignal {
                session: SessionState::Active,
                session_id: Some(7),
                applicable_session_ids: vec![7],
                session_authority: crate::plugin::presence::SessionAuthority::Missing,
                session_requirements: vec![crate::plugin::presence::SessionRequirement {
                    session_id: 7,
                    fresh_after: std::time::Instant::now() - std::time::Duration::from_secs(1),
                }],
                foreground: None,
                owner_app: None,
                idle_seconds: None,
                media: MediaState::Idle,
                basis: None,
                sessions: Vec::new(),
            },
            std::time::Instant::now(),
        )
        .await;

        assert_eq!(state.presence.read().unwrap().idle_seconds, Some(600));
    }

    #[tokio::test]
    async fn every_active_session_participates_in_idle_and_owner_app_policy() {
        let config: crate::config::Config = toml::from_str(
            r#"
            [server]
            listen = "127.0.0.1:9811"
            [owner]
            processes = ["Marathon"]
            "#,
        )
        .unwrap();
        let state = Arc::new(AppState::new(config).unwrap());
        let session_7 = state.begin_agent_connection();
        let session_8 = state.begin_agent_connection();
        assert!(state.record_agent_report(
            report(7, 600),
            session_7,
            Duration::from_secs(15),
            Duration::ZERO,
            std::time::Instant::now()
        ));
        let mut protected = report(8, 900);
        protected.foreground = Some("MARATHON".into());
        assert!(state.record_agent_report(
            protected,
            session_8,
            Duration::from_secs(15),
            Duration::ZERO,
            std::time::Instant::now()
        ));

        publish_presence(
            &state,
            PresenceSignal {
                session: SessionState::Active,
                session_id: Some(7),
                applicable_session_ids: vec![7, 8],
                session_authority: crate::plugin::presence::SessionAuthority::Missing,
                session_requirements: vec![7, 8]
                    .into_iter()
                    .map(|session_id| crate::plugin::presence::SessionRequirement {
                        session_id,
                        fresh_after: std::time::Instant::now() - std::time::Duration::from_secs(1),
                    })
                    .collect(),
                foreground: None,
                owner_app: None,
                idle_seconds: None,
                media: MediaState::Idle,
                basis: None,
                sessions: Vec::new(),
            },
            std::time::Instant::now(),
        )
        .await;

        let presence = state.presence.read().unwrap();
        assert_eq!(presence.idle_seconds, Some(600));
        assert_eq!(presence.owner_app.as_deref(), Some("MARATHON"));
        assert_eq!(
            presence.session_authority,
            crate::plugin::presence::SessionAuthority::Ready
        );
    }
}
