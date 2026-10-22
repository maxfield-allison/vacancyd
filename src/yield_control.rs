//! Yield-on-veto: when the owner returns, hand the hardware back rather
//! than letting the OS page it out.
//!
//! Why active release matters: under memory pressure Windows demotes a
//! resident model to system RAM rather than crashing it — nothing dies, but
//! the paging traffic lands on the owner's frame times. Releasing before the
//! owner's workload asks for the memory is the polite move, and it is the
//! whole point of the daemon.
//!
//! What "release" means is the adapter's business, not this module's:
//! Ollama unloads with `keep_alive: 0`, ComfyUI with `/free`, and a plain
//! OpenAI server cannot, so it reports `can_unload: false` and is skipped.
//!
//! Queued work outlives its proxy connection, so closing the connection does
//! not stop it. Adapters with `can_cancel` stop the borrowed jobs they track
//! (ComfyUI deletes queued prompts and interrupts a running one) at the
//! owner-return deadline, before the unload. Only work this daemon admitted
//! before the event is cancelled; the owner's own jobs are never named.
//!
//! A drain or finish wait stands down if its trigger clears first, and an
//! upstream that shows work this daemon did not admit is not unloaded,
//! because an unload would reach the owner's work too.

use std::collections::HashSet;
use std::sync::Arc;
use std::time::Duration;

use futures_util::future::join_all;
use tracing::{info, warn};

use crate::config::{OwnerScope, ReclaimMode, UpstreamConfig};
use crate::reclaim_log::{ReclaimCause, RefusedResource, Trigger, UpstreamReclaim};
use crate::state::AppState;

/// Bound on one unload request. The same in tests: a 50 ms test-only value
/// once abandoned healthy unloads whenever the host was busy. The test
/// that needs a hung unload to time out runs on a paused clock instead.
const YIELD_TIMEOUT: Duration = Duration::from_secs(10);

/// How long an interrupted job has to show terminal completion evidence
/// before the unload goes ahead without it. This bounds when a new check may
/// start, not how long a check may take: the upstream decides how quickly the
/// job stops, while a check's duration follows the size of what it reads.
/// A job that stops later is still released by its tracker.
#[cfg(not(test))]
const INTERRUPT_CONFIRM: Duration = Duration::from_secs(10);
#[cfg(test)]
const INTERRUPT_CONFIRM: Duration = Duration::from_secs(2);

/// Backstop on one adapter cancellation. Each request and read inside it has
/// its own bound, which follows the work it waits on (for ComfyUI, the size of
/// the owner's queue); this only catches an adapter that ignores its own
/// bounds. It is well above their sum for ComfyUI: two streamed queue reads
/// of up to 5 minutes and two state requests of up to 2 minutes. A fixed short
/// budget here once cut off a healthy cancellation on a busy host.
const CANCEL_TIMEOUT: Duration = Duration::from_secs(20 * 60);

/// Backstop on asking an upstream whether it holds work vacancyd did not
/// admit. The adapter bounds its own read well inside this; on expiry the
/// unload goes ahead as it would without the check.
const FOREIGN_CHECK_TIMEOUT: Duration = Duration::from_secs(2 * 60);

/// What the latest reclaim of an upstream decided about the borrowed work
/// it waited on.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ReclaimDecision {
    /// Still inside its drain or finish window.
    Waiting,
    /// Its trigger cleared first: nothing is cancelled.
    StoodDown,
    /// The window ended or the work did: cancellation goes ahead.
    Proceeding,
}

/// One reclaim's decision, published on [`crate::state::UpstreamRuntime::reclaim_phase`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ReclaimPhase {
    /// The reclaim's id in the reclaim log.
    pub reclaim: u64,
    /// The edges this reclaim answers: the one that scheduled it, then any
    /// that coalesced into it, as their timestamps were published. A proxy
    /// ticket follows the reclaim only if it answers one of the ticket's own
    /// edges.
    pub edges: Vec<tokio::time::Instant>,
    pub decision: ReclaimDecision,
}

/// Coalesced edges kept per reclaim. A ticket whose edge is not among them
/// applies its own check instead, which is still safe.
const MAX_PHASE_EDGES: usize = 32;

/// Scheduling acquires the hold synchronously, before another observation can
/// clear the veto and before the runtime gets around to polling this future.
/// Only host-scoped upstreams answer to the owner; a resource-scoped one is
/// reclaimed by pressure on its own resources.
pub fn on_veto_engaged(state: Arc<AppState>) -> impl std::future::Future<Output = ()> + Send {
    let upstreams = state
        .config
        .upstreams
        .iter()
        .filter(|u| u.owner_scope == OwnerScope::Host)
        .cloned()
        .collect();
    let cause = ReclaimCause {
        trigger: Trigger::OwnerVeto,
        veto_cause: state.veto.status().1,
        resources: Vec::new(),
    };
    let edge = state
        .veto
        .returned_at()
        .unwrap_or_else(tokio::time::Instant::now);
    schedule(state, upstreams, cause, edge)
}

/// While the owner veto is active, host-scoped upstreams are already yielded
/// or refusing, so pass `resource_scoped_only` to reclaim only the upstreams
/// the veto does not cover.
pub fn on_resources_refused(
    state: Arc<AppState>,
    refusals: Vec<RefusedResource>,
    resource_scoped_only: bool,
) -> impl std::future::Future<Output = ()> + Send {
    let refused: HashSet<&str> = refusals.iter().map(|r| r.resource.as_str()).collect();
    let at = tokio::time::Instant::now();
    for id in &refused {
        if let Some(resource) = state.resources.get(*id) {
            let _ = resource.reclaims.send(at);
        }
    }
    let upstreams = state
        .config
        .upstreams
        .iter()
        .filter(|u| upstream_depends_on_any(u, &refused))
        .filter(|u| !resource_scoped_only || u.owner_scope == OwnerScope::Resource)
        .cloned()
        .collect();
    let cause = ReclaimCause {
        trigger: Trigger::ResourceRefused,
        veto_cause: None,
        resources: refusals,
    };
    schedule(state, upstreams, cause, at)
}

/// One reclaim's id in the reclaim log and its cause, carried to every line
/// it logs.
struct Context {
    id: u64,
    cause: ReclaimCause,
}

/// One upstream's response to an owner-return or resource-pressure event.
struct Reclaim {
    upstream: UpstreamConfig,
    /// Ledger entries the upstream owned when the event arrived.
    entries: Vec<u64>,
}

/// `edge` is the timestamp the triggering edge was published with.
fn schedule(
    state: Arc<AppState>,
    upstreams: Vec<UpstreamConfig>,
    cause: ReclaimCause,
    edge: tokio::time::Instant,
) -> impl std::future::Future<Output = ()> + Send {
    let scheduled = tokio::time::Instant::now();
    let reclaims: Vec<Reclaim> = upstreams
        .into_iter()
        .filter_map(|upstream| {
            let mut ledger = state.admission.lock().unwrap();
            let entries = ledger.entries_of(&upstream.id);
            let began = if upstream.yield_on_veto {
                // begin_yield coalesces repeated edges to one waiter/upstream.
                ledger.begin_yield(&upstream)
            } else {
                // Cancellation, like closing proxied connections, does not
                // depend on automatic unloading.
                let cancels = upstream.on_owner_return != ReclaimMode::Finish
                    && !entries.is_empty()
                    && state
                        .upstreams
                        .get(&upstream.id)
                        .is_some_and(|rt| rt.adapter.capabilities().can_cancel);
                if !cancels {
                    return None;
                }
                ledger.begin_reclaim(&upstream)
            };
            if !began {
                // The reclaim under way answers this edge too.
                if let Some(runtime) = state.upstreams.get(&upstream.id) {
                    runtime.reclaim_phase.send_if_modified(|phase| match phase {
                        Some(phase) if phase.edges.len() < MAX_PHASE_EDGES => {
                            phase.edges.push(edge);
                            true
                        }
                        _ => false,
                    });
                }
                return None;
            }
            Some(Reclaim { upstream, entries })
        })
        .collect();
    let context = Arc::new(record(&state, cause, &reclaims));
    // Published before anything awaits, so a proxy ticket woken by the same
    // edge already finds the reclaim it is to follow.
    for reclaim in &reclaims {
        if let Some(runtime) = state.upstreams.get(&reclaim.upstream.id) {
            runtime.reclaim_phase.send_replace(Some(ReclaimPhase {
                reclaim: context.id,
                edges: vec![edge],
                decision: ReclaimDecision::Waiting,
            }));
        }
    }
    async move {
        join_all(reclaims.iter().map(|reclaim| async {
            let upstream = &reclaim.upstream;
            let waited = wait_for_work(&state, upstream, scheduled, &context).await;
            decide(
                &state,
                upstream,
                context.id,
                match waited {
                    Waited::StoodDown => ReclaimDecision::StoodDown,
                    Waited::Done => ReclaimDecision::Proceeding,
                },
            );
            if waited == Waited::StoodDown {
                let elapsed_ms = scheduled.elapsed().as_millis() as u64;
                info!(
                    reclaim = context.id,
                    upstream = upstream.id,
                    trigger = context.cause.trigger.as_str(),
                    elapsed_ms,
                    "reclaim abandoned; its trigger cleared"
                );
                state.reclaim_log.lock().unwrap().note_abandoned(
                    context.id,
                    &upstream.id,
                    elapsed_ms,
                );
                return;
            }
            if upstream.on_owner_return != ReclaimMode::Finish {
                cancel_borrowed(&state, upstream, &reclaim.entries, &context).await;
            }
            report_held(&state, upstream, &reclaim.entries);
            if upstream.yield_on_veto {
                unload(&state, upstream, &context).await;
                state.admission.lock().unwrap().end_yield(upstream);
            } else {
                state.admission.lock().unwrap().end_reclaim(upstream);
            }
        }))
        .await;
    }
}

