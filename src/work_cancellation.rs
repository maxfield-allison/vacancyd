//! Owner-return and resource-pressure cancellation are independent of borrower backpressure.
//! A ticket is subscribed before admission and survives through the response
//! body or WebSocket. Repeated returns cannot extend its first deadline. A
//! drain whose trigger clears before that deadline stands down, and the
//! ticket waits for the next edge. When the edge also started a reclaim of
//! the upstream, the ticket follows that reclaim's decision rather than its
//! own, so the stream is cut exactly when the reclaim cancels the job.
use std::sync::Arc;
use std::time::Duration;

use tokio::sync::{broadcast, watch};
use tokio::task::JoinHandle;

use crate::config::{ReclaimMode, UpstreamConfig};
use crate::state::AppState;
use crate::yield_control::ReclaimDecision;

#[derive(Clone)]
pub struct WorkCancellation(Arc<Inner>);

struct Inner {
    cancelled: watch::Receiver<Option<Trigger>>,
    task: Option<JoinHandle<()>>,
}

/// What a cancelled ticket answered: the owner veto, pressure on some of its
/// resources, or both when their edges arrived together.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Trigger {
    pub owner: bool,
    pub resources: Vec<String>,
}

/// One edge source: the owner veto (`None`) or a resource's reclaims.
type Signal = (broadcast::Receiver<tokio::time::Instant>, Option<String>);

/// Whether a drain's trigger has cleared, given the resources whose edges
/// started it.
type Cleared = Box<dyn Fn(&[&str]) -> bool + Send>;

/// How often a draining ticket re-checks its trigger.
const TRIGGER_CHECK: Duration = Duration::from_millis(250);

/// How long past its own deadline a ticket keeps waiting for the reclaim it
/// follows to decide. That reclaim was scheduled synchronously with the same
/// edge and decides within 100 ms of a deadline at most this far away, so
/// it is never reached while the reclaim runs. It bounds the stream only if
/// the reclaim's task was dropped, for example by a shutdown, and 30 s
/// matches the default drain: never more than twice the window the owner
/// already accepts.
const FOLLOW_MARGIN: Duration = Duration::from_secs(30);

type Phase = watch::Receiver<Option<crate::yield_control::ReclaimPhase>>;

impl Drop for Inner {
    fn drop(&mut self) {
        if let Some(task) = &self.task {
            task.abort();
        }
    }
}

impl WorkCancellation {
    pub fn for_upstream(state: &Arc<AppState>, upstream: &UpstreamConfig) -> Self {
        // A resource-scoped upstream is not reclaimed by the owner veto, so
        // only pressure on its own resources cancels its work.
        let mut signals = Vec::new();
        if upstream.owner_scope == crate::config::OwnerScope::Host {
            signals.push((state.veto.subscribe(), None));
        }
        for id in std::iter::once(&upstream.resource).chain(upstream.also_requires.iter()) {
            signals.push((state.resources[id].reclaims.subscribe(), Some(id.clone())));
        }
        let phase = state.upstreams[&upstream.id].reclaim_phase.subscribe();
        let (state, owned) = (state.clone(), upstream.clone());
        let cleared: Cleared =
            Box::new(move |refused| crate::yield_control::trigger_cleared(&state, &owned, refused));
        Self::from_signals(signals, upstream, Some((cleared, phase)))
    }

    /// A ticket already cancelled by `trigger`.
    #[cfg(test)]
    pub fn cancelled_by(trigger: Trigger) -> Self {
        Self(Arc::new(Inner {
            cancelled: watch::channel(Some(trigger)).1,
            task: None,
        }))
    }

    #[cfg(test)]
    pub fn new(veto: &crate::state::Veto, upstream: &UpstreamConfig) -> Self {
        Self::from_signals(vec![(veto.subscribe(), None)], upstream, None)
    }