/// Log a reclaim that is starting and keep it in the reclaim log. Logged even
/// when no upstream has anything to reclaim, since in-flight requests on the
/// refused resources are still cut off.
fn record(state: &AppState, cause: ReclaimCause, reclaims: &[Reclaim]) -> Context {
    let upstreams: Vec<UpstreamReclaim> = reclaims
        .iter()
        .map(|reclaim| UpstreamReclaim {
            upstream: reclaim.upstream.id.clone(),
            entries: reclaim.entries.clone(),
            on_owner_return: match reclaim.upstream.on_owner_return {
                ReclaimMode::Cancel => "cancel",
                ReclaimMode::Drain => "drain",
                ReclaimMode::Finish => "finish",
            },
            unload: reclaim.upstream.yield_on_veto,
            released: Vec::new(),
            abandoned_after_ms: None,
            unload_skipped_for_foreign_work: false,
        })
        .collect();
    let names: Vec<&str> = upstreams.iter().map(|u| u.upstream.as_str()).collect();
    let entries: Vec<u64> = upstreams.iter().flat_map(|u| u.entries.clone()).collect();
    let id = state
        .reclaim_log
        .lock()
        .unwrap()
        .record(&cause, upstreams.clone());
    info!(
        reclaim = id,
        trigger = cause.trigger.as_str(),
        veto_cause = cause.veto_cause.as_deref(),
        refused = cause.refused(),
        upstreams = ?names,
        entries = ?entries,
        "reclaiming borrowed work"
    );
    Context { id, cause }
}

/// Publish what reclaim `id` decided, unless a later reclaim of the same
/// upstream has already replaced it.
fn decide(state: &AppState, upstream: &UpstreamConfig, id: u64, decision: ReclaimDecision) {
    if let Some(runtime) = state.upstreams.get(&upstream.id) {
        runtime.reclaim_phase.send_if_modified(|phase| match phase {
            Some(phase) if phase.reclaim == id => {
                phase.decision = decision;
                true
            }
            _ => false,
        });
    }
}

/// How a reclaim's wait for borrowed work ended.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Waited {
    /// The work ended, or the window closed: the reclaim goes on.
    Done,
    /// The trigger cleared first. The hold on the upstream has already been
    /// ended, and nothing is to be cancelled or unloaded.
    StoodDown,
}

/// Let accepted work end on its own inside the owner's policy window: none in
/// cancel mode, `drain_timeout_s` in drain mode. Finish mode has no
/// owner-return deadline, so it waits up to the upstream's
/// `completion_timeout_s`, the bound already placed on tracking a job. Held
/// work never extends the wait, because nothing is finishing it.
///
/// While it waits, the reclaim stands down if its trigger clears (see
/// [`trigger_cleared`]): a pressure reading that recovered or an owner who
/// left again is no reason to stop work that could still finish.
async fn wait_for_work(
    state: &AppState,
    upstream: &UpstreamConfig,
    scheduled: tokio::time::Instant,
    context: &Context,
) -> Waited {
    let window = match upstream.on_owner_return {
        ReclaimMode::Cancel => return Waited::Done,
        ReclaimMode::Drain => upstream.drain_timeout_s,
        ReclaimMode::Finish => upstream.completion_timeout_s,
    };
    let deadline = scheduled + Duration::from_secs(u64::from(window));
    let needs: Vec<&str> = std::iter::once(&upstream.resource)
        .chain(&upstream.also_requires)
        .map(String::as_str)
        .collect();
    let refused: Vec<&str> = context
        .cause
        .resources
        .iter()
        .map(|refusal| refusal.resource.as_str())
        .filter(|id| needs.contains(id))
        .collect();
    loop {
        {
            let mut ledger = state.admission.lock().unwrap();
            if !ledger.has_live_work_on(upstream) {
                return Waited::Done;
            }
            if tokio::time::Instant::now() >= deadline {
                if upstream.on_owner_return == ReclaimMode::Finish {
                    warn!(
                        upstream = upstream.id,
                        completion_timeout_s = upstream.completion_timeout_s,
                        "borrowed work is still active at its completion bound; continuing without it"
                    );
                }
                return Waited::Done;
            }
            // Checked and released under the admission lock, which a new
            // edge's `begin_yield` also takes: an edge raised after this
            // check finds the upstream free and schedules a reclaim of its
            // own, rather than coalescing into one that is standing down.
            if trigger_cleared(state, upstream, &refused) {
                if upstream.yield_on_veto {
                    ledger.end_yield(upstream);
                } else {
                    ledger.end_reclaim(upstream);
                }
                return Waited::StoodDown;
            }
        }
        tokio::time::sleep_until(
            (tokio::time::Instant::now() + Duration::from_millis(100)).min(deadline),
        )
        .await;
    }
}

/// Whether nothing would reclaim `upstream` now, judged by admission's own
/// checks: the owner's presence would not refuse it (presence sampled, and
/// for a host-scoped upstream, complete session evidence and no owner veto),
/// and no resource it needs is refused by GPU pressure. Each resource in
/// `refused` must also be pressure-gated, so that a reading exists that
/// could clear, and quiet: below its threshold and not on its way to
/// engaging (`above_for_s` of zero). "Not engaged" alone is not enough after
/// an unknown reading, since the owner's load may have been building while
/// nothing could be measured.
///
/// Checking every dependency, not only the refused ones, covers a second
/// trigger that coalesced into this reclaim while it waited, and pressure
/// that engaged while the veto kept a host-scoped upstream out of
/// resource reclaims.
pub(crate) fn trigger_cleared(
    state: &AppState,
    upstream: &UpstreamConfig,
    refused: &[&str],
) -> bool {
    let snapshot = crate::gate::Snapshot::read(state);
    if crate::gate::owner_refusal(state, &snapshot, upstream).is_some() {
        return false;
    }
    let resource = |id: &str| state.config.resources.iter().find(|rc| rc.id == id);
    let admits =
        |id: &str| resource(id).is_none_or(|rc| crate::gate::pressure_refusal(state, rc).is_none());
    let quiet = |id: &str| {
        state
            .pressure_view(id)
            .and_then(|view| view.value)
            .is_some_and(|v| v.non_lender_pct < v.threshold_pct && v.above_for_s == 0)
    };
    refused.iter().all(|id| {
        resource(id).is_some_and(|rc| crate::gate::pressure_gated(state, rc))
            && admits(id)
            && quiet(id)
    }) && std::iter::once(&upstream.resource)
        .chain(&upstream.also_requires)
        .all(|id| admits(id))
}

/// How long owner-return cancellation waits for borrowed submissions that
/// are still waiting for their reply, so that the jobs they queue are
/// cancelled before an unload. A submission that takes longer is cancelled by
/// its own proxy task when the reply arrives.
#[cfg(not(test))]
const SUBMISSION_SETTLE: Duration = Duration::from_secs(2);
#[cfg(test)]
const SUBMISSION_SETTLE: Duration = Duration::from_millis(500);

/// Stop the borrowed jobs this upstream still owns from before the event, and
/// release each reservation the upstream confirms stopped.
async fn cancel_borrowed(
    state: &AppState,
    upstream: &UpstreamConfig,
    entries: &[u64],
    context: &Context,
) {
    let Some(runtime) = state.upstreams.get(&upstream.id) else {
        return;
    };
    if !runtime.adapter.capabilities().can_cancel {
        return;
    }
    let settle = tokio::time::Instant::now() + SUBMISSION_SETTLE;
    while state.admission.lock().unwrap().submitting(entries)
        && tokio::time::Instant::now() < settle
    {
        tokio::time::sleep(Duration::from_millis(25)).await;
    }
    let claims = state.admission.lock().unwrap().claim_for_cancel(entries);
    cancel_claims(state, upstream, claims, Some(context)).await;
}

/// Cancel a borrowed submission that owner-return cancellation reached
/// while it was in flight, now that its reply has settled which job it is.
pub async fn cancel_submitted(state: &AppState, upstream: &UpstreamConfig, entry: u64) {
    let claims = state.admission.lock().unwrap().claim_for_cancel(&[entry]);
    if claims.is_empty() {
        return;
    }
    // The reclaim that reached this submission is the latest that named
    // its upstream.
    let context = state
        .reclaim_log
        .lock()
        .unwrap()
        .latest_for(&upstream.id)
        .map(|(id, cause)| Context { id, cause });
    info!(
        upstream = upstream.id,
        entry,
        reclaim = context.as_ref().map(|context| context.id),
        trigger = context
            .as_ref()
            .map(|context| context.cause.trigger.as_str()),
        "cancelling borrowed work whose submission was in flight when the owner returned"
    );
    cancel_claims(state, upstream, claims, context.as_ref()).await;
    report_held(state, upstream, &[entry]);
}

async fn cancel_claims(
    state: &AppState,
    upstream: &UpstreamConfig,
    claims: Vec<crate::admission::CancelClaim>,
    context: Option<&Context>,
) {
    let Some(runtime) = state.upstreams.get(&upstream.id) else {
        return;
    };
    if claims.is_empty() {
        return;
    }
    let ids: Vec<String> = claims.iter().map(|c| c.tracking_id.clone()).collect();
    let unacknowledged: Vec<String> = claims
        .iter()
        .filter(|c| !c.confirmed)
        .map(|c| c.tracking_id.clone())
        .collect();
    let entry_ids: Vec<u64> = claims.iter().map(|c| c.entry).collect();
    let outcome = match tokio::time::timeout(
        CANCEL_TIMEOUT,
        runtime.adapter.cancel(
            &runtime.observer_http,
            upstream.target.as_str(),
            &ids,
            &unacknowledged,
        ),
    )
    .await
    {
        Ok(Ok(outcome)) => outcome,
        Ok(Err(error)) => {
            warn!(
                upstream = upstream.id,
                entries = ?entry_ids,
                error = %format!("{error:#}"),
                "could not cancel borrowed work; its reservations stay until completion is verified or an administrator releases them"
            );
            return;
        }
        Err(_) => {
            warn!(
                upstream = upstream.id,
                entries = ?entry_ids,
                timeout_ms = CANCEL_TIMEOUT.as_millis(),
                "cancelling borrowed work timed out; its reservations stay until completion is verified or an administrator releases them"
            );
            return;
        }
    };
    let claim_of = |id: &str| claims.iter().find(|c| c.tracking_id == id);
    let entry_of = |id: &str| claim_of(id).map(|c| c.entry);
    for (id, reason) in &outcome.failed {
        warn!(
            upstream = upstream.id,
            entry = entry_of(id),
            reason,
            "could not cancel borrowed work; its reservation stays until completion is verified or an administrator releases it"
        );
    }
    for (id, reason) in &outcome.left_running {
        warn!(
            upstream = upstream.id,
            entry = entry_of(id),
            reason,
            "borrowed work is still running and was not interrupted; its reservation is released when its completion is observed"
        );
    }
    // Absence from the queue proves a job stopped only when the upstream
    // acknowledged submitting it: an unacknowledged id may never have been
    // queued, or may be queued yet. Its history can still show it ended.
    let mut stopped = Vec::new();
    let mut unacknowledged = Vec::new();
    for id in &outcome.stopped {
        match claim_of(id) {
            Some(claim) if claim.confirmed || outcome.found.contains(&claim.tracking_id) => {
                stopped.push(claim.entry)
            }
            Some(claim) => unacknowledged.push((claim.entry, id.as_str())),
            None => {}
        }
    }
    release(
        state,
        upstream,
        &stopped,
        "stopped before it ran or already ended",
        context,
    );
    let ended = unacknowledged.iter().map(|(entry, id)| async move {
        // The history read carries its own bounds.
        let check = runtime
            .adapter
            .completed(&runtime.observer_http, upstream.target.as_str(), id);
        if matches!(check.await, Ok(true)) {
            return Some(*entry);
        }
        warn!(
            upstream = upstream.id,
            entry,
            "borrowed work is not in the queue, but its submission was never acknowledged, so it may not have been queued yet; its reservation stays until its completion is observed or an administrator releases it"
        );
        None
    });
    let ended: Vec<u64> = join_all(ended).await.into_iter().flatten().collect();
    release(state, upstream, &ended, "already ended", context);
    let interrupted: Vec<(u64, &str)> = outcome
        .interrupted
        .iter()
        .filter_map(|id| Some((entry_of(id)?, id.as_str())))
        .collect();
    if interrupted.is_empty() {
        return;
    }
    // An interrupt is a request. Release only on the same terminal evidence
    // that ordinary completion requires.
    let confirm = interrupted.iter().map(|(entry, id)| async move {
        let deadline = tokio::time::Instant::now() + INTERRUPT_CONFIRM;
        loop {
            // A check under way is allowed to finish: the deadline limits
            // how long the job may take to stop, not how long it takes to
            // read the evidence that it did.
            let check = runtime
                .adapter
                .completed(&runtime.observer_http, upstream.target.as_str(), id);
            if matches!(check.await, Ok(true)) {
                return Some(*entry);
            }
            if tokio::time::Instant::now() >= deadline {
                warn!(
                    upstream = upstream.id,
                    entry,
                    "interrupted borrowed work has not confirmed completion; its reservation stays until it does or an administrator releases it"
                );
                return None;
            }
            tokio::time::sleep_until(
                (tokio::time::Instant::now() + Duration::from_millis(200)).min(deadline),
            )
            .await;
        }
    });
    let confirmed: Vec<u64> = join_all(confirm).await.into_iter().flatten().collect();
    release(state, upstream, &confirmed, "interrupted", context);
}

fn release(
    state: &AppState,
    upstream: &UpstreamConfig,
    entries: &[u64],
    how: &str,
    context: Option<&Context>,
) {
    if entries.is_empty() {
        return;
    }
    let mut ledger = state.admission.lock().unwrap();
    let released: Vec<u64> = entries
        .iter()
        .copied()
        .filter(|entry| ledger.finish_cancelled(*entry))
        .collect();
    ledger.sync_active_metrics(state);
    drop(ledger);
    if released.is_empty() {
        return;
    }
    // The message predates resource-pressure reclaim; `trigger` says which
    // one this was.
    info!(
        upstream = upstream.id,
        entries = ?released,
        how,
        reclaim = context.map(|context| context.id),
        trigger = context.map_or("unrecorded", |context| context.cause.trigger.as_str()),
        veto_cause = context.and_then(|context| context.cause.veto_cause.as_deref()),
        refused = context.map(|context| context.cause.refused()),
        "cancelled borrowed work on owner return"
    );
    if let Some(context) = context {
        state
            .reclaim_log
            .lock()
            .unwrap()
            .note_release(context.id, &upstream.id, &released, how);
    }
    state.capacity_wake.notify_one();
}

/// Say which held reservations outlive the reclaim and why, so an operator
/// knows what to verify before releasing them. They never delay the unload.
fn report_held(state: &AppState, upstream: &UpstreamConfig, entries: &[u64]) {
    let remaining = state.admission.lock().unwrap().remaining(entries);
    let can_cancel = state
        .upstreams
        .get(&upstream.id)
        .is_some_and(|rt| rt.adapter.capabilities().can_cancel);
    for entry in remaining.into_iter().filter(|entry| entry.held) {
        let reason = if !can_cancel {
            "this adapter cannot cancel upstream work, so completion is uncertain once its connection closed"
        } else if entry.tracking_id.is_none() {
            "completion is uncertain and no upstream job id is known to cancel"
        } else if upstream.on_owner_return == ReclaimMode::Finish {
            "finish mode does not cancel borrowed work, and its completion tracking has ended"
        } else {
            "cancellation was not confirmed"
        };
        warn!(
            upstream = upstream.id,
            entry = entry.id,
            reason,
            "borrowed work stays held after owner return; verify it at the upstream, then release it"
        );
    }
}

async fn unload(state: &AppState, upstream: &crate::config::UpstreamConfig, context: &Context) {
    let Some(runtime) = state.upstreams.get(&upstream.id) else {
        return;
    };
    if !runtime.adapter.capabilities().can_unload {
        return;
    }
    // An unload reaches every model the upstream holds, the owner's too.
    // When the upstream can say it is running or queueing work vacancyd did
    // not admit, leave its models alone; if that cannot be told, unload as
    // before, since the owner asked for the hardware back.
    if holds_foreign_work(state, upstream, runtime, context).await {
        info!(
            upstream = upstream.id,
            reclaim = context.id,
            "model unload skipped; the upstream holds work vacancyd did not admit"
        );
        state
            .reclaim_log
            .lock()
            .unwrap()
            .note_unload_skipped(context.id, &upstream.id);
        return;
    }
    let result = tokio::time::timeout(
        YIELD_TIMEOUT,
        runtime
            .adapter
            .unload_all(&runtime.observer_http, upstream.target.as_str()),
    )
    .await;
    match result {
        Ok(Ok(n)) if n > 0 => state
            .metrics
            .yields_total
            .with_label_values(&[&upstream.id])
            .inc_by(n as u64),
        Ok(Ok(_)) => {}
        Ok(Err(error)) => warn!(upstream = upstream.id, %error, "yield failed"),
        Err(_) => warn!(
            upstream = upstream.id,
            timeout_ms = YIELD_TIMEOUT.as_millis(),
            "yield timed out"
        ),
    }
}

/// Whether the server behind `upstream` holds work vacancyd did not admit.
/// vacancyd's own jobs are every ledger entry admitted through any upstream
/// that reaches the same server. `false`, with the reason logged, when that
/// cannot be told: a job of vacancyd's that cannot be named yet, an adapter
/// that cannot list the server's work, or a failed read.
async fn holds_foreign_work(
    state: &AppState,
    upstream: &UpstreamConfig,
    runtime: &crate::state::UpstreamRuntime,
    context: &Context,
) -> bool {
    let server = crate::config::UpstreamServer::of(upstream.target.as_str());
    let sharing: Vec<&str> = state
        .config
        .upstreams
        .iter()
        .filter(|other| {
            other.id == upstream.id
                || (server.is_some()
                    && crate::config::UpstreamServer::of(other.target.as_str()) == server)
        })
        .map(|other| other.id.as_str())
        .collect();
    let Some(ours) = state.admission.lock().unwrap().job_ids_on(&sharing) else {
        info!(
            upstream = upstream.id,
            reclaim = context.id,
            "a submission to this server is still unconfirmed, so vacancyd's work cannot be told from the owner's; unloading"
        );
        return false;
    };
    let check = tokio::time::timeout(
        FOREIGN_CHECK_TIMEOUT,
        runtime
            .adapter
            .holds_foreign_work(&runtime.observer_http, upstream.target.as_str(), &ours),
    )
    .await;
    match check {
        Ok(Ok(held)) => held.unwrap_or(false),
        Ok(Err(error)) => {
            warn!(
                upstream = upstream.id,
                reclaim = context.id,
                error = %format!("{error:#}"),
                "could not check the upstream for work vacancyd did not admit; unloading anyway"
            );
            false
        }
        Err(_) => {
            warn!(
                upstream = upstream.id,
                reclaim = context.id,
                timeout_ms = FOREIGN_CHECK_TIMEOUT.as_millis(),
                "checking the upstream for work vacancyd did not admit timed out; unloading anyway"
            );
            false
        }
    }
}