    /// Without `stand_down`, a drain always runs to its deadline.
    fn from_signals(
        mut signals: Vec<Signal>,
        upstream: &UpstreamConfig,
        mut stand_down: Option<(Cleared, Phase)>,
    ) -> Self {
        let (send, receive) = watch::channel(None);
        let mode = upstream.on_owner_return;
        let drain = Duration::from_secs(u64::from(upstream.drain_timeout_s));
        let task = (mode != ReclaimMode::Finish).then(|| {
            tokio::spawn(async move {
                let mut trigger: Trigger;
                loop {
                    // A lagged receiver cannot establish the original deadline;
                    // cancel conservatively instead of granting a fresh interval.
                    let (first, index, _) = {
                        let futures = signals
                            .iter_mut()
                            .map(|(signal, _)| Box::pin(signal.recv()));
                        futures_util::future::select_all(futures).await
                    };
                    let mut owner = signals[index].1.is_none();
                    let mut refused: Vec<String> = signals[index].1.iter().cloned().collect();
                    let mut edges: Vec<tokio::time::Instant> = first.iter().copied().collect();
                    let first = first.ok().and_then(|mut earliest| {
                        // A delayed task may see owner and resource edges together.
                        // Queue order across channels must not extend the deadline.
                        for (signal, resource) in &mut signals {
                            match signal.try_recv() {
                                Ok(at) => {
                                    earliest = earliest.min(at);
                                    edges.push(at);
                                    owner |= resource.is_none();
                                    refused.extend(resource.iter().cloned());
                                }
                                Err(broadcast::error::TryRecvError::Empty) => {}
                                Err(_) => return None,
                            }
                        }
                        Some(earliest)
                    });
                    trigger = Trigger {
                        owner,
                        resources: refused.clone(),
                    };
                    let Some(returned_at) = first else {
                        break;
                    };
                    if mode != ReclaimMode::Drain {
                        break;
                    }
                    let deadline = returned_at + drain;
                    let refused: Vec<&str> = refused.iter().map(String::as_str).collect();
                    let stood_down = loop {
                        let Some((cleared, phase)) = stand_down.as_mut() else {
                            tokio::time::sleep_until(deadline).await;
                            break false;
                        };
                        // A reclaim that answers this ticket's own edge decides
                        // for both.
                        let followed = phase
                            .borrow()
                            .as_ref()
                            .filter(|reclaim| reclaim.edges.iter().any(|at| edges.contains(at)))
                            .map(|reclaim| reclaim.decision);
                        match followed {
                            Some(ReclaimDecision::StoodDown) => break true,
                            Some(ReclaimDecision::Proceeding) => break false,
                            Some(ReclaimDecision::Waiting) => {
                                let backstop = deadline + FOLLOW_MARGIN;
                                if tokio::time::Instant::now() >= backstop {
                                    break false;
                                }
                                let changed = tokio::time::timeout_at(
                                    (tokio::time::Instant::now() + TRIGGER_CHECK).min(backstop),
                                    phase.changed(),
                                )
                                .await;
                                if matches!(changed, Ok(Err(_))) {
                                    tokio::time::sleep(TRIGGER_CHECK).await;
                                }
                                continue;
                            }
                            None => {}
                        }
                        if tokio::time::Instant::now() >= deadline {
                            break false;
                        }
                        if cleared(&refused) {
                            break true;
                        }
                        tokio::time::sleep_until(
                            (tokio::time::Instant::now() + TRIGGER_CHECK).min(deadline),
                        )
                        .await;
                    };
                    if !stood_down {
                        break;
                    }
                }
                send.send_replace(Some(trigger));
            })
        });
        Self(Arc::new(Inner {
            cancelled: receive,
            task,
        }))
    }

    pub async fn cancelled(&self) {
        if self.0.task.is_none() {
            std::future::pending::<()>().await;
        }
        let mut receive = self.0.cancelled.clone();
        while receive.borrow_and_update().is_none() {
            if receive.changed().await.is_err() {
                return;
            }
        }
    }
}

/// Whether the deadline has already passed, without waiting for it.
pub fn is_cancelled(ticket: &Option<WorkCancellation>) -> bool {
    ticket
        .as_ref()
        .is_some_and(|ticket| ticket.0.task.is_some() && ticket.0.cancelled.borrow().is_some())
}