fn upstream_depends_on_any(
    upstream: &crate::config::UpstreamConfig,
    resources: &HashSet<&str>,
) -> bool {
    resources.contains(upstream.resource.as_str())
        || upstream
            .also_requires
            .iter()
            .any(|id| resources.contains(id.as_str()))
}

#[cfg(test)]
mod tests {
    use super::*;
    use futures_util::future::BoxFuture;
    use std::sync::atomic::{AtomicUsize, Ordering};

    use crate::plugin::adapter::{Adapter, AdapterCapabilities, RequestClass};

    struct HungAdapter;

    struct CountingAdapter(Arc<AtomicUsize>);

    impl Adapter for CountingAdapter {
        fn kind(&self) -> &'static str {
            "counting"
        }
        fn capabilities(&self) -> AdapterCapabilities {
            AdapterCapabilities {
                can_unload: true,
                reports_model: false,
                can_cancel: false,
            }
        }
        fn classify(&self, _: &http::Method, _: &str) -> RequestClass {
            RequestClass::Work
        }
        fn unload_all<'a>(
            &'a self,
            _: &'a reqwest::Client,
            _: &'a str,
        ) -> BoxFuture<'a, anyhow::Result<usize>> {
            Box::pin(async move {
                self.0.fetch_add(1, Ordering::SeqCst);
                Ok(1)
            })
        }
    }

    #[tokio::test]
    async fn finish_waits_for_live_work_but_never_for_held_work() {
        for uncertain in [false, true] {
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
                yield_on_veto = true
                on_owner_return = "finish"
            "#,
            )
            .unwrap();
            let calls = Arc::new(AtomicUsize::new(0));
            let mut state = AppState::new(config).unwrap();
            state.upstreams.get_mut("work").unwrap().adapter =
                Box::new(CountingAdapter(calls.clone()));
            let state = Arc::new(state);
            state.mark_presence_sampled();
            state.resources["mem"]
                .capacity
                .write()
                .unwrap()
                .record_success(crate::plugin::probe::Capacity::default());
            let ticket = crate::admission::reserve(
                &state,
                &state.config.upstreams[0],
                None,
                crate::plugin::adapter::Completion::ResponseClose,
            )
            .unwrap();
            state.veto.observe(Some("owner".into()), 0);
            let task = tokio::spawn(on_veto_engaged(state.clone()));
            // Another owner/pressure edge must not create a second waiter.
            on_veto_engaged(state.clone()).await;
            tokio::time::sleep(Duration::from_millis(20)).await;
            assert_eq!(calls.load(Ordering::SeqCst), 0);
            if uncertain {
                // A dropped transport leaves held work that nothing will
                // finish. It keeps its reservation but cannot defer the unload.
                drop(ticket);
            } else {
                ticket.response_complete();
            }
            tokio::time::timeout(Duration::from_secs(1), task)
                .await
                .unwrap()
                .unwrap();
            assert_eq!(calls.load(Ordering::SeqCst), 1);
            let work = state.admission.lock().unwrap().snapshot();
            if uncertain {
                assert_eq!(work.len(), 1);
                assert_eq!(work[0].phase, "held");
            } else {
                assert!(work.is_empty());
            }
        }
    }

    /// A ComfyUI stand-in with a real queue. Prompt ids starting with
    /// `owner-` belong to the owner's direct use and never pass the proxy.
    #[derive(Default)]
    struct Comfy {
        running: std::sync::Mutex<Vec<String>>,
        pending: std::sync::Mutex<Vec<String>>,
        history: std::sync::Mutex<Vec<(String, &'static str)>>,
        deletes: std::sync::Mutex<Vec<serde_json::Value>>,
        interrupts: std::sync::Mutex<Vec<serde_json::Value>>,
        interrupt_fails: std::sync::atomic::AtomicBool,
        frees: AtomicUsize,
        /// `system.comfyui_version` in `/system_stats`; `None` omits it.
        version: std::sync::Mutex<Option<String>>,
        /// ComfyUI 0.26.0 and later: `POST /api/jobs/{id}/cancel` exists.
        job_cancel: std::sync::atomic::AtomicBool,
        job_cancels: std::sync::Mutex<Vec<String>>,
        job_cancel_fails: std::sync::atomic::AtomicBool,
        /// Before 0.3.57, `/interrupt` ignores `prompt_id`.
        global_interrupt: std::sync::atomic::AtomicBool,
        /// Workflow bytes carried by each queue item and history entry.
        padding: AtomicUsize,
        /// How long ComfyUI takes to answer a job cancel and a history read,
        /// as with a large owner queue or a busy host.
        job_cancel_delay_ms: AtomicUsize,
        history_delay_ms: AtomicUsize,
        /// `GET /queue` and the job lookup answer 500.
        queue_fails: std::sync::atomic::AtomicBool,
        /// ComfyUI 0.6.0 and later: `GET /api/jobs/{id}` exists.
        jobs: std::sync::atomic::AtomicBool,
        queue_reads: AtomicUsize,
        job_lookups: AtomicUsize,
    }

    impl Comfy {
        /// A current release, with the atomic job cancel.
        fn current() -> Self {
            let comfy = Self::with_version(Some("0.26.0"));
            comfy.job_cancel.store(true, Ordering::SeqCst);
            comfy
        }

        fn with_version(version: Option<&str>) -> Self {
            let comfy = Self::default();
            *comfy.version.lock().unwrap() = version.map(str::to_owned);
            // The jobs route arrived in 0.6.0.
            let minor = version
                .and_then(|v| v.trim_start_matches('v').split('.').nth(1))
                .and_then(|m| m.parse::<u64>().ok());
            let major = version.and_then(|v| {
                v.trim_start_matches('v')
                    .split('.')
                    .next()?
                    .parse::<u64>()
                    .ok()
            });
            comfy.jobs.store(
                major.is_some_and(|m| m > 0) || minor.is_some_and(|m| m >= 6),
                Ordering::SeqCst,
            );
            comfy
        }

        /// A release before the prompt-scoped interrupt.
        fn old(version: Option<&str>) -> Self {
            let comfy = Self::with_version(version);
            comfy.global_interrupt.store(true, Ordering::SeqCst);
            comfy
        }

        fn finish(&self, id: &str, status: &'static str) {
            self.running.lock().unwrap().retain(|r| r != id);
            self.history.lock().unwrap().push((id.into(), status));
        }

        fn pad(&self) -> String {
            "w".repeat(self.padding.load(Ordering::SeqCst))
        }
    }

    async fn comfy(fake: Arc<Comfy>) -> (String, tokio::task::JoinHandle<()>) {
        use axum::extract::{Path, State};
        use axum::routing::{get, post};
        use axum::Json;
        let item = |c: &Comfy, id: &String| serde_json::json!([0, id, {"1": {"inputs": {"text": c.pad()}}}, {"extra_pnginfo": {"workflow": {}}}, ["9"]]);
        let app = axum::Router::new()
            .route(
                "/queue",
                get(move |State(c): State<Arc<Comfy>>| async move {
                    c.queue_reads.fetch_add(1, Ordering::SeqCst);
                    if c.queue_fails.load(Ordering::SeqCst) {
                        return Err(http::StatusCode::INTERNAL_SERVER_ERROR);
                    }
                    let running: Vec<_> = c.running.lock().unwrap().iter().map(|id| item(&c, id)).collect();
                    let pending: Vec<_> = c.pending.lock().unwrap().iter().map(|id| item(&c, id)).collect();
                    Ok(Json(serde_json::json!({"queue_running": running, "queue_pending": pending})))
                })
                .post(
                    |State(c): State<Arc<Comfy>>, Json(body): Json<serde_json::Value>| async move {
                        for id in body["delete"].as_array().unwrap() {
                            c.pending.lock().unwrap().retain(|p| p != id.as_str().unwrap());
                        }
                        c.deletes.lock().unwrap().push(body);
                        http::StatusCode::OK
                    },
                ),
            )
            .route(
                "/interrupt",
                post(
                    |State(c): State<Arc<Comfy>>, Json(body): Json<serde_json::Value>| async move {
                        c.interrupts.lock().unwrap().push(body.clone());
                        if c.interrupt_fails.load(Ordering::SeqCst) {
                            return http::StatusCode::INTERNAL_SERVER_ERROR;
                        }
                        let running = c.running.lock().unwrap().first().cloned();
                        let target = if c.global_interrupt.load(Ordering::SeqCst) {
                            // Old ComfyUI: whatever runs, whoever owns it.
                            running
                        } else {
                            // 0.3.57 and later: only the named running prompt.
                            running.filter(|id| body["prompt_id"] == **id)
                        };
                        if let Some(id) = target {
                            c.finish(&id, "error");
                        }
                        http::StatusCode::OK
                    },
                ),
            )
            .route(
                "/api/jobs/{id}/cancel",
                post(|State(c): State<Arc<Comfy>>, Path(id): Path<String>| async move {
                    let delay = c.job_cancel_delay_ms.load(Ordering::SeqCst) as u64;
                    tokio::time::sleep(Duration::from_millis(delay)).await;
                    if !c.job_cancel.load(Ordering::SeqCst) {
                        return (http::StatusCode::NOT_FOUND, Json(serde_json::json!(null)));
                    }
                    c.job_cancels.lock().unwrap().push(id.clone());
                    if c.job_cancel_fails.load(Ordering::SeqCst) {
                        return (http::StatusCode::INTERNAL_SERVER_ERROR, Json(serde_json::json!(null)));
                    }
                    let running = c.running.lock().unwrap().contains(&id);
                    let pending = c.pending.lock().unwrap().contains(&id);
                    if running {
                        c.finish(&id, "error");
                    } else if pending {
                        c.pending.lock().unwrap().retain(|p| *p != id);
                    }
                    (http::StatusCode::OK, Json(serde_json::json!({"cancelled": running || pending})))
                }),
            )
            .route(
                "/api/jobs/{id}",
                get(|State(c): State<Arc<Comfy>>, Path(id): Path<String>| async move {
                    use axum::response::IntoResponse;
                    if !c.jobs.load(Ordering::SeqCst) {
                        // aiohttp's answer for a route it does not have.
                        return (
                            http::StatusCode::NOT_FOUND,
                            [("content-type", "text/plain; charset=utf-8")],
                            "404: Not Found",
                        )
                            .into_response();
                    }
                    c.job_lookups.fetch_add(1, Ordering::SeqCst);
                    if c.queue_fails.load(Ordering::SeqCst) {
                        return (
                            http::StatusCode::INTERNAL_SERVER_ERROR,
                            Json(serde_json::json!({"error": "boom"})),
                        )
                            .into_response();
                    }
                    let finished = c
                        .history
                        .lock()
                        .unwrap()
                        .iter()
                        .find(|(h, _)| *h == id)
                        .map(|(_, status)| *status);
                    let status = if let Some(status) = finished {
                        if status == "success" { "completed" } else { "failed" }
                    } else if c.running.lock().unwrap().contains(&id) {
                        "in_progress"
                    } else if c.pending.lock().unwrap().contains(&id) {
                        "pending"
                    } else {
                        return (
                            http::StatusCode::NOT_FOUND,
                            Json(serde_json::json!({"error": "Job not found"})),
                        )
                            .into_response();
                    };
                    Json(serde_json::json!({"id": id, "status": status})).into_response()
                }),
            )
            .route(
                "/system_stats",
                get(|State(c): State<Arc<Comfy>>| async move {
                    let mut system = serde_json::json!({"os": "linux"});
                    if let Some(version) = c.version.lock().unwrap().clone() {
                        system["comfyui_version"] = version.into();
                    }
                    Json(serde_json::json!({"system": system, "devices": []}))
                }),
            )
            .route(
                "/history/{id}",
                get(|State(c): State<Arc<Comfy>>, Path(id): Path<String>| async move {
                    let delay = c.history_delay_ms.load(Ordering::SeqCst) as u64;
                    tokio::time::sleep(Duration::from_millis(delay)).await;
                    let status = c
                        .history
                        .lock()
                        .unwrap()
                        .iter()
                        .find(|(h, _)| *h == id)
                        .map(|(_, status)| *status);
                    Json(match status {
                        Some(status) => serde_json::json!({ id.clone(): {
                            "prompt": [0, id, {"1": {"inputs": {"text": c.pad()}}}, {}, []],
                            "outputs": {},
                            "status": {"status_str": status, "completed": status == "success", "messages": []},
                        }}),
                        None => serde_json::json!({}),
                    })
                }),
            )
            .route(
                "/free",
                post(|State(c): State<Arc<Comfy>>| async move {
                    c.frees.fetch_add(1, Ordering::SeqCst);
                    http::StatusCode::OK
                }),
            )
            .with_state(fake);
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let base = format!("http://{}", listener.local_addr().unwrap());
        let server = tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
        (base, server)
    }

    /// Admit one borrowed prompt through the ledger, exactly as the proxy
    /// does after a successful `/prompt` reply, and start its tracker.
    async fn borrowed_prompt(
        fake: Arc<Comfy>,
        policy: &str,
        yield_on_veto: bool,
        id: &str,
    ) -> (Arc<AppState>, tokio::task::JoinHandle<()>) {
        borrowed_submission(fake, policy, yield_on_veto, id, true).await
    }

    /// As [`borrowed_prompt`], with `confirmed` false for a job id vacancyd
    /// assigned whose submission reply never arrived.
    async fn borrowed_submission(
        fake: Arc<Comfy>,
        policy: &str,
        yield_on_veto: bool,
        id: &str,
        confirmed: bool,
    ) -> (Arc<AppState>, tokio::task::JoinHandle<()>) {
        let (base, server) = comfy(fake).await;
        let config: crate::config::Config = toml::from_str(&format!(
            r#"
            [server]
            listen = "127.0.0.1:0"
            [owner.gpu_pressure]
            enabled = false
            [[resource]]
            id = "mem"
            kind = "system-memory"
            [[upstream]]
            id = "comfy"
            listen = "127.0.0.1:0"
            target = "{base}"
            kind = "comfyui"
            resource = "mem"
            yield_on_veto = {yield_on_veto}
            {policy}
        "#
        ))
        .unwrap();
        let state = Arc::new(AppState::new(config).unwrap());
        state.mark_presence_sampled();
        state.resources["mem"]
            .capacity
            .write()
            .unwrap()
            .record_success(crate::plugin::probe::Capacity::default());
        state.upstreams["comfy"]
            .models
            .write()
            .unwrap()
            .record_success(crate::lifecycle::Models::default());
        let upstream = state.config.upstreams[0].clone();
        crate::admission::reserve_for(
            &state,
            &upstream,
            crate::gate::Demand::default(),
            crate::plugin::adapter::Completion::Tracked,
            (!confirmed).then_some(id),
        )
        .unwrap()
        .track(
            upstream,
            id.into(),
            if confirmed {
                crate::admission::Acknowledgement::Confirmed
            } else {
                crate::admission::Acknowledgement::Unconfirmed
            },
        );
        (state, server)
    }

    /// Run the owner-return reclaim to completion. The limit only guards
    /// against a hang: how long a reclaim takes depends on the host's load
    /// and the size of what it reads, and no test asserts it here.
    async fn reclaim(state: &Arc<AppState>) {
        state.veto.observe(Some("owner".into()), 0);
        tokio::time::timeout(HANG_GUARD, on_veto_engaged(state.clone()))
            .await
            .expect("the reclaim hung");
    }

    /// Long enough that only a hang reaches it, on any runner.
    const HANG_GUARD: Duration = Duration::from_secs(120);

    async fn wait_until(what: &str, done: impl Fn() -> bool) {
        tokio::time::timeout(HANG_GUARD, async {
            while !done() {
                tokio::time::sleep(Duration::from_millis(20)).await;
            }
        })
        .await
        .unwrap_or_else(|_| panic!("timed out waiting until {what}"));
    }

    fn held_work(state: &AppState) -> Vec<String> {
        state
            .admission
            .lock()
            .unwrap()
            .snapshot()
            .into_iter()
            .map(|w| w.tracking_id.unwrap_or_default())
            .collect()
    }

    #[tokio::test]
    async fn drain_lets_a_borrowed_prompt_finish_inside_the_window() {
        let fake = Arc::new(Comfy::current());
        fake.running.lock().unwrap().push("job-a".into());
        let (state, _server) = borrowed_prompt(
            fake.clone(),
            "on_owner_return = \"drain\"\ndrain_timeout_s = 20",
            true,
            "job-a",
        )
        .await;
        let started = std::time::Instant::now();
        state.veto.observe(Some("owner".into()), 0);
        let reclaim = tokio::spawn(on_veto_engaged(state.clone()));
        tokio::time::sleep(Duration::from_millis(300)).await;
        fake.finish("job-a", "success");
        tokio::time::timeout(Duration::from_secs(5), reclaim)
            .await
            .unwrap()
            .unwrap();
        // The unload follows completion, not the end of the 20-second window.
        assert!(started.elapsed() < Duration::from_secs(5));
        assert!(fake.interrupts.lock().unwrap().is_empty());
        assert!(fake.deletes.lock().unwrap().is_empty());
        assert_eq!(fake.frees.load(Ordering::SeqCst), 1);
        assert!(held_work(&state).is_empty());
    }

    #[tokio::test]
    async fn drain_overrun_interrupts_the_borrowed_prompt_and_releases_it() {
        let fake = Arc::new(Comfy::with_version(Some("0.3.57")));
        fake.running.lock().unwrap().push("job-a".into());
        fake.pending.lock().unwrap().push("owner-2".into());
        let (state, _server) = borrowed_prompt(
            fake.clone(),
            "on_owner_return = \"drain\"\ndrain_timeout_s = 1",
            true,
            "job-a",
        )
        .await;
        let started = std::time::Instant::now();
        reclaim(&state).await;
        assert!(started.elapsed() >= Duration::from_secs(1));
        assert_eq!(
            *fake.deletes.lock().unwrap(),
            vec![serde_json::json!({"delete": ["job-a"]})]
        );
        assert_eq!(
            *fake.interrupts.lock().unwrap(),
            vec![serde_json::json!({"prompt_id": "job-a"})]
        );
        assert!(held_work(&state).is_empty());
        // The owner's queued prompt was never named, and its models are
        // left loaded for it.
        assert_eq!(*fake.pending.lock().unwrap(), vec!["owner-2".to_owned()]);
        assert_eq!(fake.frees.load(Ordering::SeqCst), 0);
    }

    #[tokio::test]
    async fn the_owners_running_prompt_is_never_interrupted() {
        let fake = Arc::new(Comfy::current());
        fake.running.lock().unwrap().push("owner-1".into());
        fake.pending
            .lock()
            .unwrap()
            .extend(["job-a".to_owned(), "owner-2".to_owned()]);
        let (state, _server) =
            borrowed_prompt(fake.clone(), "on_owner_return = \"cancel\"", true, "job-a").await;
        reclaim(&state).await;
        assert!(fake.interrupts.lock().unwrap().is_empty());
        assert!(fake.job_cancels.lock().unwrap().is_empty());
        assert_eq!(*fake.running.lock().unwrap(), vec!["owner-1".to_owned()]);
        assert_eq!(*fake.pending.lock().unwrap(), vec!["owner-2".to_owned()]);
        // Removed before it ran, so the reservation is released. The owner's
        // prompts keep their models.
        assert!(held_work(&state).is_empty());
        assert_eq!(fake.frees.load(Ordering::SeqCst), 0);
    }

    #[tokio::test]
    async fn a_failed_interrupt_keeps_the_reservation_and_still_unloads() {
        let fake = Arc::new(Comfy::with_version(Some("0.3.57")));
        fake.running.lock().unwrap().push("job-a".into());
        fake.interrupt_fails.store(true, Ordering::SeqCst);
        let (state, _server) =
            borrowed_prompt(fake.clone(), "on_owner_return = \"cancel\"", true, "job-a").await;
        reclaim(&state).await;
        assert_eq!(fake.interrupts.lock().unwrap().len(), 1);
        assert_eq!(held_work(&state), vec!["job-a".to_owned()]);
        assert_eq!(fake.frees.load(Ordering::SeqCst), 1);
    }

    #[tokio::test]
    async fn cancel_mode_interrupts_at_once_even_without_unloading() {
        let fake = Arc::new(Comfy::current());
        fake.running.lock().unwrap().push("job-a".into());
        let (state, _server) = borrowed_prompt(
            fake.clone(),
            "on_owner_return = \"cancel\"\ndrain_timeout_s = 3600",
            false,
            "job-a",
        )
        .await;
        let started = std::time::Instant::now();
        let log = crate::test_log::capture();
        reclaim(&state).await;
        let logged = log.contents();
        drop(log);
        assert!(started.elapsed() < Duration::from_secs(1));
        // The cancellation names the trigger that caused it, under the
        // message existing log searches look for.
        let released = logged
            .lines()
            .find(|line| line.contains("cancelled borrowed work on owner return"))
            .unwrap_or_else(|| panic!("{logged}"));
        assert!(released.contains("trigger=\"owner-veto\""), "{released}");
        assert!(
            logged.contains("reclaiming borrowed work") && logged.contains("upstreams=[\"comfy\"]"),
            "{logged}"
        );
        let events = state.reclaim_log.lock().unwrap().events();
        let event = events.last().unwrap();
        assert_eq!(event.trigger, Trigger::OwnerVeto);
        assert_eq!(event.upstreams[0].on_owner_return, "cancel");
        assert_eq!(event.upstreams[0].released[0].how, "interrupted");
        // Current ComfyUI stops it through the atomic job cancel.
        assert_eq!(*fake.job_cancels.lock().unwrap(), vec!["job-a".to_owned()]);
        assert!(fake.interrupts.lock().unwrap().is_empty());
        assert!(held_work(&state).is_empty());
        // yield_on_veto is off: cancellation happens, unloading does not.
        assert_eq!(fake.frees.load(Ordering::SeqCst), 0);
    }

    #[tokio::test]
    async fn finish_mode_never_cancels_and_stops_waiting_at_the_completion_bound() {
        let fake = Arc::new(Comfy::current());
        fake.running.lock().unwrap().push("job-a".into());
        let (state, _server) = borrowed_prompt(
            fake.clone(),
            "on_owner_return = \"finish\"\ncompletion_timeout_s = 1",
            true,
            "job-a",
        )
        .await;
        reclaim(&state).await;
        assert!(fake.interrupts.lock().unwrap().is_empty());
        assert!(fake.deletes.lock().unwrap().is_empty());
        assert!(fake.job_cancels.lock().unwrap().is_empty());
        assert_eq!(fake.frees.load(Ordering::SeqCst), 1);
        assert_eq!(held_work(&state), vec!["job-a".to_owned()]);
    }

    #[tokio::test]
    async fn old_comfyui_never_receives_an_interrupt_that_could_reach_the_owners_prompt() {
        // Each of these either predates the prompt-scoped interrupt or cannot
        // show that it does not.
        for version in [
            Some("0.3.56"),
            Some("v0.3.40-12-g1a2b3c4"),
            Some("unknown"),
            Some(""),
            None,
        ] {
            let fake = Arc::new(Comfy::old(version));
            fake.running.lock().unwrap().push("job-a".into());
            fake.pending.lock().unwrap().push("owner-2".into());
            let (state, _server) =
                borrowed_prompt(fake.clone(), "on_owner_return = \"cancel\"", true, "job-a").await;
            reclaim(&state).await;
            assert!(fake.interrupts.lock().unwrap().is_empty(), "{version:?}");
            // The borrowed prompt keeps running and keeps its reservation;
            // the owner's queued prompt is untouched, and so are its models.
            assert_eq!(*fake.running.lock().unwrap(), vec!["job-a".to_owned()]);
            assert_eq!(*fake.pending.lock().unwrap(), vec!["owner-2".to_owned()]);
            assert_eq!(held_work(&state), vec!["job-a".to_owned()], "{version:?}");
            assert_eq!(fake.frees.load(Ordering::SeqCst), 0);
            // Its reservation ends on the ordinary completion evidence.
            fake.finish("job-a", "success");
            wait_until("the tracker releases the finished prompt", || {
                held_work(&state).is_empty()
            })
            .await;
        }
    }

    #[tokio::test]
    async fn the_scoped_interrupt_is_used_from_the_release_that_added_it() {
        for version in ["0.3.57", "v0.3.57-3-gabcdef0", "0.25.1"] {
            let fake = Arc::new(Comfy::with_version(Some(version)));
            fake.running.lock().unwrap().push("job-a".into());
            let (state, _server) =
                borrowed_prompt(fake.clone(), "on_owner_return = \"cancel\"", false, "job-a").await;
            reclaim(&state).await;
            assert_eq!(
                *fake.interrupts.lock().unwrap(),
                vec![serde_json::json!({"prompt_id": "job-a"})],
                "{version}"
            );
            assert!(held_work(&state).is_empty(), "{version}");
        }
    }

    #[tokio::test]
    async fn a_failed_atomic_job_cancel_is_not_retried_as_an_interrupt() {
        let fake = Arc::new(Comfy::current());
        fake.job_cancel_fails.store(true, Ordering::SeqCst);
        fake.running.lock().unwrap().push("job-a".into());
        let (state, _server) =
            borrowed_prompt(fake.clone(), "on_owner_return = \"cancel\"", true, "job-a").await;
        reclaim(&state).await;
        // The endpoint exists but failed: only a missing route may fall back.
        assert_eq!(fake.job_cancels.lock().unwrap().len(), 1);
        assert!(fake.interrupts.lock().unwrap().is_empty());
        assert_eq!(held_work(&state), vec!["job-a".to_owned()]);
        assert_eq!(fake.frees.load(Ordering::SeqCst), 1);
    }

    #[tokio::test]
    async fn an_owners_queue_far_beyond_one_mebibyte_still_cancels_borrowed_work() {
        for (running, jobs) in [(false, true), (true, true), (false, false), (true, false)] {
            let fake = Arc::new(Comfy::current());
            // With the jobs route (ComfyUI 0.6.0 and later) the queue is never
            // read; without it, the whole queue is.
            fake.jobs.store(jobs, Ordering::SeqCst);
            // Every item carries a 1.2 MiB workflow, so each alone exceeds the
            // old 1 MiB read cap, as does the interrupted prompt's history.
            fake.padding.store(1200 * 1024, Ordering::SeqCst);
            fake.running.lock().unwrap().push("owner-1".into());
            let mut pending: Vec<String> = (2..4).map(|n| format!("owner-{n}")).collect();
            if running {
                *fake.running.lock().unwrap() = vec!["job-a".into()];
            } else {
                pending.insert(1, "job-a".into());
            }
            *fake.pending.lock().unwrap() = pending.clone();
            let (state, _server) =
                borrowed_prompt(fake.clone(), "on_owner_return = \"cancel\"", true, "job-a").await;
            reclaim(&state).await;
            assert!(
                held_work(&state).is_empty(),
                "running {running} jobs {jobs}"
            );
            pending.retain(|id| id != "job-a");
            assert_eq!(*fake.pending.lock().unwrap(), pending);
            // Cancellation reads the queue only without the jobs route. The
            // check before the unload reads it once either way, stopping at
            // the owner's first prompt.
            if jobs {
                assert_eq!(fake.queue_reads.load(Ordering::SeqCst), 1);
                assert!(fake.job_lookups.load(Ordering::SeqCst) > 0);
            } else {
                assert!(fake.queue_reads.load(Ordering::SeqCst) > 1);
                assert_eq!(fake.job_lookups.load(Ordering::SeqCst), 0);
            }
            assert_eq!(fake.frees.load(Ordering::SeqCst), 0);
        }
    }

    /// A fixed 2-second budget on the whole cancellation once cut off a
    /// healthy one on a busy CI runner. Each step is now bounded by the work
    /// it waits on, so a stop request and a history read that are each slower
    /// than that old budget still complete the cancellation.
    #[tokio::test]
    async fn slow_upstream_replies_do_not_cut_a_cancellation_short() {
        let fake = Arc::new(Comfy::current());
        fake.running.lock().unwrap().push("job-a".into());
        fake.job_cancel_delay_ms.store(2500, Ordering::SeqCst);
        let (state, _server) =
            borrowed_prompt(fake.clone(), "on_owner_return = \"cancel\"", true, "job-a").await;
        // Slow history only after admission, so the tracker's own polls are
        // as slow as the confirmation's.
        fake.history_delay_ms.store(2500, Ordering::SeqCst);
        let started = std::time::Instant::now();
        reclaim(&state).await;
        assert!(started.elapsed() > Duration::from_millis(4500));
        assert_eq!(*fake.job_cancels.lock().unwrap(), vec!["job-a".to_owned()]);
        assert!(held_work(&state).is_empty());
        assert_eq!(fake.frees.load(Ordering::SeqCst), 1);
    }

    /// A queue that cannot be read (beyond the size limit, malformed, stalled
    /// or failing) leaves nothing interrupted, since no one can tell what
    /// runs. Deletion by id still went ahead, and the reservation stays for
    /// its tracker or an operator. The read limit itself is tested on the
    /// streamed reader; here the read simply fails.
    #[tokio::test]
    async fn an_unreadable_queue_fails_cancellation_and_keeps_the_hold() {
        let fake = Arc::new(Comfy::current());
        fake.running.lock().unwrap().push("job-a".into());
        let (state, _server) =
            borrowed_prompt(fake.clone(), "on_owner_return = \"cancel\"", true, "job-a").await;
        fake.queue_fails.store(true, Ordering::SeqCst);
        let log = crate::test_log::capture();
        reclaim(&state).await;
        let logged = log.contents();
        drop(log);
        assert_eq!(fake.deletes.lock().unwrap().len(), 1);
        assert!(fake.job_cancels.lock().unwrap().is_empty());
        assert!(fake.interrupts.lock().unwrap().is_empty());
        assert_eq!(held_work(&state), vec!["job-a".to_owned()]);
        // Whether the owner has work there cannot be read, so the unload
        // goes ahead as before, and the log says why.
        assert_eq!(fake.frees.load(Ordering::SeqCst), 1);
        assert!(
            logged.contains(
                "could not check the upstream for work vacancyd did not admit; unloading anyway"
            ),
            "{logged}"
        );
    }

    #[tokio::test]
    async fn the_unload_is_skipped_while_the_upstream_holds_the_owners_prompts() {
        for owner_running in [true, false] {
            let fake = Arc::new(Comfy::current());
            if owner_running {
                fake.running.lock().unwrap().push("owner-1".into());
                fake.pending.lock().unwrap().push("job-a".into());
            } else {
                fake.running.lock().unwrap().push("job-a".into());
                fake.pending
                    .lock()
                    .unwrap()
                    .extend(["owner-1".to_owned(), "owner-2".to_owned()]);
            }
            let (state, _server) =
                borrowed_prompt(fake.clone(), "on_owner_return = \"cancel\"", true, "job-a").await;
            let log = crate::test_log::capture();
            reclaim(&state).await;
            let logged = log.contents();
            drop(log);
            assert!(held_work(&state).is_empty(), "{owner_running}");
            assert_eq!(fake.frees.load(Ordering::SeqCst), 0, "{owner_running}");
            let line = logged
                .lines()
                .find(|line| line.contains("model unload skipped"))
                .unwrap_or_else(|| panic!("{logged}"));
            assert!(
                line.contains("the upstream holds work vacancyd did not admit"),
                "{line}"
            );
            // No prompt id or content reaches the log line.
            assert!(!line.contains("owner-1"), "{line}");
            let events = state.reclaim_log.lock().unwrap().events();
            assert!(events[0].upstreams[0].unload_skipped_for_foreign_work);
        }
    }

    #[tokio::test]
    async fn an_upstream_holding_only_borrowed_work_is_still_unloaded() {
        let fake = Arc::new(Comfy::current());
        fake.pending.lock().unwrap().push("job-a".into());
        let (state, _server) =
            borrowed_prompt(fake.clone(), "on_owner_return = \"cancel\"", true, "job-a").await;
        reclaim(&state).await;
        assert!(held_work(&state).is_empty());
        assert_eq!(fake.frees.load(Ordering::SeqCst), 1);
        let events = state.reclaim_log.lock().unwrap().events();
        assert!(!events[0].upstreams[0].unload_skipped_for_foreign_work);
    }

    #[tokio::test]
    async fn a_drain_does_not_stand_down_while_session_evidence_still_refuses() {
        let fake = Arc::new(Comfy::current());
        fake.running.lock().unwrap().push("job-a".into());
        let (state, _server) = borrowed_prompt(
            fake.clone(),
            "on_owner_return = \"drain\"\ndrain_timeout_s = 2",
            true,
            "job-a",
        )
        .await;
        state.veto.observe(Some("owner".into()), 0);
        let reclaim = tokio::spawn(on_veto_engaged(state.clone()));
        tokio::time::sleep(Duration::from_millis(300)).await;
        // The veto releases, but presence can no longer say who is there.
        *state.presence.write().unwrap() = crate::plugin::presence::PresenceSignal {
            session: crate::plugin::presence::SessionState::Unknown,
            session_authority: crate::plugin::presence::SessionAuthority::Ambiguous,
            ..Default::default()
        };
        state.veto.observe(None, 0);
        let snapshot = crate::gate::Snapshot::read(&state);
        assert!(
            crate::gate::owner_refusal(&state, &snapshot, &state.config.upstreams[0]).is_some()
        );
        tokio::time::timeout(HANG_GUARD, reclaim)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(*fake.job_cancels.lock().unwrap(), vec!["job-a".to_owned()]);
        assert_eq!(
            state.reclaim_log.lock().unwrap().events()[0].upstreams[0].abandoned_after_ms,
            None
        );
    }

    /// A prompt admitted through another upstream that reaches the same
    /// ComfyUI is vacancyd's own, not the owner's.
    #[tokio::test]
    async fn work_admitted_through_another_route_to_the_same_server_is_not_foreign() {
        let fake = Arc::new(Comfy::current());
        fake.pending.lock().unwrap().push("job-b".into());
        let (base, _server) = comfy(fake.clone()).await;
        // The same server by another spelling of loopback. The second route
        // neither cancels nor unloads, so its prompt stays queued.
        let other = base.replace("127.0.0.1", "localhost");
        let config: crate::config::Config = toml::from_str(&format!(
            r#"
            [server]
            listen = "127.0.0.1:0"
            [owner.gpu_pressure]
            enabled = false
            [[resource]]
            id = "mem"
            kind = "system-memory"
            [[upstream]]
            id = "comfy"
            listen = "127.0.0.1:0"
            target = "{base}"
            kind = "comfyui"
            resource = "mem"
            yield_on_veto = true
            on_owner_return = "cancel"
            [[upstream]]
            id = "comfy-api"
            listen = "127.0.0.1:0"
            target = "{other}/api"
            kind = "comfyui"
            resource = "mem"
            on_owner_return = "finish"
        "#
        ))
        .unwrap();
        let state = Arc::new(AppState::new(config).unwrap());
        state.mark_presence_sampled();
        state.resources["mem"]
            .capacity
            .write()
            .unwrap()
            .record_success(crate::plugin::probe::Capacity::default());
        state.upstreams["comfy-api"]
            .models
            .write()
            .unwrap()
            .record_success(crate::lifecycle::Models::default());
        let upstream = state.config.upstreams[1].clone();
        crate::admission::reserve_for(
            &state,
            &upstream,
            crate::gate::Demand::default(),
            crate::plugin::adapter::Completion::Tracked,
            None,
        )
        .unwrap()
        .track(
            upstream,
            "job-b".into(),
            crate::admission::Acknowledgement::Confirmed,
        );
        reclaim(&state).await;
        assert_eq!(*fake.pending.lock().unwrap(), vec!["job-b".to_owned()]);
        assert_eq!(fake.frees.load(Ordering::SeqCst), 1);
    }

    /// While a submission's reply is outstanding, its prompt may be queued
    /// under an id vacancyd cannot yet confirm, so nothing in the queue can
    /// be called the owner's: the unload goes ahead as before.
    #[tokio::test]
    async fn an_unconfirmed_submission_keeps_the_unload_and_says_why() {
        for in_flight in [false, true] {
            let fake = Arc::new(Comfy::current());
            fake.running.lock().unwrap().push("owner-1".into());
            // An id vacancyd assigned whose reply never confirmed it.
            let (state, _server) = borrowed_submission(
                fake.clone(),
                "on_owner_return = \"cancel\"",
                true,
                "job-a",
                false,
            )
            .await;
            let mut _submitting = None;
            if in_flight {
                // Instead, a submission whose reply is still awaited.
                let upstream = state.config.upstreams[0].clone();
                let entry = state.admission.lock().unwrap().entries_of(&upstream.id)[0];
                state.admission.lock().unwrap().finish_cancelled(entry);
                state.resources["mem"]
                    .capacity
                    .write()
                    .unwrap()
                    .record_success(crate::plugin::probe::Capacity::default());
                _submitting = Some(
                    crate::admission::reserve_for(
                        &state,
                        &upstream,
                        crate::gate::Demand::default(),
                        crate::plugin::adapter::Completion::Tracked,
                        Some("job-a"),
                    )
                    .unwrap(),
                );
            }
            let log = crate::test_log::capture();
            reclaim(&state).await;
            let logged = log.contents();
            drop(log);
            assert_eq!(fake.frees.load(Ordering::SeqCst), 1, "{in_flight}");
            assert!(
                logged.contains("cannot be told from the owner's; unloading"),
                "{logged}"
            );
            assert!(!logged.contains("model unload skipped"), "{logged}");
        }
    }

    #[tokio::test]
    async fn a_drain_stands_down_when_the_owner_leaves_and_a_later_return_reclaims() {
        let fake = Arc::new(Comfy::current());
        fake.running.lock().unwrap().push("job-a".into());
        let (state, _server) = borrowed_prompt(
            fake.clone(),
            "on_owner_return = \"drain\"\ndrain_timeout_s = 2",
            true,
            "job-a",
        )
        .await;
        let log = crate::test_log::capture();
        state.veto.observe(Some("owner".into()), 0);
        let first = tokio::spawn(on_veto_engaged(state.clone()));
        tokio::time::sleep(Duration::from_millis(300)).await;
        // The owner leaves again inside the drain window.
        state.veto.observe(None, 0);
        tokio::time::timeout(Duration::from_secs(1), first)
            .await
            .expect("the drain stands down without waiting for its deadline")
            .unwrap();
        let logged = log.contents();
        drop(log);
        assert!(fake.job_cancels.lock().unwrap().is_empty());
        assert!(fake.interrupts.lock().unwrap().is_empty());
        assert!(fake.deletes.lock().unwrap().is_empty());
        assert_eq!(fake.frees.load(Ordering::SeqCst), 0);
        assert_eq!(held_work(&state), vec!["job-a".to_owned()]);
        let line = logged
            .lines()
            .find(|line| line.contains("reclaim abandoned; its trigger cleared"))
            .unwrap_or_else(|| panic!("{logged}"));
        assert!(line.contains("reclaim=1"), "{line}");
        assert!(line.contains("upstream=\"comfy\""), "{line}");
        assert!(line.contains("elapsed_ms="), "{line}");
        let abandoned = state.reclaim_log.lock().unwrap().events()[0].upstreams[0]
            .abandoned_after_ms
            .expect("the stand-down is recorded");
        assert!(abandoned < 2_000, "{abandoned}");

        // The hold ended, so a later return is a reclaim of its own and runs
        // to its deadline as usual.
        reclaim(&state).await;
        assert_eq!(*fake.job_cancels.lock().unwrap(), vec!["job-a".to_owned()]);
        assert!(held_work(&state).is_empty());
        assert_eq!(fake.frees.load(Ordering::SeqCst), 1);
        let events = state.reclaim_log.lock().unwrap().events();
        assert_eq!(events.len(), 2);
        assert_eq!(events[1].upstreams[0].abandoned_after_ms, None);
    }

    /// A ComfyUI that chose its own job id, then a daemon crash: recovery
    /// must name the job that exists, so owner-return cancellation stops it
    /// instead of looking for the id vacancyd had assigned.
    #[tokio::test]
    async fn a_recovered_upstream_chosen_id_is_the_one_cancelled() {
        let fake = Arc::new(Comfy::current());
        fake.running
            .lock()
            .unwrap()
            .push("upstream-issued-id".into());
        let (base, _server) = comfy(fake.clone()).await;
        let config = format!(
            r#"
            [server]
            listen = "127.0.0.1:0"
            [owner.gpu_pressure]
            enabled = false
            [[resource]]
            id = "mem"
            kind = "system-memory"
            [[upstream]]
            id = "comfy"
            listen = "127.0.0.1:0"
            target = "{base}"
            kind = "comfyui"
            resource = "mem"
            on_owner_return = "cancel"
        "#
        );
        let journal = crate::secure_file::test_root().join(format!(
            "vacancyd-recovered-cancel-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        let before = Arc::new(AppState::new(toml::from_str(&config).unwrap()).unwrap());
        before.mark_presence_sampled();
        *before.admission.lock().unwrap() = crate::admission::Ledger::open(
            &journal,
            &before.config,
            crate::admission::Missing::Create,
        )
        .unwrap();
        // Evidence taken just before admission, however long opening the
        // journal took on a busy host.
        before.resources["mem"]
            .capacity
            .write()
            .unwrap()
            .record_success(crate::plugin::probe::Capacity::default());
        before.upstreams["comfy"]
            .models
            .write()
            .unwrap()
            .record_success(crate::lifecycle::Models::default());
        let upstream = before.config.upstreams[0].clone();
        crate::admission::reserve_for(
            &before,
            &upstream,
            crate::gate::Demand::default(),
            crate::plugin::adapter::Completion::Tracked,
            Some("0f2c5a1e-6b1d-4d6b-9d7e-3b1f0c2a9e11"),
        )
        .unwrap()
        .track(
            upstream,
            "upstream-issued-id".into(),
            crate::admission::Acknowledgement::Confirmed,
        );
        // The daemon crashes with the job running.
        *before.admission.lock().unwrap() = crate::admission::Ledger::default();

        let after = Arc::new(AppState::new(toml::from_str(&config).unwrap()).unwrap());
        after.mark_presence_sampled();
        *after.admission.lock().unwrap() = crate::admission::Ledger::open(
            &journal,
            &after.config,
            crate::admission::Missing::Create,
        )
        .unwrap();
        assert_eq!(held_work(&after), vec!["upstream-issued-id".to_owned()]);
        reclaim(&after).await;
        assert_eq!(
            *fake.job_cancels.lock().unwrap(),
            vec!["upstream-issued-id".to_owned()]
        );
        assert!(held_work(&after).is_empty());
        *after.admission.lock().unwrap() = crate::admission::Ledger::default();
        for file in std::fs::read_dir(&journal).unwrap() {
            std::fs::remove_file(file.unwrap().path()).unwrap();
        }
        std::fs::remove_dir(&journal).unwrap();
    }

    #[tokio::test]
    async fn an_unacknowledged_submission_is_released_only_on_evidence() {
        // Found pending under its assigned id: that proves the upstream took
        // it, so deleting it releases it.
        let fake = Arc::new(Comfy::current());
        fake.pending.lock().unwrap().push("job-a".into());
        let (state, _server) = borrowed_submission(
            fake.clone(),
            "on_owner_return = \"cancel\"",
            false,
            "job-a",
            false,
        )
        .await;
        reclaim(&state).await;
        assert!(fake.pending.lock().unwrap().is_empty());
        assert!(held_work(&state).is_empty());

        // Nowhere in the queue: it may never have been queued, or may be yet,
        // so the reservation stays and tracking continues.
        let fake = Arc::new(Comfy::current());
        let (state, _server) = borrowed_submission(
            fake.clone(),
            "on_owner_return = \"cancel\"",
            false,
            "job-b",
            false,
        )
        .await;
        reclaim(&state).await;
        assert_eq!(held_work(&state), vec!["job-b".to_owned()]);
        // Its history showing it ended is the evidence that releases it.
        fake.finish("job-b", "success");
        wait_until("history releases the ended prompt", || {
            held_work(&state).is_empty()
        })
        .await;
    }

    impl Adapter for HungAdapter {
        fn kind(&self) -> &'static str {
            "hung"
        }

        fn capabilities(&self) -> AdapterCapabilities {
            AdapterCapabilities {
                can_unload: true,
                reports_model: false,
                can_cancel: false,
            }
        }

        fn classify(&self, _: &http::Method, _: &str) -> RequestClass {
            RequestClass::PassThrough
        }

        fn unload_all<'a>(
            &'a self,
            _: &'a reqwest::Client,
            _: &'a str,
        ) -> BoxFuture<'a, anyhow::Result<usize>> {
            Box::pin(std::future::pending())
        }
    }

    #[test]
    fn dependency_matching_includes_primary_and_additional_resources() {
        let config: crate::config::Config = toml::from_str(
            r#"
            [server]
            listen = "127.0.0.1:0"
            [[resource]]
            id = "gpu0"
            kind = "nvidia-gpu"
            [[resource]]
            id = "mem"
            kind = "system-memory"
            [[resource]]
            id = "gpu1"
            kind = "nvidia-gpu"
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
        let upstream = &config.upstreams[0];

        assert!(upstream_depends_on_any(upstream, &HashSet::from(["gpu0"])));
        assert!(upstream_depends_on_any(upstream, &HashSet::from(["mem"])));
        assert!(!upstream_depends_on_any(upstream, &HashSet::from(["gpu1"])));
    }

    /// On a paused clock, so the production bound elapses at once in real
    /// time without a test-only value.
    #[tokio::test(start_paused = true)]
    async fn a_hung_adapter_cannot_hold_the_yield_path() {
        let config: crate::config::Config = toml::from_str(
            r#"
            [server]
            listen = "127.0.0.1:9811"
            [[resource]]
            id = "mem"
            kind = "system-memory"
            [[upstream]]
            id = "hung"
            listen = "127.0.0.1:11435"
            target = "http://127.0.0.1:11434"
            kind = "ollama"
            resource = "mem"
            yield_on_veto = true
            on_owner_return = "cancel"
            "#,
        )
        .unwrap();
        let mut state = AppState::new(config).unwrap();
        state.upstreams.get_mut("hung").unwrap().adapter = Box::new(HungAdapter);
        let started = std::time::Instant::now();
        let clock = tokio::time::Instant::now();

        on_veto_engaged(Arc::new(state)).await;

        // Held for exactly the unload bound, then released.
        assert_eq!(clock.elapsed(), YIELD_TIMEOUT);
        assert!(started.elapsed() < Duration::from_secs(1));
    }
}