/// What cancelled the ticket, once it has been cancelled.
pub fn trigger(ticket: &Option<WorkCancellation>) -> Option<Trigger> {
    ticket
        .as_ref()
        .and_then(|ticket| ticket.0.cancelled.borrow().clone())
}

pub async fn cancelled(ticket: &Option<WorkCancellation>) {
    match ticket {
        Some(ticket) => ticket.cancelled().await,
        None => std::future::pending().await,
    }
}

/// Response-body ownership of a forwarding task. A pass-through reply is
/// aborted when the borrower disconnects; a reply that ends a reservation is
/// left to read to its own end (the task itself stays bounded).
pub struct ForwardTask {
    task: JoinHandle<()>,
    outlives_borrower: bool,
}

impl ForwardTask {
    pub fn new(outlives_borrower: bool, task: JoinHandle<()>) -> Self {
        Self {
            task,
            outlives_borrower,
        }
    }
}

impl Drop for ForwardTask {
    fn drop(&mut self) {
        if !self.outlives_borrower {
            self.task.abort();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::state::Veto;

    fn config(mode: &str, seconds: u32) -> UpstreamConfig {
        toml::from_str(&format!(
            r#"
            id = "test"
            listen = "127.0.0.1:11435"
            target = "http://127.0.0.1:11434"
            kind = "ollama"
            resource = "mem"
            on_owner_return = "{mode}"
            drain_timeout_s = {seconds}
        "#
        ))
        .unwrap()
    }

    fn resource_state(mode: &str, seconds: u32) -> Arc<AppState> {
        scoped_resource_state(mode, seconds, crate::config::OwnerScope::Host)
    }

    fn scoped_resource_state(
        mode: &str,
        seconds: u32,
        scope: crate::config::OwnerScope,
    ) -> Arc<AppState> {
        let mut cfg: crate::config::Config = toml::from_str(
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
            [[resource]]
            id = "other"
            kind = "system-memory"
        "#,
        )
        .unwrap();
        let mut affected = config(mode, seconds);
        affected.also_requires.push("cpu".into());
        affected.owner_scope = scope;
        let mut unrelated = config("cancel", 0);
        unrelated.id = "other-work".into();
        unrelated.resource = "other".into();
        unrelated.listen = "127.0.0.1:11436".parse().unwrap();
        cfg.upstreams = vec![affected, unrelated];
        Arc::new(AppState::new(cfg).unwrap())
    }

    #[tokio::test(start_paused = true)]
    async fn pressure_uses_configured_policy_and_only_affected_dependencies() {
        for mode in ["cancel", "drain", "finish"] {
            let state = resource_state(mode, 7);
            let affected = WorkCancellation::for_upstream(&state, &state.config.upstreams[0]);
            let unrelated = WorkCancellation::for_upstream(&state, &state.config.upstreams[1]);
            // Cancellation applies even when automatic model unloading is off.
            assert!(!state.config.upstreams[0].yield_on_veto);
            crate::yield_control::on_resources_refused(
                state.clone(),
                vec![crate::reclaim_log::RefusedResource::unknown(
                    "cpu", "ready", "test",
                )],
                false,
            )
            .await;
            if mode == "finish" {
                assert!(
                    tokio::time::timeout(Duration::from_secs(86400), affected.cancelled())
                        .await
                        .is_err()
                );
            } else {
                if mode == "drain" {
                    assert!(
                        tokio::time::timeout(Duration::from_secs(6), affected.cancelled())
                            .await
                            .is_err()
                    );
                    // Repeated edges do not restart the original seven seconds.
                    crate::yield_control::on_resources_refused(
                        state.clone(),
                        vec![crate::reclaim_log::RefusedResource::unknown(
                            "cpu", "ready", "test",
                        )],
                        false,
                    )
                    .await;
                }
                // Allow the deadline task to publish after its timer fires;
                // an assertion timer at the same instant has no ordering guarantee.
                tokio::time::timeout(Duration::from_secs(2), affected.cancelled())
                    .await
                    .unwrap();
            }
            assert!(
                tokio::time::timeout(Duration::from_secs(1), unrelated.cancelled())
                    .await
                    .is_err()
            );
        }
    }

    #[tokio::test(start_paused = true)]
    async fn resource_scoped_work_ignores_the_owner_but_not_its_resources() {
        let state = scoped_resource_state("cancel", 0, crate::config::OwnerScope::Resource);
        let ticket = WorkCancellation::for_upstream(&state, &state.config.upstreams[0]);
        state.veto.observe(Some("owner".into()), 0);
        crate::yield_control::on_veto_engaged(state.clone()).await;
        assert!(
            tokio::time::timeout(Duration::from_secs(1), ticket.cancelled())
                .await
                .is_err()
        );
        // Pressure on its own resource still reclaims it while the owner is
        // present, when host-scoped pressure yields are suppressed.
        crate::yield_control::on_resources_refused(
            state.clone(),
            vec![crate::reclaim_log::RefusedResource::unknown(
                "cpu", "ready", "test",
            )],
            true,
        )
        .await;
        tokio::time::timeout(Duration::from_secs(2), ticket.cancelled())
            .await
            .unwrap();
    }

    #[tokio::test(start_paused = true)]
    async fn a_cancelled_ticket_records_whether_the_owner_or_pressure_ended_it() {
        let state = resource_state("cancel", 0);
        let upstream = &state.config.upstreams[0];
        let pressure = Some(WorkCancellation::for_upstream(&state, upstream));
        assert_eq!(trigger(&pressure), None);
        state.resources["cpu"]
            .reclaims
            .send(tokio::time::Instant::now())
            .unwrap();
        tokio::time::timeout(Duration::from_secs(1), cancelled(&pressure))
            .await
            .unwrap();
        assert_eq!(
            trigger(&pressure),
            Some(Trigger {
                owner: false,
                resources: vec!["cpu".into()],
            })
        );

        let owner = Some(WorkCancellation::for_upstream(&state, upstream));
        state.veto.observe(Some("owner".into()), 0);
        tokio::time::timeout(Duration::from_secs(1), cancelled(&owner))
            .await
            .unwrap();
        assert_eq!(
            trigger(&owner),
            Some(Trigger {
                owner: true,
                resources: Vec::new(),
            })
        );
    }

    #[tokio::test(start_paused = true)]
    async fn delayed_cross_channel_observation_uses_the_earliest_edge() {
        let state = resource_state("drain", 30);
        let ticket = WorkCancellation::for_upstream(&state, &state.config.upstreams[0]);
        let older = tokio::time::Instant::now() - Duration::from_secs(20);
        state.resources["cpu"].reclaims.send(older).unwrap();
        // Both are queued before the worker polls. The owner channel is first
        // in the select list, but its later timestamp must not win.
        state.veto.observe(Some("owner".into()), 0);
        assert!(
            tokio::time::timeout(Duration::from_secs(9), ticket.cancelled())
                .await
                .is_err()
        );
        tokio::time::timeout(Duration::from_secs(2), ticket.cancelled())
            .await
            .unwrap();
    }

    #[tokio::test(start_paused = true)]
    async fn unrelated_resource_overflow_cannot_cancel_work() {
        let state = resource_state("cancel", 0);
        let ticket = WorkCancellation::for_upstream(&state, &state.config.upstreams[0]);
        for _ in 0..100 {
            let _ = state.resources["other"]
                .reclaims
                .send(tokio::time::Instant::now());
        }
        assert!(
            tokio::time::timeout(Duration::from_secs(1), ticket.cancelled())
                .await
                .is_err()
        );
    }

    #[tokio::test(start_paused = true)]
    async fn drain_uses_first_return_deadline_even_when_worker_starts_late() {
        let veto = Veto::default();
        let ticket = WorkCancellation::new(&veto, &config("drain", 30));
        veto.observe(Some("owner".into()), 0);
        tokio::time::advance(Duration::from_secs(20)).await;
        veto.observe(None, 0);
        veto.observe(Some("returned again".into()), 0);
        assert!(
            tokio::time::timeout(Duration::from_secs(9), ticket.cancelled())
                .await
                .is_err()
        );
        tokio::time::advance(Duration::from_secs(1)).await;
        ticket.cancelled().await;
    }

    #[tokio::test(start_paused = true)]
    async fn immediate_zero_drain_and_unlimited_completion_are_distinct() {
        for mode in ["cancel", "drain"] {
            let veto = Veto::default();
            let ticket = WorkCancellation::new(&veto, &config(mode, 0));
            assert!(
                tokio::time::timeout(Duration::from_secs(1), ticket.cancelled())
                    .await
                    .is_err()
            );
            veto.observe(Some("owner".into()), 0);
            ticket.cancelled().await;
        }
        let veto = Veto::default();
        let ticket = WorkCancellation::new(&veto, &config("finish", 1));
        veto.observe(Some("owner".into()), 0);
        assert!(
            tokio::time::timeout(Duration::from_secs(86400), ticket.cancelled())
                .await
                .is_err()
        );
    }

    #[tokio::test(start_paused = true)]
    async fn event_overflow_cancels_instead_of_extending_the_deadline() {
        let veto = Veto::default();
        let ticket = WorkCancellation::new(&veto, &config("drain", 30));
        for _ in 0..100 {
            veto.observe(Some("owner".into()), 0);
            veto.observe(None, 0);
        }
        tokio::time::timeout(Duration::from_secs(1), ticket.cancelled())
            .await
            .unwrap();
    }

    /// A resource-scoped upstream on a pressure-gated GPU, with a clear
    /// reading.
    fn gpu_state(mode: &str, seconds: u32) -> Arc<AppState> {
        let mut cfg: crate::config::Config = toml::from_str(
            r#"
            [server]
            listen = "127.0.0.1:0"
            [[resource]]
            id = "gpu0"
            kind = "nvidia-gpu"
            selector = "GPU-aaaa"
        "#,
        )
        .unwrap();
        let mut upstream = config(mode, seconds);
        upstream.resource = "gpu0".into();
        upstream.owner_scope = crate::config::OwnerScope::Resource;
        cfg.upstreams = vec![upstream];
        let state = Arc::new(AppState::new(cfg).unwrap());
        state.mark_presence_sampled();
        clear(&state);
        state
    }

    fn clear(state: &AppState) {
        state.resources["gpu0"]
            .pressure
            .write()
            .unwrap()
            .record_success(crate::plugin::pressure::PressureVerdict {
                non_lender_pct: 3.0,
                engaged: false,
                threshold_pct: 45.0,
                baseline_pct: Some(2.0),
                target_pct: None,
                learned_samples: 0,
                learned_s: 0,
                mode: Default::default(),
                calibrated: true,
                calibration_samples: 60,
                calibration_required: 60,
                calibration_hint: None,
                above_for_s: 0,
            });
    }

    /// Refuse gpu0 as unknown and raise its edge, returning the edge's time.
    fn refuse(state: &AppState) -> tokio::time::Instant {
        state.resources["gpu0"]
            .pressure
            .write()
            .unwrap()
            .record_failure("GPU attribution sample failed");
        let at = tokio::time::Instant::now();
        state.resources["gpu0"].reclaims.send(at).unwrap();
        at
    }

    #[tokio::test(start_paused = true)]
    async fn a_drain_stands_down_when_its_pressure_reading_recovers() {
        let state = gpu_state("drain", 30);
        let ticket = WorkCancellation::for_upstream(&state, &state.config.upstreams[0]);
        refuse(&state);
        tokio::time::sleep(Duration::from_secs(8)).await;
        clear(&state);
        assert!(
            tokio::time::timeout(Duration::from_secs(60), ticket.cancelled())
                .await
                .is_err()
        );
        // A later edge is a drain of its own, and one whose trigger holds
        // still cancels at its deadline.
        refuse(&state);
        assert!(
            tokio::time::timeout(Duration::from_secs(29), ticket.cancelled())
                .await
                .is_err()
        );
        tokio::time::timeout(Duration::from_secs(2), ticket.cancelled())
            .await
            .unwrap();
    }

    /// When a reclaim answers the ticket's own edge, the ticket does what
    /// that reclaim decides, not what its own clock or check says.
    #[tokio::test(start_paused = true)]
    async fn a_ticket_follows_the_reclaim_that_answers_its_edge() {
        use crate::yield_control::{ReclaimDecision, ReclaimPhase};
        let publish = |state: &AppState, edge, decision| {
            state.upstreams["test"]
                .reclaim_phase
                .send_replace(Some(ReclaimPhase {
                    reclaim: 1,
                    edges: vec![edge],
                    decision,
                }));
        };
        // Still waiting past the ticket's own deadline: not cut until the
        // reclaim proceeds.
        let state = gpu_state("drain", 30);
        let ticket = WorkCancellation::for_upstream(&state, &state.config.upstreams[0]);
        let edge = refuse(&state);
        publish(&state, edge, ReclaimDecision::Waiting);
        assert!(
            tokio::time::timeout(Duration::from_secs(40), ticket.cancelled())
                .await
                .is_err()
        );
        publish(&state, edge, ReclaimDecision::Proceeding);
        tokio::time::timeout(Duration::from_secs(1), ticket.cancelled())
            .await
            .unwrap();

        // A reclaim that never decides, as if its task were dropped: cut
        // at the ticket's deadline plus the margin.
        let state = gpu_state("drain", 30);
        let ticket = WorkCancellation::for_upstream(&state, &state.config.upstreams[0]);
        let edge = refuse(&state);
        publish(&state, edge, ReclaimDecision::Waiting);
        assert!(
            tokio::time::timeout(Duration::from_secs(59), ticket.cancelled())
                .await
                .is_err()
        );
        tokio::time::timeout(Duration::from_secs(2), ticket.cancelled())
            .await
            .unwrap();

        // Stood down while its own check would still refuse: not cut.
        let state = gpu_state("drain", 30);
        let ticket = WorkCancellation::for_upstream(&state, &state.config.upstreams[0]);
        let edge = refuse(&state);
        publish(&state, edge, ReclaimDecision::StoodDown);
        assert!(
            tokio::time::timeout(Duration::from_secs(90), ticket.cancelled())
                .await
                .is_err()
        );

        // A decision for another edge is not this ticket's: its own check
        // still refuses, so it is cut at its own deadline.
        let state = gpu_state("drain", 30);
        let ticket = WorkCancellation::for_upstream(&state, &state.config.upstreams[0]);
        let other = tokio::time::Instant::now();
        tokio::time::advance(Duration::from_secs(1)).await;
        refuse(&state);
        publish(&state, other, ReclaimDecision::StoodDown);
        assert!(
            tokio::time::timeout(Duration::from_secs(29), ticket.cancelled())
                .await
                .is_err()
        );
        tokio::time::timeout(Duration::from_secs(2), ticket.cancelled())
            .await
            .unwrap();
    }

    #[tokio::test(start_paused = true)]
    async fn a_drain_stands_down_when_the_owner_leaves() {
        let state = resource_state("drain", 30);
        state.mark_presence_sampled();
        let ticket = WorkCancellation::for_upstream(&state, &state.config.upstreams[0]);
        state.veto.observe(Some("owner".into()), 0);
        tokio::time::sleep(Duration::from_secs(10)).await;
        state.veto.observe(None, 0);
        assert!(
            tokio::time::timeout(Duration::from_secs(60), ticket.cancelled())
                .await
                .is_err()
        );
    }

    #[test]
    fn default_and_invalid_policies() {
        let base = "id='u'\nlisten='127.0.0.1:1'\ntarget='http://127.0.0.1:2'\nkind='ollama'\nresource='r'\n";
        let upstream: UpstreamConfig = toml::from_str(base).unwrap();
        assert_eq!(upstream.on_owner_return, ReclaimMode::Drain);
        assert_eq!(upstream.drain_timeout_s, 30);
        for invalid in [
            "on_owner_return='forever'",
            "drain_timeout_s=-1",
            "drain_timeout_s=0.5",
        ] {
            assert!(toml::from_str::<UpstreamConfig>(&format!("{base}{invalid}")).is_err());
        }
    }
}
