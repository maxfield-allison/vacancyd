//! Driving the attributed GPU pressure signal.
//!
//! Holds everything the signal needs across polls: one
//! [`PressureTracker`](crate::plugin::pressure::PressureTracker) per lendable
//! GPU, the adapter↔GPU mapping, the lender process set, and the saved
//! calibration. The collector calls [`PressureEngine::poll`] once per cycle and
//! gets back the resources that newly require targeted yield.
//!
//! Everything expensive here runs on its own cadence rather than the collector's:
//! the counter read spans about a thousand instances and the socket listing
//! spawns a process, neither of which is worth doing every couple of seconds
//! when the thing being measured moves over minutes.

use std::collections::{BTreeMap, HashSet};
use std::path::{Path, PathBuf};
use std::time::Instant;

use tracing::{debug, info, warn};

use crate::config::GpuPressureConfig;
use crate::plugin::pressure::{
    PressureMode, PressureStore, PressureTracker, PressureVerdict, ProfileState, Restored,
    StoredCalibration,
};
use crate::pressure_history::{top_processes, PressureRecord, Profile};
use crate::probes::gpu_id::{self, PciAddress, WindowsAdapter};
use crate::probes::{gpu_procs, listeners};
use crate::reclaim_log::RefusedResource;
use crate::state::{AdapterMapping, AdapterMappingSource, AppState, ObservationQuality};

use super::unseen_work::{self, UnseenWork};

/// How often the lender process tree is re-derived even without a newly seen
/// material-load process. This bounds how long a quiet restarted child can be
/// absent before it later becomes busy.
const LENDER_REFRESH_S: u64 = 10;
/// A newly observed process at or above this utilization causes immediate
/// listener/descendant rediscovery before it is classified.
const MATERIAL_PROCESS_PCT: f64 = 5.0;
/// Native adapter discovery is identity metadata, not a hot-path sample.
const ADAPTER_REFRESH_S: u64 = 60;
/// How often learned calibration is written back. Long, because the file is a
/// cache: losing the last few minutes of learning costs nothing measurable.
const SAVE_EVERY_S: u64 = 300;
/// The log target of the per-sample decision records written at debug.
const DECISION_TARGET: &str = "vacancyd::pressure";

pub struct PressureEngine {
    cfg: GpuPressureConfig,
    /// The pressure cache, or `None` when its directory is not private to
    /// the service and learning is kept for this run only.
    state_path: Option<PathBuf>,
    store: PressureStore,
    identities: BTreeMap<String, String>,
    trackers: BTreeMap<String, PressureTracker>,
    /// Exact adapter mappings live with the other observations in `AppState`;
    /// this engine only owns attribution-specific temporal state.
    lender_processes: HashSet<listeners::ProcessIdentity>,
    /// The same lender processes per upstream whose target is on this
    /// machine, which is what judging each upstream's visibility needs.
    /// `None` until a discovery succeeds and after one fails: an unread
    /// listener table says nothing about any upstream.
    upstream_lenders: Option<BTreeMap<String, HashSet<listeners::ProcessIdentity>>>,
    material_processes_seen: HashSet<listeners::ProcessIdentity>,
    /// Upstreams whose borrowed GPU work this engine may not be seeing.
    unseen: UnseenWork,
    /// Every upstream target; lender discovery keeps those on this machine.
    targets: Vec<String>,
    /// The upstream each of `targets` belongs to, in the same order.
    upstream_ids: Vec<String>,
    /// The ports of those targets, logged with each lender set change.
    target_ports: Vec<u16>,
    started: Instant,
    last_sample: Option<Instant>,
    last_lenders: Option<Instant>,
    last_adapters: Option<Instant>,
    last_save: Option<Instant>,
    sample_failure_logged: bool,
    lender_failure_logged: bool,
    /// What the last adapter inventory could not map. The inventory is
    /// re-read every minute, so a lasting problem is logged when it starts
    /// or changes, not on every read.
    adapter_problems: AdapterProblems,
    /// Resources whose reading went unknown while it was admitting work.
    unknown: BTreeMap<String, UnknownEpisode>,
    /// Collector time of each resource's last observed sample. An unknown
    /// episode is dated from it.
    last_observed_ms: BTreeMap<String, u64>,
}

/// An unknown pressure reading on a resource that had been admitting work.
/// New work is refused at once, but running work is reclaimed only if the
/// reading stays unknown for the tracker's gap limit: one failed sample
/// says nothing about the owner, and a reclaim can cost the borrower its
/// job. A valid sample ends the episode.
struct UnknownEpisode {
    /// Collector time of the reading's last good sample: what the grace
    /// measures is how long nothing could be observed, as the tracker's own
    /// gap rule does. Signed, since a stale reading's last sample may
    /// precede the collector's start.
    since_ms: i64,
    /// What the reclaim will say, if it comes to one.
    refusal: RefusedResource,
    reclaimed: bool,
}

impl PressureEngine {
    pub fn new(state: &AppState, config_path: &std::path::Path) -> Self {
        let cfg = state.config.owner.gpu_pressure.clone();
        let state_path = usable_cache_path(&cfg, config_path);
        let store = state_path
            .as_deref()
            .map(PressureStore::load)
            .unwrap_or_default();

        let mut trackers = BTreeMap::new();
        for id in lendable_gpu_ids(&state.config) {
            let rc = state
                .config
                .resources
                .iter()
                .find(|r| r.id == id)
                .expect("id came from the resource list");
            let t = PressureTracker::new(cfg.tracker_config(state.config.owner.release_after_s));
            trackers.insert(rc.id.clone(), t);
        }

        let targets: Vec<String> = state
            .config
            .upstreams
            .iter()
            .map(|u| u.target.as_str().to_owned())
            .collect();
        Self {
            cfg,
            state_path,
            store,
            identities: BTreeMap::new(),
            trackers,
            lender_processes: HashSet::new(),
            upstream_lenders: None,
            material_processes_seen: HashSet::new(),
            unseen: UnseenWork::default(),
            target_ports: listeners::ports_of_targets(&targets),
            targets,
            upstream_ids: state
                .config
                .upstreams
                .iter()
                .map(|u| u.id.clone())
                .collect(),
            started: Instant::now(),
            last_sample: None,
            last_lenders: None,
            last_adapters: None,
            last_save: None,
            sample_failure_logged: false,
            lender_failure_logged: false,
            adapter_problems: AdapterProblems::None,
            unknown: BTreeMap::new(),
            last_observed_ms: BTreeMap::new(),
        }
    }

    fn due(last: Option<Instant>, every_s: u64) -> bool {
        last.is_none_or(|t| t.elapsed().as_secs() >= every_s)
    }

    /// Sample if due and return resources whose pressure changed from
    /// admissible to refusing, each with why. The caller uses those rising
    /// edges to yield only upstreams that depend on the affected resource.
    pub fn poll(&mut self, state: &AppState) -> Vec<RefusedResource> {
        if !self.cfg.enabled || self.trackers.is_empty() {
            return Vec::new();
        }
        let mut rising = self.sample(state);
        // An unknown reading that outlasts its grace reclaims running work
        // whether or not this cycle could sample at all.
        rising.extend(self.overdue_unknown(state, self.now_ms()));
        rising
    }

    fn now_ms(&self) -> u64 {
        self.started.elapsed().as_millis() as u64
    }

    fn sample(&mut self, state: &AppState) -> Vec<RefusedResource> {
        let session_before = state.presence.read().unwrap().session;
        if presence_unresolved(state) {
            // A presence backend without a trustworthy answer yet (a view not
            // yet confirmed, an unreadable or inconsistent read) refuses
            // through the veto. Its profile is unknown, so nothing is sampled,
            // but the last verdict is kept rather than discarded: this is
            // usually one poll inside a sign-in or sign-out, and a lasting
            // outage still ages the verdict stale, which refuses.
            //
            // Staleness refuses new work but has no rising edge of its own,
            // and the veto does not reclaim an upstream with owner_scope =
            // "resource". Once a kept verdict is stale, mark it unknown so
            // work admitted on it is reclaimed like on any unavailable
            // pressure reading.
            self.expire_stale(
                state,
                "presence is unresolved, so GPU pressure cannot be sampled",
                self.now_ms(),
            );
            return Vec::new();
        }
        if session_before == crate::plugin::presence::SessionState::Unknown {
            self.mark_all_unknown(
                state,
                "attendance profile is unknown; calibration is suspended",
                self.now_ms(),
            );
            return Vec::new();
        }
        if !Self::due(self.last_sample, self.cfg.poll_ms.max(1000) / 1000) {
            return Vec::new();
        }
        self.last_sample = Some(Instant::now());

        let batch = match gpu_procs::sample() {
            Ok(batch) => batch,
            Err(error) => {
                self.mark_all_unknown(
                    state,
                    &format!("GPU attribution sample failed: {error}"),
                    self.now_ms(),
                );
                if !self.sample_failure_logged {
                    warn!(
                        %error,
                        "GPU attribution is unavailable; pressure remains unknown until a sample succeeds"
                    );
                    self.sample_failure_logged = true;
                }
                return Vec::new();
            }
        };
        if self.sample_failure_logged {
            info!("GPU attribution recovered");
            self.sample_failure_logged = false;
        }

        let mut processes = listeners::ProcessTable::capture();
        if Self::due(self.last_lenders, LENDER_REFRESH_S)
            || self.new_material_process(&batch, &processes)
        {
            self.last_lenders = Some(Instant::now());
            let listener_pids = match listeners::pids_serving(&self.targets) {
                Ok(pids) => {
                    if self.lender_failure_logged {
                        info!("lender process discovery recovered");
                        self.lender_failure_logged = false;
                    }
                    Some(pids)
                }
                Err(error) => {
                    // No lender is subtracted, so borrowed work counts as
                    // owner pressure: the reading errs towards protecting
                    // the owner. The listener observation on each upstream
                    // carries the same read's failure to `/status`.
                    if !self.lender_failure_logged {
                        warn!(%error, "lender process discovery failed; borrowed GPU work counts as owner pressure");
                        self.lender_failure_logged = true;
                    }
                    None
                }
            };
            // Bind the listener PIDs to identities observed after the listener read, so
            // a PID reused between the two reads cannot inherit the old
            // process generation's lender status.
            processes = listeners::ProcessTable::capture();
            self.upstream_lenders = listener_pids
                .map(|per_target| lenders_by_upstream(&self.upstream_ids, per_target, &processes));
            // The descendant closure of a union is the union of the
            // closures, so this is the set one combined expansion gives.
            let found: HashSet<listeners::ProcessIdentity> = self
                .upstream_lenders
                .iter()
                .flat_map(|by_upstream| by_upstream.values().flatten().copied())
                .collect();
            if found != self.lender_processes {
                debug!(processes = ?found, ports = ?self.target_ports, "lender process set changed");
            }
            self.lender_processes = found;
        }

        self.refresh_adapter_mappings(state);

        if presence_unresolved(state) {
            // Presence became unresolved while sampling: the reading has no
            // profile. Discard it, keeping the last verdict as above.
            return Vec::new();
        }
        let session_after = state.presence.read().unwrap().session;
        let Some(unattended) = sample_profile(session_before, session_after) else {
            self.mark_all_unknown(
                state,
                "attendance changed during GPU sampling; discarding mixed-profile reading",
                self.now_ms(),
            );
            return Vec::new();
        };
        let now_ms = self.started.elapsed().as_millis() as u64;
        let rising = self.apply_batch(state, &batch, &processes, unattended, now_ms);

        if self
            .last_save
            .is_some_and(|t| t.elapsed().as_secs() >= SAVE_EVERY_S)
        {
            self.last_save = Some(Instant::now());
            self.save();
        } else if self.last_save.is_none() {
            self.last_save = Some(Instant::now());
        }

        rising
    }

    fn apply_batch(
        &mut self,
        state: &AppState,
        batch: &gpu_procs::ProcGpuBatch,
        processes: &listeners::ProcessTable,
        unattended: bool,
        now_ms: u64,
    ) -> Vec<RefusedResource> {
        let mut rising = Vec::new();
        for (id, tracker) in self.trackers.iter_mut() {
            let mapping = state
                .adapter_mapping_view(id)
                .expect("tracked resource has runtime state");
            if mapping.quality != ObservationQuality::Ready {
                // Without an identity the next sample may come from another
                // card, so this one does break sustain and release timing.
                tracker.interrupt();
                let detail = mapping
                    .last_error
                    .as_deref()
                    .unwrap_or("GPU adapter identity is unavailable");
                let began = mark_unknown(
                    state,
                    id,
                    tracker,
                    &format!("GPU adapter identity is unavailable: {detail}"),
                );
                let since = last_observed(&self.last_observed_ms, id, now_ms);
                begin_unknown(&mut self.unknown, began, since);
                continue;
            }
            let luid = &mapping
                .value
                .as_ref()
                .expect("a ready adapter mapping always carries a value")
                .luid;
            if !batch.observed_luids.contains(luid) {
                // A missed sample does not interrupt the tracker: its own gap
                // rule does when valid samples are further apart than
                // `max_gap_ms`, so one miss keeps sustain timing continuous.
                tracker.note_missed_sample();
                let began = mark_unknown(
                    state,
                    id,
                    tracker,
                    "adapter was absent from the GPU counter batch",
                );
                let since = last_observed(&self.last_observed_ms, id, now_ms);
                begin_unknown(&mut self.unknown, began, since);
                continue;
            }
            let rc = state.config.resources.iter().find(|r| &r.id == id).unwrap();
            let pci = mapping.value.as_ref().unwrap().pci;
            let identity = gpu_identity(rc.selector.as_deref(), pci);
            let tracker_cfg = self.cfg.tracker_config(state.config.owner.release_after_s);
            if self.identities.get(id) != Some(&identity) {
                // An adapter remap must not carry an old GPU's calibration or
                // hysteresis into a newly discovered device.
                *tracker = PressureTracker::new(tracker_cfg.clone());
                let uuid_selector = is_uuid_selector(rc.selector.as_deref());
                match saved_calibration(
                    self.store.devices.get(id),
                    uuid_selector,
                    &identity,
                    tracker_cfg.mode,
                    &self.cfg.ignore,
                ) {
                    Ok(saved) => log_restore(id, &saved.learned, tracker.seed(&saved.learned)),
                    Err(reason) => info!(
                        resource = id,
                        reason = reason.as_str(),
                        "saved GPU calibration not restored; calibrating from new samples"
                    ),
                }
                self.identities.insert(id.clone(), identity.clone());
            }
            let counted = gpu_procs::non_lender_samples(
                &batch.processes,
                luid,
                &self.lender_processes,
                &self.cfg.ignore,
                &|pid| processes.identity(pid),
                &|pid| processes.name(pid),
            );
            let pct = gpu_procs::total_pct(&counted);
            let previous = state.pressure_view(id);
            let v = tracker.observe(now_ms, pct, unattended);
            let observed_before = self.last_observed_ms.insert(id.clone(), now_ms);
            // A reading still inside its unknown grace was admitting work
            // as far as running work knows: nothing reclaimed it yet.
            let mut unknown = self.unknown.remove(id).filter(|episode| !episode.reclaimed);
            // Unobserved for longer than the gap limit is an outage in its own
            // right, even when no poll ran out the grace before this sample.
            if observed_before.is_some_and(|at| now_ms.saturating_sub(at) > tracker_cfg.max_gap_ms)
            {
                if let Some(episode) = unknown.take() {
                    rising.push(overdue_refusal(&episode, now_ms, tracker_cfg.max_gap_ms));
                }
            }
            let was_clear = previous.as_ref().is_some_and(pressure_was_clear) || unknown.is_some();
            let was_engaged = previous.as_ref().is_some_and(|view| {
                view.quality == crate::state::ObservationQuality::Ready
                    && view.value.as_ref().is_some_and(|v| v.engaged)
            });
            if (v.engaged || !v.calibrated) && was_clear {
                rising.push(RefusedResource::from_verdict(id, &v));
                info!(
                    resource = id,
                    pressure_pct = pct,
                    "resource pressure engaged"
                );
            } else if !v.engaged && was_engaged {
                info!(
                    resource = id,
                    pressure_pct = pct,
                    "resource pressure cleared"
                );
            }
            if let Some(episode) = unknown.filter(|_| v.calibrated && !v.engaged) {
                info!(
                    resource = id,
                    unknown_for_ms = unknown_for(&episode, now_ms),
                    "GPU pressure reading recovered before running work was reclaimed"
                );
            }
            record_decision(
                state,
                id,
                Profile::of(unattended),
                &v,
                &tracker.profile_state(!unattended),
            );
            {
                let mut history = state.resources[id].pressure_history.lock().unwrap();
                history.set_top_processes(top_processes(&counted, |pid| processes.name(pid)));
                history.set_learned(tracker.summary(false), tracker.summary(true));
            }
            if tracker_cfg.mode == PressureMode::Fixed {
                // Fixed mode learns nothing; keep what calibration learned
                // for when the owner switches back.
            } else if is_uuid_selector(rc.selector.as_deref()) {
                self.store.devices.insert(
                    id.clone(),
                    StoredCalibration {
                        identity,
                        ignored_processes: self.cfg.ignore.clone(),
                        learned: tracker.snapshot(),
                    },
                );
            } else {
                self.store.devices.remove(id);
            }
            state.resources[id]
                .pressure
                .write()
                .unwrap()
                .record_success(v);
        }
        self.check_borrower_visibility(state, batch, processes);
        rising
    }

    /// Whether each upstream served on this machine shows the GPU work its
    /// admitted work should cause; see [`unseen_work`]. Reports only: it
    /// never changes what pressure counts.
    fn check_borrower_visibility(
        &mut self,
        state: &AppState,
        batch: &gpu_procs::ProcGpuBatch,
        processes: &listeners::ProcessTable,
    ) {
        let Some(lenders) = &self.upstream_lenders else {
            return;
        };
        let working = state.admission.lock().unwrap().upstreams_with_live_work();
        for upstream in &state.config.upstreams {
            let Some(tree) = lenders.get(&upstream.id) else {
                continue;
            };
            let gpus = tracked_gpus_of(upstream, &self.trackers);
            if gpus.is_empty() {
                continue;
            }
            let sample = unseen_work::Sample {
                working: working.contains(&upstream.id)
                    && gpus.iter().all(|id| observed_in(state, id, batch)),
                tree_used_gpu: batch.processes.iter().any(|gpu_use| {
                    processes
                        .identity(gpu_use.pid)
                        .is_some_and(|identity| tree.contains(&identity))
                }),
            };
            let Some(change) = self.unseen.observe(&upstream.id, sample) else {
                continue;
            };
            let reported = match change {
                unseen_work::Change::Unseen => {
                    warn!(
                        upstream = upstream.id,
                        resources = ?gpus,
                        "borrowed work ran on this upstream, but no process serving its target used a GPU. GPU pressure may miss that work and count it as owner load. This happens when the target's listener only relays connections, for example into a virtual machine or a container"
                    );
                    gpus
                }
                unseen_work::Change::Seen => {
                    info!(
                        upstream = upstream.id,
                        "a process serving this upstream's target was seen using a GPU, so the warning is cleared"
                    );
                    Vec::new()
                }
            };
            *state.upstreams[&upstream.id]
                .unseen_gpu_work
                .write()
                .unwrap() = reported;
        }
    }

    /// Mark each kept verdict that has aged stale as unknown. Those that had
    /// admitted work start their grace from their last good sample, not from
    /// now: the reading already went without one for its freshness window.
    fn expire_stale(&mut self, state: &AppState, error: &str, now_ms: u64) {
        for (id, tracker) in &mut self.trackers {
            let Some(view) = state
                .pressure_view(id)
                .filter(|view| view.quality == crate::state::ObservationQuality::Stale)
            else {
                continue;
            };
            tracker.note_missed_sample();
            let age_ms = view.age.map_or(0, |age| age.as_millis() as u64);
            let began = mark_unknown(state, id, tracker, error);
            let since = last_observed(&self.last_observed_ms, id, now_ms)
                .min(now_ms as i64 - age_ms as i64);
            begin_unknown(&mut self.unknown, began, since);
        }
    }

    /// Mark every reading unknown. A missed sample restarts only the release
    /// timer; see [`PressureTracker::note_missed_sample`].
    fn mark_all_unknown(&mut self, state: &AppState, error: &str, now_ms: u64) {
        for (id, tracker) in &mut self.trackers {
            tracker.note_missed_sample();
            let began = mark_unknown(state, id, tracker, error);
            let since = last_observed(&self.last_observed_ms, id, now_ms);
            begin_unknown(&mut self.unknown, began, since);
        }
    }

    /// The rising edges of unknown readings that have stayed unknown for the
    /// tracker's gap limit, each once per episode. The refusal says how long
    /// the reading had been unknown.
    fn overdue_unknown(&mut self, state: &AppState, now_ms: u64) -> Vec<RefusedResource> {
        let grace_ms = self
            .cfg
            .tracker_config(state.config.owner.release_after_s)
            .max_gap_ms;
        let mut overdue = Vec::new();
        for episode in self.unknown.values_mut() {
            let unknown_for_ms = unknown_for(episode, now_ms);
            if episode.reclaimed || unknown_for_ms < grace_ms {
                continue;
            }
            episode.reclaimed = true;
            overdue.push(overdue_refusal(episode, now_ms, grace_ms));
        }
        overdue
    }

    fn new_material_process(
        &mut self,
        batch: &gpu_procs::ProcGpuBatch,
        processes: &listeners::ProcessTable,
    ) -> bool {
        let mut found = false;
        for identity in batch
            .processes
            .iter()
            .filter(|sample| sample.util_pct >= MATERIAL_PROCESS_PCT)
            .filter_map(|sample| processes.identity(sample.pid))
        {
            found |= self.material_processes_seen.insert(identity);
        }
        found
    }

    /// Refresh the exact PCI-address to Windows-LUID associations.
    fn refresh_adapter_mappings(&mut self, state: &AppState) {
        if !Self::due(self.last_adapters, ADAPTER_REFRESH_S) {
            return;
        }
        self.last_adapters = Some(Instant::now());

        let inventory = gpu_id::windows_adapters();
        let problems = AdapterProblems::of(&inventory);
        match adapter_problem_change(&self.adapter_problems, &problems) {
            Some(AdapterLog::Unavailable(error)) => {
                warn!(error, "GPU adapter inventory is unavailable");
            }
            Some(AdapterLog::Partial(errors)) => {
                warn!(?errors, "some GPU adapter addresses were unavailable");
            }
            Some(AdapterLog::Recovered) => info!("GPU adapter addresses are readable again"),
            None => {}
        }
        self.adapter_problems = problems;
        let adapters = match inventory {
            Ok(inventory) => {
                if !inventory.not_pci.is_empty() {
                    debug!(
                        luids = ?inventory.not_pci,
                        "skipped GPU adapters that are not PCI devices"
                    );
                }
                inventory.adapters
            }
            Err(error) => {
                for id in self.trackers.keys() {
                    state.resources[id]
                        .adapter_mapping
                        .write()
                        .unwrap()
                        .record_failure(error.to_string());
                }
                return;
            }
        };

        let mut claims = Vec::new();
        for id in self.trackers.keys() {
            let rc = state
                .config
                .resources
                .iter()
                .find(|resource| &resource.id == id)
                .expect("tracked resource came from config");
            let probed = state.resources[id].probe.lock().unwrap().adapter();
            let claimed = match (rc.adapter, probed) {
                (Some(configured), Some(observed)) if configured != observed => {
                    let error = format!(
                        "configured adapter {configured} disagrees with probe adapter {observed}"
                    );
                    state.resources[id]
                        .adapter_mapping
                        .write()
                        .unwrap()
                        .record_failure(error);
                    continue;
                }
                (Some(configured), _) => (configured, AdapterMappingSource::Config),
                (None, Some(observed)) => (observed, AdapterMappingSource::Probe),
                (None, None) => {
                    state.resources[id]
                        .adapter_mapping
                        .write()
                        .unwrap()
                        .record_failure("vendor probe has not reported a PCI address");
                    continue;
                }
            };
            claims.push((id.clone(), claimed.0, claimed.1));
        }

        for (id, attempt) in join_adapters(&claims, &adapters) {
            match attempt {
                MappingAttempt::Mapped(mapping) => {
                    let previous = state
                        .adapter_mapping_view(&id)
                        .expect("tracked resource has runtime state");
                    let changed = previous.quality != ObservationQuality::Ready
                        || previous.value.as_ref() != Some(&mapping);
                    state.resources[&id]
                        .adapter_mapping
                        .write()
                        .unwrap()
                        .record_success(mapping.clone());
                    if changed {
                        info!(
                            resource = id,
                            pci = %mapping.pci,
                            luid = mapping.luid,
                            source = ?mapping.source,
                            "identified GPU adapter for pressure attribution"
                        );
                    }
                }
                MappingAttempt::Unmapped(error) => {
                    state.resources[&id]
                        .adapter_mapping
                        .write()
                        .unwrap()
                        .record_failure(error.clone());
                    debug!(resource = id, error, "GPU adapter identity is unavailable");
                }
            }
        }
    }

    /// Persist learned calibration. Best-effort: a cache that cannot be written
    /// is a warm start lost, not a reason to disturb a running daemon.
    pub fn save(&mut self) {
        let Some(path) = &self.state_path else {
            return;
        };
        if let Err(e) = self.store.save(path) {
            warn!(path = %path.display(), error = %e, "could not persist GPU baselines");
        }
    }
}

/// The pressure cache this configuration may use, or `None`, logged, when a
/// configured `state_file` sits in a directory another account can change.
///
/// Such an account could plant the file, seeding calibration that decides
/// when lending is refused, or replace it between saves; the service runs as
/// root or LocalSystem, so its own files are worth protecting there too. The
/// default location is not checked: it sits beside the configuration, and an
/// account that can change that directory can change the configuration.
fn usable_cache_path(cfg: &GpuPressureConfig, config_path: &Path) -> Option<PathBuf> {
    let path = cfg.state_path(config_path);
    if cfg.state_file.is_empty() {
        return Some(path);
    }
    match prepare_cache_directory(&path) {
        Ok(()) => Some(path),
        Err(reason) => {
            warn!(
                setting = "owner.gpu_pressure.state_file",
                path = %path.display(),
                reason,
                "pressure cache not used: its directory is not private to the service; GPU calibration is kept for this run only. Choose a directory only the service account can write"
            );
            None
        }
    }
}

/// Apply the configured-cache trust boundary without creating directories or
/// logging from a diagnostics request. Default caches share the config's trust.
pub(crate) fn readable_cache_path(cfg: &GpuPressureConfig, config_path: &Path) -> Option<PathBuf> {
    let path = cfg.state_path(config_path);
    if cfg.state_file.is_empty() {
        return Some(path);
    }
    let directory = cache_directory(&path);
    if cache_path_refusal(directory).is_some() {
        return None;
    }
    match crate::secure_file::shared_directory(directory) {
        Ok(None) => Some(path),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Some(path),
        _ => None,
    }
}

/// Use the cache file's existing directory only if it is private, and create
/// a missing one private (on Windows, to Administrators and LocalSystem).
/// `Err` says why the cache must not be used.
///
/// The service runs as root or LocalSystem, so nothing here may act through
/// a path another account can redirect: a directory swapped for a link
/// between its creation and a permission change would have that change
/// applied to whatever the link names. The whole path is checked before
/// anything is created. Unix directories are private from creation; Windows
/// directories are protected before cache files or child directories are
/// created inside them.
fn prepare_cache_directory(path: &Path) -> Result<(), String> {
    let directory = cache_directory(path);
    if let Some(reason) = cache_path_refusal(directory) {
        return Err(reason);
    }
    match crate::secure_file::shared_directory(directory) {
        Ok(None) => return Ok(()),
        Ok(Some(reason)) => return Err(reason),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
        Err(error) => return Err(format!("its permissions could not be read: {error}")),
    }
    crate::secure_file::create_private_dir_all(directory)
        .map_err(|error| format!("it could not be created: {error}"))?;
    match crate::secure_file::shared_directory(directory) {
        Ok(None) => Ok(()),
        Ok(Some(reason)) => Err(reason),
        Err(error) => Err(format!("its permissions could not be read: {error}")),
    }
}

/// Why another account could redirect the path to the cache directory, or
/// `None`.
fn cache_path_refusal(directory: &Path) -> Option<String> {
    match crate::secure_file::replaceable_path(directory) {
        Ok(reason) => reason,
        Err(error) => Some(format!("its path could not be checked: {error}")),
    }
}

fn cache_directory(path: &Path) -> &Path {
    path.parent()
        .filter(|parent| !parent.as_os_str().is_empty())
        .unwrap_or_else(|| Path::new("."))
}

/// `check-config`: warn when the service would refuse a configured pressure
/// cache because another account can change its directory or a directory on
/// the way to it. A missing directory in a path that passes is not reported;
/// the service creates it private.
pub fn warn_shared_cache_directory(cfg: &GpuPressureConfig, config_path: &Path) {
    if !cfg.enabled || cfg.state_file.is_empty() {
        return;
    }
    let path = cfg.state_path(config_path);
    let directory = cache_directory(&path);
    let reason = match cache_path_refusal(directory) {
        Some(reason) => reason,
        None => match crate::secure_file::shared_directory(directory) {
            Ok(None) => return,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => return,
            Ok(Some(reason)) => reason,
            Err(error) => format!("its permissions could not be read: {error}"),
        },
    };
    warn!(
        setting = "owner.gpu_pressure.state_file",
        path = %path.display(),
        reason,
        "the service will not use this pressure cache, because its directory is not private to the service; GPU calibration would be lost at every restart. Choose a directory only the service account can write"
    );
}

/// Why a saved calibration was not restored.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum NotRestored {
    /// Only UUID selectors are cached: an index can name another card after
    /// a driver or slot change.
    IndexSelector,
    NoEntry,
    IdentityMismatch,
    /// Fixed mode uses its configured threshold; the saved calibration is
    /// kept for a later switch back.
    FixedMode,
    IgnoreListChanged,
}

impl NotRestored {
    fn as_str(self) -> &'static str {
        match self {
            Self::IndexSelector => "index selector not cached",
            Self::NoEntry => "no saved entry",
            Self::IdentityMismatch => "GPU identity mismatch",
            Self::FixedMode => "fixed mode; saved calibration kept",
            Self::IgnoreListChanged => "ignore list changed",
        }
    }
}

/// The saved calibration for a resource, if it is still valid for this GPU
/// and ignore list.
///
/// Only what decides the measured value is checked. The quantile, margin,
/// floor, cap, half-life, rise rate and timings are applied to the histogram
/// afterwards, so changing them keeps it. The borrower ports are deliberately
/// not checked either. They choose which processes are subtracted, but a
/// borrower that is not running work adds almost nothing to an idle card, and the set changes whenever an
/// upstream is added for another GPU. Keying on it discarded every card's
/// calibration when a machine's second GPU was first lent.
fn saved_calibration<'a>(
    saved: Option<&'a StoredCalibration>,
    uuid_selector: bool,
    identity: &str,
    mode: PressureMode,
    ignore: &[String],
) -> Result<&'a StoredCalibration, NotRestored> {
    if !uuid_selector {
        return Err(NotRestored::IndexSelector);
    }
    let saved = saved.ok_or(NotRestored::NoEntry)?;
    if !same_gpu(&saved.identity, identity) {
        Err(NotRestored::IdentityMismatch)
    } else if mode == PressureMode::Fixed {
        Err(NotRestored::FixedMode)
    } else if !same_names(&saved.ignored_processes, ignore) {
        Err(NotRestored::IgnoreListChanged)
    } else {
        Ok(saved)
    }
}

/// A UUID selector names a physical card: the driver reports the same UUID
/// for it whatever slot or bus number it is enumerated at.
fn is_uuid_selector(selector: Option<&str>) -> bool {
    selector.is_some_and(|s| s.starts_with("GPU-"))
}

/// What a resource's tracker and saved calibration belong to.
///
/// For a UUID selector that is the UUID alone. The PCI address is not part of
/// it: renumbering a bus, which a change to another device in the machine can
/// do, moves the same card to a new address, and keying on the address
/// discarded that card's learning. An index selector keeps its PCI address,
/// because an index can name another card after a driver or slot change;
/// its calibration is never saved (see [`NotRestored::IndexSelector`]).
fn gpu_identity(selector: Option<&str>, pci: PciAddress) -> String {
    match selector {
        Some(uuid) if is_uuid_selector(Some(uuid)) => uuid.to_owned(),
        other => format!("{}@{pci}", other.unwrap_or("index:0")),
    }
}

/// Whether a saved identity names the GPU `identity` names. Caches written
/// before the PCI address left the key saved `<uuid>@<pci>`; the UUID in such
/// an entry is what identified the card, so it still matches, and the next
/// save rewrites the entry under the UUID alone.
fn same_gpu(saved: &str, identity: &str) -> bool {
    saved == identity
        || saved
            .split_once('@')
            .is_some_and(|(uuid, _pci)| is_uuid_selector(Some(uuid)) && uuid == identity)
}

/// Ignore lists compare as [`gpu_procs::non_lender_samples`] applies them:
/// without ASCII case, skipping empty entries, regardless of order or repeats.
fn same_names(a: &[String], b: &[String]) -> bool {
    let set = |names: &[String]| {
        names
            .iter()
            .filter(|n| !n.is_empty())
            .map(|n| n.to_ascii_lowercase())
            .collect::<std::collections::BTreeSet<_>>()
    };
    set(a) == set(b)
}

/// Log what restoring a matching saved calibration produced. A profile
/// carried over from an older cache's frozen baseline says so: its threshold
/// stands until its own histogram is warm.
fn log_restore(
    id: &str,
    saved: &crate::plugin::pressure::PressureSnapshot,
    restored: [Restored; 2],
) {
    let usable = |r: &Restored| matches!(r, Restored::Learned | Restored::LegacyBaseline);
    if !restored.iter().any(usable) {
        let reason = if saved.is_empty() {
            "no completed calibration saved"
        } else {
            "saved calibration invalid"
        };
        info!(
            resource = id,
            reason, "saved GPU calibration not restored; calibrating from new samples"
        );
        return;
    }
    let profiles = |kind: Restored| {
        [Profile::Attended, Profile::Unattended]
            .into_iter()
            .zip(restored)
            .filter(|(_, r)| *r == kind)
            .map(|(profile, _)| profile.as_str())
            .collect::<Vec<_>>()
            .join(", ")
    };
    let legacy = profiles(Restored::LegacyBaseline);
    let invalid = profiles(Restored::Invalid);
    // Absent fields are not logged.
    info!(
        resource = id,
        migrated_legacy_baseline = (!legacy.is_empty()).then_some(legacy.as_str()),
        invalid_profile = (!invalid.is_empty()).then_some(invalid.as_str()),
        "restored validated GPU calibration"
    );
}

/// Publish one sample's decision: the pressure gauges, the recent-sample
/// history, and at debug level one structured record.
///
/// The per-profile gauges are set for both attendance profiles on every
/// sample: the observed one from `verdict`, the other from what the tracker
/// holds for it (`other`). A profile's calibration can be lost while it is
/// not observed, and only re-reading the tracker shows that; a value kept
/// from the last time the profile was observed would be stale. Engagement
/// belongs to the current sample, so only the observed profile has an
/// engaged series.
fn record_decision(
    state: &AppState,
    id: &str,
    profile: Profile,
    verdict: &PressureVerdict,
    other: &ProfileState,
) {
    let metrics = &state.metrics;
    metrics
        .resource_pressure_pct
        .with_label_values(&[id])
        .set(verdict.non_lender_pct);
    let observed = ProfileState {
        calibrated: verdict.calibrated,
        calibration_samples: verdict.calibration_samples,
        threshold_pct: verdict.threshold_pct,
        baseline_pct: verdict.baseline_pct,
        target_pct: verdict.target_pct,
        learned_samples: verdict.learned_samples,
        learned_s: verdict.learned_s,
    };
    let other_profile = Profile::of(profile == Profile::Attended);
    publish_profile(state, id, profile, &observed);
    publish_profile(state, id, other_profile, other);
    metrics
        .resource_pressure_engaged
        .with_label_values(&[id, profile.as_str()])
        .set(i64::from(verdict.engaged));
    let _ = metrics
        .resource_pressure_engaged
        .remove_label_values(&[id, other_profile.as_str()]);
    state.resources[id]
        .pressure_history
        .lock()
        .unwrap()
        .push(PressureRecord::now(verdict, profile));
    debug!(
        target: DECISION_TARGET,
        resource = id,
        non_lender_pct = verdict.non_lender_pct,
        profile = profile.as_str(),
        threshold_pct = verdict.threshold_pct,
        target_pct = verdict.target_pct,
        baseline_pct = verdict.baseline_pct,
        calibrated = verdict.calibrated,
        calibration_samples = verdict.calibration_samples,
        calibration_required = verdict.calibration_required,
        engaged = verdict.engaged,
        above_for_s = verdict.above_for_s,
        mode = verdict.mode.as_str(),
        "pressure sample"
    );
}

/// One profile's calibration gauges. The threshold is the one in force. The
/// baseline (the learned quantile) and target are absent rather than zero
/// while unlearned: an unlearned quantile is not 0%.
fn publish_profile(state: &AppState, id: &str, profile: Profile, learned: &ProfileState) {
    let metrics = &state.metrics;
    let labels = [id, profile.as_str()];
    metrics
        .resource_calibrated
        .with_label_values(&labels)
        .set(i64::from(learned.calibrated));
    metrics
        .resource_calibration_samples
        .with_label_values(&labels)
        .set(learned.calibration_samples as i64);
    metrics
        .resource_pressure_threshold_pct
        .with_label_values(&labels)
        .set(learned.threshold_pct);
    for (gauge, value) in [
        (
            &metrics.resource_pressure_baseline_pct,
            learned.baseline_pct,
        ),
        (&metrics.resource_pressure_target_pct, learned.target_pct),
    ] {
        match value {
            Some(value) => gauge.with_label_values(&labels).set(value),
            None => {
                let _ = gauge.remove_label_values(&labels);
            }
        }
    }
}

/// Failures in one adapter inventory read, compared read to read.
#[derive(Debug, Clone, PartialEq, Eq)]
enum AdapterProblems {
    None,
    Unavailable(String),
    Partial(Vec<String>),
}

impl AdapterProblems {
    fn of(inventory: &anyhow::Result<gpu_id::AdapterInventory>) -> Self {
        match inventory {
            Err(error) => Self::Unavailable(format!("{error:#}")),
            Ok(inventory) if inventory.errors.is_empty() => Self::None,
            // `errors` is sorted, so equal sets compare equal.
            Ok(inventory) => Self::Partial(inventory.errors.clone()),
        }
    }
}

#[derive(Debug, PartialEq, Eq)]
enum AdapterLog<'a> {
    Unavailable(&'a str),
    Partial(&'a [String]),
    Recovered,
}

/// What, if anything, a new inventory read is worth logging: a problem when
/// it first appears or changes, and its end. A repeat logs nothing.
fn adapter_problem_change<'a>(
    last: &AdapterProblems,
    now: &'a AdapterProblems,
) -> Option<AdapterLog<'a>> {
    if last == now {
        return None;
    }
    Some(match now {
        AdapterProblems::None => AdapterLog::Recovered,
        AdapterProblems::Unavailable(error) => AdapterLog::Unavailable(error),
        AdapterProblems::Partial(errors) => AdapterLog::Partial(errors),
    })
}

#[derive(Debug, PartialEq, Eq)]
enum MappingAttempt {
    Mapped(AdapterMapping),
    Unmapped(String),
}

/// Exact, injective join. Ambiguity on either side refuses the affected
/// resource instead of resolving by iteration order.
fn join_adapters(
    resources: &[(String, PciAddress, AdapterMappingSource)],
    adapters: &[WindowsAdapter],
) -> BTreeMap<String, MappingAttempt> {
    let mut out = BTreeMap::new();
    for (id, pci, source) in resources {
        let claimants = resources
            .iter()
            .filter(|(_, candidate, _)| candidate == pci)
            .count();
        if claimants != 1 {
            out.insert(
                id.clone(),
                MappingAttempt::Unmapped(format!(
                    "PCI adapter {pci} is claimed by {claimants} resources"
                )),
            );
            continue;
        }

        let matches: Vec<_> = adapters
            .iter()
            .filter(|adapter| adapter.pci == *pci)
            .collect();
        match matches.as_slice() {
            [adapter] => {
                out.insert(
                    id.clone(),
                    MappingAttempt::Mapped(AdapterMapping {
                        pci: *pci,
                        luid: adapter.luid.to_ascii_lowercase(),
                        source: *source,
                    }),
                );
            }
            [] => {
                out.insert(
                    id.clone(),
                    MappingAttempt::Unmapped(format!(
                        "no Windows adapter reported PCI address {pci}"
                    )),
                );
            }
            many => {
                out.insert(
                    id.clone(),
                    MappingAttempt::Unmapped(format!(
                        "{} Windows adapters reported PCI address {pci}",
                        many.len()
                    )),
                );
            }
        }
    }
    out
}

/// Each upstream's lender processes, for the upstreams whose target is on
/// this machine. `per_target` is in `upstream_ids` order.
fn lenders_by_upstream(
    upstream_ids: &[String],
    per_target: Vec<Option<HashSet<u32>>>,
    processes: &listeners::ProcessTable,
) -> BTreeMap<String, HashSet<listeners::ProcessIdentity>> {
    upstream_ids
        .iter()
        .zip(per_target)
        .filter_map(|(id, pids)| Some((id.clone(), listeners::lender_processes(&pids?, processes))))
        .collect()
}

/// The GPUs this engine tracks among those `upstream` borrows, sorted.
fn tracked_gpus_of(
    upstream: &crate::config::UpstreamConfig,
    trackers: &BTreeMap<String, PressureTracker>,
) -> Vec<String> {
    let mut gpus: Vec<String> = std::iter::once(&upstream.resource)
        .chain(&upstream.also_requires)
        .filter(|id| trackers.contains_key(*id))
        .cloned()
        .collect();
    gpus.sort_unstable();
    gpus.dedup();
    gpus
}

/// Whether `batch` measured the GPU tracked as `id`: its adapter is mapped
/// and was in the counter batch.
fn observed_in(state: &AppState, id: &str, batch: &gpu_procs::ProcGpuBatch) -> bool {
    state.adapter_mapping_view(id).is_some_and(|mapping| {
        mapping.quality == ObservationQuality::Ready
            && mapping
                .value
                .is_some_and(|mapping| batch.observed_luids.contains(&mapping.luid))
    })
}

/// Invalidate one pressure input. A returned refusal means an upstream could
/// have been admitted immediately before this failure and may need to be
/// yielded if the reading stays unknown; it carries the reading's quality at
/// that moment and `error`.
///
/// The failure is published, not only recorded: the reading and engaged
/// series are removed, since neither has a current value; both profiles'
/// calibration gauges are re-read from `tracker`, so they show its state,
/// including any reset the caller made for a lost adapter identity; and the
/// history gains an unknown record with `error`, so an export shows the gap
/// and its reason.
fn mark_unknown(
    state: &AppState,
    id: &str,
    tracker: &PressureTracker,
    error: &str,
) -> Option<RefusedResource> {
    let previous = state.pressure_view(id);
    let was_clear = previous.as_ref().is_some_and(pressure_was_clear);
    state.resources[id]
        .pressure
        .write()
        .unwrap()
        .record_failure(error);
    let metrics = &state.metrics;
    let _ = metrics.resource_pressure_pct.remove_label_values(&[id]);
    for profile in [Profile::Attended, Profile::Unattended] {
        let _ = metrics
            .resource_pressure_engaged
            .remove_label_values(&[id, profile.as_str()]);
        publish_profile(
            state,
            id,
            profile,
            &tracker.profile_state(profile == Profile::Unattended),
        );
    }
    state.resources[id]
        .pressure_history
        .lock()
        .unwrap()
        .push_unknown(error);
    let quality = previous.map_or("unknown", |view| view.quality.as_str());
    was_clear.then(|| RefusedResource::unknown(id, quality, error))
}

/// The rising edge of an episode whose grace ran out, logged.
fn overdue_refusal(episode: &UnknownEpisode, now_ms: u64, grace_ms: u64) -> RefusedResource {
    let unknown_for_ms = unknown_for(episode, now_ms);
    info!(
        resource = episode.refusal.resource,
        unknown_for_ms,
        grace_ms,
        "GPU pressure stayed unknown past its grace; reclaiming running work"
    );
    episode.refusal.clone().unknown_for(unknown_for_ms)
}

/// When `id` was last observed, or `now_ms` if never.
fn last_observed(last: &BTreeMap<String, u64>, id: &str, now_ms: u64) -> i64 {
    last.get(id).copied().unwrap_or(now_ms) as i64
}

/// How long an episode's reading has been unknown at `now_ms`.
fn unknown_for(episode: &UnknownEpisode, now_ms: u64) -> u64 {
    (now_ms as i64).saturating_sub(episode.since_ms).max(0) as u64
}

/// Start an unknown episode, dated `since_ms`, for a resource whose reading
/// had admitted work (`began`, from [`mark_unknown`]). An episode already
/// under way keeps its start and first reason.
fn begin_unknown(
    episodes: &mut BTreeMap<String, UnknownEpisode>,
    began: Option<RefusedResource>,
    since_ms: i64,
) {
    if let Some(refusal) = began {
        episodes
            .entry(refusal.resource.clone())
            .or_insert(UnknownEpisode {
                since_ms,
                refusal,
                reclaimed: false,
            });
    }
}

/// A retained clear verdict with no prior read error may have admitted work.
/// This remains true after the observation ages stale: staleness is computed
/// at read time and has no independent rising-edge callback to trigger yield.
fn pressure_was_clear(
    view: &crate::state::ObservationView<crate::plugin::pressure::PressureVerdict>,
) -> bool {
    view.last_error.is_none()
        && view
            .value
            .as_ref()
            .is_some_and(|v| v.calibrated && !v.engaged)
}

/// Whether no fresh presence reading is in force, or the backend reports an
/// unknown session with ambiguous authority. No pressure sample can be
/// assigned to an attendance profile while this holds. A fresh unknown
/// signal from a platform with no presence backend is handled separately.
fn presence_unresolved(state: &AppState) -> bool {
    use crate::plugin::presence::{SessionAuthority, SessionState};
    if state.stale_presence().is_some() {
        return true;
    }
    let presence = state.presence.read().unwrap();
    presence.session == SessionState::Unknown
        && presence.session_authority == SessionAuthority::Ambiguous
}

/// A slow subprocess batch cannot be assigned to a profile captured before
/// it ran if the observed attendance changed while it was being collected.
fn sample_profile(
    before: crate::plugin::presence::SessionState,
    after: crate::plugin::presence::SessionState,
) -> Option<bool> {
    use crate::plugin::presence::SessionState::Unknown;
    (before != Unknown && after != Unknown && is_unattended(before) == is_unattended(after))
        .then(|| is_unattended(after))
}

/// GPU resources that some upstream can actually lend.
///
/// A configured card that no upstream names cannot be taken by a borrower, so
/// load on it is not a reason to stop lending anything. On a desktop that card
/// is also the one driving the monitors, so it is *never* quiet — and because
/// pressure is evaluated per resource, so an unoffered display card needs no
/// tracker of its own. It still participates in identity correlation above;
/// discovery and lending declaration are separate concerns.
pub fn lendable_gpu_ids(cfg: &crate::config::Config) -> Vec<String> {
    let named: HashSet<&str> = cfg
        .upstreams
        .iter()
        .flat_map(|u| {
            std::iter::once(u.resource.as_str()).chain(u.also_requires.iter().map(String::as_str))
        })
        .collect();
    cfg.resources
        .iter()
        .filter(|r| r.kind == "nvidia-gpu" && named.contains(r.id.as_str()))
        .map(|r| r.id.clone())
        .collect()
}

/// Whether a session state means nobody is at the machine.
pub fn is_unattended(s: crate::plugin::presence::SessionState) -> bool {
    use crate::plugin::presence::SessionState as S;
    matches!(s, S::Locked | S::LoggedOut | S::Disconnected)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::plugin::presence::SessionState as S;
    use crate::state::ObservationQuality;

    fn inventory(errors: &[&str]) -> anyhow::Result<gpu_id::AdapterInventory> {
        Ok(gpu_id::AdapterInventory {
            adapters: Vec::new(),
            not_pci: vec!["0x0_0x9".into()],
            errors: errors.iter().map(|e| e.to_string()).collect(),
        })
    }

    #[test]
    fn live_65_an_unchanged_adapter_problem_is_logged_once() {
        let mut last = AdapterProblems::None;
        let mut logged = Vec::new();
        for read in [
            inventory(&[]),
            inventory(&[]),
            inventory(&["LUID a: invalid PCI bus 300"]),
            inventory(&["LUID a: invalid PCI bus 300"]),
            inventory(&["LUID a: invalid PCI bus 300"]),
            inventory(&["LUID a: invalid PCI bus 300", "LUID b (NTSTATUS 0x1)"]),
            Err(anyhow::anyhow!("D3DKMTEnumAdapters2 failed")),
            Err(anyhow::anyhow!("D3DKMTEnumAdapters2 failed")),
            inventory(&[]),
            inventory(&[]),
        ] {
            let now = AdapterProblems::of(&read);
            logged.push(format!("{:?}", adapter_problem_change(&last, &now)));
            last = now;
        }
        assert_eq!(
            logged,
            [
                "None",
                "None",
                r#"Some(Partial(["LUID a: invalid PCI bus 300"]))"#,
                "None",
                "None",
                r#"Some(Partial(["LUID a: invalid PCI bus 300", "LUID b (NTSTATUS 0x1)"]))"#,
                r#"Some(Unavailable("D3DKMTEnumAdapters2 failed"))"#,
                "None",
                "Some(Recovered)",
                "None",
            ]
        );
    }

    #[test]
    fn non_pci_adapters_alone_are_not_a_problem() {
        assert_eq!(AdapterProblems::of(&inventory(&[])), AdapterProblems::None);
    }

    /// Mirrors the reference machine: two cards, only one of which anything
    /// lends. The other drives the monitors.
    const TWO_CARDS: &str = r#"
[server]
listen = "0.0.0.0:9811"

[[resource]]
id = "gpu0"
kind = "nvidia-gpu"
selector = "GPU-aaaa"

[[resource]]
id = "gpu1"
kind = "nvidia-gpu"
selector = "GPU-bbbb"

[[resource]]
id = "mem"
kind = "system-memory"
reserve = 1024

[[upstream]]
id = "ollama"
listen = "0.0.0.0:11435"
target = "http://127.0.0.1:11434"
kind = "ollama"
resource = "gpu0"
also_requires = ["mem"]
"#;

    #[test]
    fn a_card_nobody_lends_is_not_watched() {
        let cfg: crate::config::Config = toml::from_str(TWO_CARDS).unwrap();
        assert_eq!(lendable_gpu_ids(&cfg), vec!["gpu0".to_string()]);
    }

    #[test]
    fn non_gpu_resources_named_by_upstreams_are_not_watched() {
        // `mem` is in also_requires, but this signal is about GPUs.
        let cfg: crate::config::Config = toml::from_str(TWO_CARDS).unwrap();
        assert!(!lendable_gpu_ids(&cfg).contains(&"mem".to_string()));
    }

    #[test]
    fn a_second_lent_card_is_watched() {
        let cfg: crate::config::Config = toml::from_str(&format!(
            "{TWO_CARDS}\n[[upstream]]\nid = \"other\"\nlisten = \"0.0.0.0:9000\"\n\
             target = \"http://127.0.0.1:9001\"\nkind = \"openai\"\nresource = \"gpu1\"\n"
        ))
        .unwrap();
        assert_eq!(
            lendable_gpu_ids(&cfg),
            vec!["gpu0".to_string(), "gpu1".to_string()]
        );
    }

    #[test]
    fn exact_adapter_join_is_injective_and_order_independent() {
        let pci_a = "0000:01:00.0".parse().unwrap();
        let pci_b = "0000:0e:00.0".parse().unwrap();
        let resources = vec![
            ("gpu-a".into(), pci_a, AdapterMappingSource::Probe),
            ("gpu-b".into(), pci_b, AdapterMappingSource::Config),
        ];
        let adapters = vec![
            WindowsAdapter {
                luid: "LUID-B".into(),
                pci: pci_b,
            },
            WindowsAdapter {
                luid: "LUID-A".into(),
                pci: pci_a,
            },
        ];

        assert_eq!(
            join_adapters(&resources, &adapters)["gpu-a"],
            MappingAttempt::Mapped(AdapterMapping {
                pci: pci_a,
                luid: "luid-a".into(),
                source: AdapterMappingSource::Probe,
            })
        );
        assert_eq!(
            join_adapters(&resources, &adapters)["gpu-b"],
            MappingAttempt::Mapped(AdapterMapping {
                pci: pci_b,
                luid: "luid-b".into(),
                source: AdapterMappingSource::Config,
            })
        );
    }

    #[test]
    fn adapter_join_refuses_duplicate_claims_and_inventory_rows() {
        let pci: PciAddress = "0000:01:00.0".parse().unwrap();
        let duplicate_claims = vec![
            ("gpu-a".into(), pci, AdapterMappingSource::Probe),
            ("gpu-b".into(), pci, AdapterMappingSource::Probe),
        ];
        let one_adapter = vec![WindowsAdapter {
            luid: "luid-a".into(),
            pci,
        }];
        assert!(matches!(
            join_adapters(&duplicate_claims, &one_adapter)["gpu-a"],
            MappingAttempt::Unmapped(_)
        ));

        let one_claim = vec![("gpu-a".into(), pci, AdapterMappingSource::Probe)];
        let duplicate_adapters = vec![
            WindowsAdapter {
                luid: "luid-a".into(),
                pci,
            },
            WindowsAdapter {
                luid: "luid-b".into(),
                pci,
            },
        ];
        assert!(matches!(
            join_adapters(&one_claim, &duplicate_adapters)["gpu-a"],
            MappingAttempt::Unmapped(_)
        ));
    }

    #[test]
    fn a_presence_reading_past_its_freshness_bound_suspends_pressure_sampling() {
        // An old attended or unattended reading must not decide which profile
        // a GPU sample teaches, or whether pressure is engaged.
        let state = two_lent_gpus();
        state.mark_presence_sampled();
        *state.presence.write().unwrap() = crate::plugin::presence::PresenceSignal {
            session: crate::plugin::presence::SessionState::LoggedOut,
            ..Default::default()
        };
        assert!(!presence_unresolved(&state));
        let too_old = state.presence_fresh_for() + std::time::Duration::from_secs(1);
        *state.presence_sampled_at.write().unwrap() =
            Some(std::time::Instant::now().checked_sub(too_old).unwrap());
        assert!(presence_unresolved(&state));
    }

    #[test]
    fn attendance_follows_session_state() {
        assert!(is_unattended(S::Locked));
        assert!(is_unattended(S::LoggedOut));
        assert!(is_unattended(S::Disconnected));
        assert!(!is_unattended(S::Active));
        // Unknown is not unattended: it must never be resolved in favour of
        // lending, and it is the state a failed session read produces.
        assert!(!is_unattended(S::Unknown));
    }

    #[test]
    fn batches_crossing_attendance_profiles_are_not_training_data() {
        assert_eq!(sample_profile(S::Active, S::Active), Some(false));
        assert_eq!(sample_profile(S::Locked, S::Disconnected), Some(true));
        assert_eq!(sample_profile(S::Active, S::Locked), None);
        assert_eq!(sample_profile(S::Locked, S::Active), None);
        assert_eq!(sample_profile(S::Unknown, S::Active), None);
        assert_eq!(sample_profile(S::Locked, S::Unknown), None);
    }

    fn two_lent_gpus() -> AppState {
        let mut cfg: crate::config::Config = toml::from_str(&format!(
            "{TWO_CARDS}\n[[upstream]]\nid = \"other\"\nlisten = \"0.0.0.0:9000\"\n\
             target = \"http://127.0.0.1:9001\"\nkind = \"openai\"\nresource = \"gpu1\"\n"
        ))
        .unwrap();
        cfg.owner.gpu_pressure.warmup_samples = 1;
        AppState::new(cfg).unwrap()
    }

    fn ids(refused: Vec<RefusedResource>) -> Vec<String> {
        refused
            .into_iter()
            .map(|refusal| refusal.resource)
            .collect()
    }

    fn grace_ms(state: &AppState) -> u64 {
        state
            .config
            .owner
            .gpu_pressure
            .tracker_config(state.config.owner.release_after_s)
            .max_gap_ms
    }

    fn engine_for(state: &AppState) -> PressureEngine {
        let config_path = std::env::temp_dir()
            .join(format!("vacancyd-pressure-test-{}", std::process::id()))
            .join("config.toml");
        state.resources["gpu0"]
            .adapter_mapping
            .write()
            .unwrap()
            .record_success(AdapterMapping {
                pci: "0000:01:00.0".parse().unwrap(),
                luid: "0x00000000_0x000000a0".into(),
                source: AdapterMappingSource::Probe,
            });
        state.resources["gpu1"]
            .adapter_mapping
            .write()
            .unwrap()
            .record_success(AdapterMapping {
                pci: "0000:02:00.0".parse().unwrap(),
                luid: "0x00000000_0x000000b0".into(),
                source: AdapterMappingSource::Probe,
            });
        PressureEngine::new(state, &config_path)
    }

    /// What an earlier run saved for gpu0: both profiles learned.
    fn saved_gpu0() -> StoredCalibration {
        use crate::plugin::pressure::{LearnedProfile, PressureSnapshot};
        let learned = |bin: u16, threshold: f64| LearnedProfile {
            bins: BTreeMap::from([(bin, 8.0)]),
            samples: 8,
            observed_ms: 35_000,
            threshold_in_force: Some(threshold),
            legacy_baseline_pct: None,
            migrated: false,
        };
        StoredCalibration {
            identity: "GPU-aaaa".into(),
            ignored_processes: vec!["Wallpaper".into()],
            learned: PressureSnapshot {
                attended: learned(24, 32.5),
                unattended: learned(4, 27.0),
            },
        }
    }

    #[test]
    fn restore_requires_matching_hardware_and_ignore_list_only() {
        for change in [
            "none",
            "identity",
            "legacy-key",
            "legacy-key-pci",
            "legacy-key-identity",
            "ignore",
            "tuning",
            "borrowers",
            "case",
            "cap",
            "unfinished",
            "invalid",
            "legacy",
        ] {
            let mut config = two_lent_gpus().config.clone();
            config.owner.gpu_pressure.warmup_samples = 4;
            let state = AppState::new(config).unwrap();
            let mut engine = engine_for(&state);
            let mut saved = saved_gpu0();
            if change == "identity" {
                saved.identity = "GPU-old".into();
            }
            // Written before the PCI address left the key: at the address
            // the card has now, at another one, and for another card.
            if change == "legacy-key" {
                saved.identity = "GPU-aaaa@0000:01:00.0".into();
            }
            if change == "legacy-key-pci" {
                saved.identity = "GPU-aaaa@0000:03:00.0".into();
            }
            if change == "legacy-key-identity" {
                saved.identity = "GPU-old@0000:01:00.0".into();
            }
            if change == "ignore" {
                saved.ignored_processes.push("changed-app".into());
            }
            // What changed in the running configuration since the save.
            engine.cfg.ignore = vec!["wallpaper".into()];
            if change == "case" {
                engine.cfg.ignore = vec!["WALLPAPER".into(), "".into(), "wallpaper".into()];
            }
            if change == "tuning" {
                // Applied to the histogram, never part of what it measured.
                engine.cfg.margin_pct += 5.0;
                engine.cfg.floor_pct += 5.0;
                engine.cfg.sustain_s += 5;
                engine.cfg.learn_quantile = 0.5;
                engine.cfg.learn_half_life_h = 12.0;
                engine.cfg.raise_pct_per_h = 3.0;
            }
            if change == "borrowers" {
                engine.target_ports.push(1234);
            }
            if change == "cap" {
                // Below the saved attended threshold: it is held at the cap.
                engine.cfg.cap_pct = 30.0;
            }
            if change == "unfinished" {
                // Saved during a first calibration, before either profile
                // completed.
                saved.learned = Default::default();
            }
            if change == "invalid" {
                saved.learned.attended.threshold_in_force = Some(f64::NAN);
                saved.learned.unattended = Default::default();
            }
            if change == "legacy" {
                // Read from a schema 3 cache.
                saved.learned.unattended = crate::plugin::pressure::LearnedProfile {
                    legacy_baseline_pct: Some(2.0),
                    ..Default::default()
                };
            }
            engine.store.devices.insert("gpu0".into(), saved);
            let sample = batch(&["0x00000000_0x000000a0", "0x00000000_0x000000b0"], vec![]);
            let log = crate::test_log::capture();
            engine.apply_batch(&state, &sample, &Default::default(), false, 0);
            let logged = log.contents();
            drop(log);
            let verdict = state.pressure_view("gpu0").unwrap().value.unwrap();
            let restored = !matches!(
                change,
                "identity" | "legacy-key-identity" | "ignore" | "unfinished" | "invalid"
            );
            assert_eq!(verdict.calibrated, restored, "{change}");
            let expected = match change {
                "identity" | "legacy-key-identity" => "reason=\"GPU identity mismatch\"",
                "ignore" => "reason=\"ignore list changed\"",
                "unfinished" => "reason=\"no completed calibration saved\"",
                "invalid" => "reason=\"saved calibration invalid\"",
                "legacy" => "migrated_legacy_baseline=\"unattended\"",
                _ => "restored validated GPU calibration",
            };
            let gpu0 = logged
                .lines()
                .find(|line| line.contains("resource=\"gpu0\""))
                .unwrap_or_default();
            assert!(gpu0.contains(expected), "{change}: {logged}");
            if restored && change != "legacy" {
                assert!(!gpu0.contains("migrated_legacy_baseline"), "{gpu0}");
            }
            // gpu1 has no saved entry, and says so.
            assert!(
                logged.contains("resource=\"gpu1\" reason=\"no saved entry\""),
                "{change}: {logged}"
            );
            if !restored {
                continue;
            }
            // Saved again under the UUID alone, whatever form it was read in.
            assert_eq!(
                engine.store.devices["gpu0"].identity, "GPU-aaaa",
                "{change}"
            );
            // Eight saved readings and this one: the histogram is warm.
            assert_eq!(verdict.learned_samples, 9, "{change}");
            assert_eq!(
                verdict.threshold_pct,
                if change == "cap" { 30.0 } else { 32.5 },
                "{change}"
            );
            // The other profile survives an observation of this one, and a
            // migrated baseline is saved as the threshold it became.
            let unattended = &engine.store.devices["gpu0"].learned.unattended;
            if change == "legacy" {
                assert_eq!(unattended.threshold_in_force, Some(25.0), "2 + 20, floored");
                assert_eq!(unattended.legacy_baseline_pct, None);
                assert!(unattended.bins.is_empty(), "no samples are fabricated");
            } else {
                assert_eq!(unattended.samples, 8, "{change}");
            }
        }
    }

    #[test]
    fn calibration_progress_blocks_admission_until_ready() {
        let mut cfg = two_lent_gpus().config.clone();
        cfg.owner.gpu_pressure.warmup_samples = 4;
        let state = AppState::new(cfg).unwrap();
        state.mark_presence_sampled();
        let mut engine = engine_for(&state);
        let sample = batch(&["0x00000000_0x000000a0", "0x00000000_0x000000b0"], vec![]);
        for resource in state.resources.values() {
            resource
                .capacity
                .write()
                .unwrap()
                .record_success(crate::plugin::probe::Capacity {
                    free_bytes: 64 * 1024 * 1024 * 1024,
                    ..Default::default()
                });
        }
        engine.apply_batch(&state, &sample, &Default::default(), false, 0);
        let refusal = crate::gate::admit(&state, &state.config.upstreams[0], None).unwrap_err();
        assert_eq!(refusal.resource.as_deref(), Some("gpu0"));
        assert!(refusal.detail.contains("calibration is not ready (1/4)"));
        assert!(state.metrics.render().contains(
            "vacancyd_resource_calibration_samples{profile=\"attended\",resource=\"gpu0\"} 1"
        ));
        for now in [5000, 10000, 15000] {
            engine.apply_batch(&state, &sample, &Default::default(), false, now);
        }
        assert!(crate::gate::admit(&state, &state.config.upstreams[0], None).is_ok());
    }

    fn batch(
        observed: &[&str],
        processes: Vec<gpu_procs::ProcGpuSample>,
    ) -> gpu_procs::ProcGpuBatch {
        gpu_procs::ProcGpuBatch {
            processes,
            observed_luids: observed.iter().map(|luid| (*luid).to_string()).collect(),
        }
    }

    #[test]
    fn a_partial_batch_invalidates_only_the_missing_adapter() {
        let state = two_lent_gpus();
        let mut engine = engine_for(&state);
        let processes = listeners::ProcessTable::default();
        let gpu0 = "0x00000000_0x000000a0";
        let gpu1 = "0x00000000_0x000000b0";

        let complete = batch(&[gpu0, gpu1], Vec::new());
        assert!(engine
            .apply_batch(&state, &complete, &processes, false, 0)
            .is_empty());
        assert_eq!(
            state.pressure_view("gpu0").unwrap().quality,
            ObservationQuality::Ready
        );
        assert_eq!(
            state.pressure_view("gpu1").unwrap().quality,
            ObservationQuality::Ready
        );

        let partial = batch(&[gpu0], Vec::new());
        assert!(engine
            .apply_batch(&state, &partial, &processes, false, 5_000)
            .is_empty());
        assert_eq!(ids(engine.overdue_unknown(&state, 20_000)), vec!["gpu1"]);
        assert_eq!(
            state.pressure_view("gpu0").unwrap().quality,
            ObservationQuality::Ready
        );
        let missing = state.pressure_view("gpu1").unwrap();
        assert_eq!(missing.quality, ObservationQuality::Unknown);
        assert!(missing.last_error.as_deref().unwrap().contains("absent"));
    }

    #[test]
    fn an_unresolved_presence_read_keeps_the_last_pressure_verdict() {
        use crate::plugin::presence::{PresenceSignal, SessionAuthority};
        let state = two_lent_gpus();
        // A fresh presence sample, so each step below is judged by the
        // session it describes rather than by its age.
        state.mark_presence_sampled();
        let mut engine = engine_for(&state);
        let processes = listeners::ProcessTable::default();
        let quiet = batch(
            &["0x00000000_0x000000a0", "0x00000000_0x000000b0"],
            Vec::new(),
        );
        engine.apply_batch(&state, &quiet, &processes, false, 0);
        // One unconfirmed read inside a sign-out: the veto refuses, and the
        // pressure verdict is neither resampled nor thrown away.
        *state.presence.write().unwrap() = PresenceSignal {
            session: S::Unknown,
            session_authority: SessionAuthority::Ambiguous,
            ..PresenceSignal::default()
        };
        assert!(engine.poll(&state).is_empty());
        for id in ["gpu0", "gpu1"] {
            assert_eq!(
                state.pressure_view(id).unwrap().quality,
                ObservationQuality::Ready
            );
        }
        // A kept verdict that outlives its freshness becomes unknown, once,
        // so work admitted on it is reclaimed if it stays unknown: the veto
        // does not reclaim a resource-scoped upstream, and staleness alone
        // has no rising edge.
        let kept = state.pressure_view("gpu0").unwrap().value.unwrap();
        state.resources["gpu0"]
            .pressure
            .write()
            .unwrap()
            .record_success_at(
                kept,
                Instant::now() - state.pressure_fresh_for() - std::time::Duration::from_secs(1),
            );
        // Its grace counts from the last good sample, 16 s ago, so the
        // reclaim is due 4 s later rather than a whole gap limit later.
        assert!(engine.poll(&state).is_empty());
        engine.started -= std::time::Duration::from_secs(5);
        let overdue = engine.poll(&state);
        assert_eq!(ids(overdue.clone()), vec!["gpu0"]);
        let crate::reclaim_log::RefusalCause::Unknown { unknown_for_ms, .. } = overdue[0].cause
        else {
            panic!("{overdue:?}");
        };
        let stale_for = state.pressure_fresh_for().as_millis() as u64 + 6_000;
        assert!(
            (stale_for..stale_for + 1_000).contains(&unknown_for_ms.unwrap()),
            "{overdue:?}"
        );
        let expired = state.pressure_view("gpu0").unwrap();
        assert_eq!(expired.quality, ObservationQuality::Unknown);
        assert!(expired
            .last_error
            .as_deref()
            .unwrap()
            .contains("presence is unresolved"));
        assert_eq!(
            state.pressure_view("gpu1").unwrap().quality,
            ObservationQuality::Ready
        );
        assert!(engine.poll(&state).is_empty());
        // A platform with no presence backend has no attendance profile.
        state.presence.write().unwrap().session_authority = SessionAuthority::NotRequired;
        assert!(engine.poll(&state).is_empty());
        for id in ["gpu0", "gpu1"] {
            let view = state.pressure_view(id).unwrap();
            assert_eq!(view.quality, ObservationQuality::Unknown);
            assert!(view
                .last_error
                .as_deref()
                .unwrap()
                .contains("attendance profile is unknown"));
        }
        // Nothing can be sampled, yet gpu1's grace still runs out on a poll.
        engine.started -= std::time::Duration::from_millis(grace_ms(&state));
        assert_eq!(ids(engine.poll(&state)), vec!["gpu1"]);
        assert!(engine.poll(&state).is_empty());
    }

    #[test]
    fn a_mapping_failure_invalidates_previously_clear_pressure() {
        let state = two_lent_gpus();
        let mut engine = engine_for(&state);
        let processes = listeners::ProcessTable::default();
        let batch = batch(
            &["0x00000000_0x000000a0", "0x00000000_0x000000b0"],
            Vec::new(),
        );
        engine.apply_batch(&state, &batch, &processes, false, 0);
        state.resources["gpu0"]
            .adapter_mapping
            .write()
            .unwrap()
            .record_failure("duplicate PCI identity");

        assert!(engine
            .apply_batch(&state, &batch, &processes, false, 5_000)
            .is_empty());
        assert_eq!(ids(engine.overdue_unknown(&state, 20_000)), vec!["gpu0"]);
        let pressure = state.pressure_view("gpu0").unwrap();
        assert_eq!(pressure.quality, ObservationQuality::Unknown);
        assert!(pressure
            .last_error
            .as_deref()
            .unwrap()
            .contains("duplicate PCI identity"));
        assert_eq!(
            state.pressure_view("gpu1").unwrap().quality,
            ObservationQuality::Ready
        );
    }

    #[test]
    fn pressure_engages_and_yields_per_adapter() {
        let state = two_lent_gpus();
        let mut engine = engine_for(&state);
        let processes = listeners::ProcessTable::default();
        let gpu0 = "0x00000000_0x000000a0";
        let gpu1 = "0x00000000_0x000000b0";
        let quiet = batch(&[gpu0, gpu1], Vec::new());
        engine.apply_batch(&state, &quiet, &processes, false, 0);

        let busy = batch(
            &[gpu0, gpu1],
            vec![gpu_procs::ProcGpuSample {
                pid: 42,
                luid: gpu0.to_string(),
                util_pct: 95.0,
            }],
        );
        for now_ms in [5_000, 10_000, 15_000] {
            assert!(engine
                .apply_batch(&state, &busy, &processes, false, now_ms)
                .is_empty());
        }
        assert_eq!(
            ids(engine.apply_batch(&state, &busy, &processes, false, 20_000)),
            vec!["gpu0"]
        );
        assert!(state.pressure_view("gpu0").unwrap().value.unwrap().engaged);
        assert!(!state.pressure_view("gpu1").unwrap().value.unwrap().engaged);
    }

    #[test]
    fn a_listener_descendant_is_subtracted_from_pressure() {
        let state = two_lent_gpus();
        let mut engine = engine_for(&state);
        let processes = listeners::ProcessTable::from_entries(&[
            (10, 100, None, "ollama"),
            (11, 101, Some(10), "llama-server"),
        ]);
        engine.lender_processes = listeners::lender_processes(&HashSet::from([10]), &processes);
        let busy_child = batch(
            &["0x00000000_0x000000a0", "0x00000000_0x000000b0"],
            vec![gpu_procs::ProcGpuSample {
                pid: 11,
                luid: "0x00000000_0x000000a0".into(),
                util_pct: 95.0,
            }],
        );

        for now_ms in [0, 5_000, 10_000, 15_000, 20_000] {
            assert!(engine
                .apply_batch(&state, &busy_child, &processes, false, now_ms)
                .is_empty());
        }
        let verdict = state.pressure_view("gpu0").unwrap().value.unwrap();
        assert_eq!(verdict.non_lender_pct, 0.0);
        assert!(!verdict.engaged);
    }

    /// gpu0 lent to `ollama`, which has borrowed work running, with its
    /// target served by the relay process 10. Process 30 is outside that
    /// tree: a virtual machine doing the relayed work, or the owner.
    fn running_borrowed_work() -> (
        std::sync::Arc<AppState>,
        PressureEngine,
        crate::admission::Ticket,
    ) {
        let state = two_lent_gpus();
        admitting(&state);
        state.upstreams["ollama"]
            .models
            .write()
            .unwrap()
            .record_success(crate::lifecycle::Models::default());
        let state = std::sync::Arc::new(state);
        let mut engine = engine_for(&state);
        let quiet = batch(&[GPU0, GPU1], Vec::new());
        engine.apply_batch(&state, &quiet, &Default::default(), false, 0);
        let processes = relay_processes();
        engine.upstream_lenders = Some(lenders_by_upstream(
            &engine.upstream_ids,
            vec![Some(HashSet::from([10])), Some(HashSet::new())],
            &processes,
        ));
        engine.lender_processes = engine.upstream_lenders.as_ref().unwrap()["ollama"].clone();
        let ticket = crate::admission::reserve_for(
            &state,
            &state.config.upstreams[0],
            crate::gate::Demand::default(),
            crate::plugin::adapter::Completion::Tracked,
            None,
        )
        .unwrap();
        (state, engine, ticket)
    }

    fn relay_processes() -> listeners::ProcessTable {
        listeners::ProcessTable::from_entries(&[(10, 100, None, "relay"), (30, 300, None, "vm")])
    }

    /// One sample in which process `pid` uses `pct` percent of gpu0.
    fn gpu0_used_by(pid: u32, pct: f64) -> gpu_procs::ProcGpuBatch {
        batch(
            &[GPU0, GPU1],
            vec![gpu_procs::ProcGpuSample {
                pid,
                luid: GPU0.into(),
                util_pct: pct,
            }],
        )
    }

    fn reported(state: &AppState, upstream: &str) -> Option<Vec<String>> {
        crate::api::status_body(state)
            .upstreams
            .into_iter()
            .find(|u| u.id == upstream)
            .unwrap()
            .unseen_gpu_work
            .map(|unseen| unseen.resources)
    }

    #[test]
    fn borrowed_work_outside_the_serving_tree_is_reported_and_still_counted_as_owner_load() {
        let (state, mut engine, _work) = running_borrowed_work();
        let processes = relay_processes();
        let relayed = gpu0_used_by(30, 90.0);

        for now_ms in [5_000, 10_000] {
            engine.apply_batch(&state, &relayed, &processes, false, now_ms);
            assert_eq!(reported(&state, "ollama"), None, "at {now_ms} ms");
        }
        engine.apply_batch(&state, &relayed, &processes, false, 15_000);

        assert_eq!(reported(&state, "ollama"), Some(vec!["gpu0".into()]));
        assert_eq!(reported(&state, "other"), None);
        // Owner protection is unchanged: the unseen work still counts.
        let verdict = state.pressure_view("gpu0").unwrap().value.unwrap();
        assert_eq!(verdict.non_lender_pct, 90.0);
    }

    #[test]
    fn borrowed_work_the_serving_tree_does_is_never_reported() {
        let (state, mut engine, _work) = running_borrowed_work();
        let processes = relay_processes();
        let native = gpu0_used_by(10, 90.0);

        for now_ms in [5_000, 10_000, 15_000, 20_000] {
            engine.apply_batch(&state, &native, &processes, false, now_ms);
        }

        assert_eq!(reported(&state, "ollama"), None);
        let verdict = state.pressure_view("gpu0").unwrap().value.unwrap();
        assert_eq!(verdict.non_lender_pct, 0.0);
    }

    #[test]
    fn gpu_load_without_running_borrowed_work_is_not_reported() {
        let (state, mut engine, work) = running_borrowed_work();
        drop(work);
        let processes = relay_processes();
        let owner = gpu0_used_by(30, 90.0);

        for now_ms in [5_000, 10_000, 15_000, 20_000] {
            engine.apply_batch(&state, &owner, &processes, false, now_ms);
        }

        assert_eq!(reported(&state, "ollama"), None);
    }

    #[test]
    fn an_unseen_upstream_is_cleared_once_its_serving_tree_uses_a_gpu() {
        let (state, mut engine, _work) = running_borrowed_work();
        let processes = relay_processes();
        let relayed = gpu0_used_by(30, 90.0);
        for now_ms in [5_000, 10_000, 15_000] {
            engine.apply_batch(&state, &relayed, &processes, false, now_ms);
        }
        assert!(reported(&state, "ollama").is_some());

        engine.apply_batch(&state, &gpu0_used_by(10, 40.0), &processes, false, 20_000);

        assert_eq!(reported(&state, "ollama"), None);
    }

    #[test]
    fn nothing_is_judged_while_lender_discovery_has_failed() {
        let (state, mut engine, _work) = running_borrowed_work();
        engine.upstream_lenders = None;
        let processes = relay_processes();
        let relayed = gpu0_used_by(30, 90.0);

        for now_ms in [5_000, 10_000, 15_000, 20_000] {
            engine.apply_batch(&state, &relayed, &processes, false, now_ms);
        }

        assert_eq!(reported(&state, "ollama"), None);
    }

    #[test]
    fn material_process_discovery_is_start_time_qualified() {
        let state = two_lent_gpus();
        let mut engine = engine_for(&state);
        let busy = batch(
            &["0x00000000_0x000000a0"],
            vec![gpu_procs::ProcGpuSample {
                pid: 11,
                luid: "0x00000000_0x000000a0".into(),
                util_pct: MATERIAL_PROCESS_PCT,
            }],
        );
        let first = listeners::ProcessTable::from_entries(&[(11, 101, None, "runner")]);
        assert!(engine.new_material_process(&busy, &first));
        assert!(!engine.new_material_process(&busy, &first));

        let reused = listeners::ProcessTable::from_entries(&[(11, 202, None, "game")]);
        assert!(engine.new_material_process(&busy, &reused));
    }

    #[test]
    fn a_failure_yields_work_admitted_by_a_now_stale_clear_sample_once() {
        let state = two_lent_gpus();
        state.resources["gpu0"]
            .pressure
            .write()
            .unwrap()
            .record_success_at(
                crate::plugin::pressure::PressureVerdict {
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
                Instant::now() - state.pressure_fresh_for() - std::time::Duration::from_secs(1),
            );
        assert_eq!(
            state.pressure_view("gpu0").unwrap().quality,
            ObservationQuality::Stale
        );

        let tracker = PressureTracker::new(
            state
                .config
                .owner
                .gpu_pressure
                .tracker_config(state.config.owner.release_after_s),
        );
        assert_eq!(
            mark_unknown(&state, "gpu0", &tracker, "counter failed"),
            Some(RefusedResource::unknown("gpu0", "stale", "counter failed"))
        );
        assert_eq!(
            mark_unknown(&state, "gpu0", &tracker, "counter still failed"),
            None
        );
    }

    #[test]
    fn a_saved_calibration_is_restored_only_when_every_key_matches() {
        let saved = StoredCalibration {
            ignored_processes: vec!["wallpaper".into()],
            ..saved_gpu0()
        };
        let check = |saved: Option<&StoredCalibration>, uuid: bool, mode: PressureMode| {
            saved_calibration(saved, uuid, "GPU-aaaa", mode, &["wallpaper".into()]).map(|_| ())
        };
        let calibrated = PressureMode::Calibrated;
        assert_eq!(check(Some(&saved), true, calibrated), Ok(()));
        assert_eq!(
            check(Some(&saved), false, calibrated),
            Err(NotRestored::IndexSelector)
        );
        assert_eq!(check(None, true, calibrated), Err(NotRestored::NoEntry));
        assert_eq!(
            check(Some(&saved), true, PressureMode::Fixed),
            Err(NotRestored::FixedMode)
        );
        // Fixed mode only claims to keep a calibration that exists.
        assert_eq!(
            check(None, true, PressureMode::Fixed),
            Err(NotRestored::NoEntry)
        );
        let mut other = saved.clone();
        other.identity = "GPU-aaab".into();
        assert_eq!(
            check(Some(&other), true, calibrated),
            Err(NotRestored::IdentityMismatch)
        );
        // An older cache's `<uuid>@<pci>` key matches the UUID at any address.
        other.identity = "GPU-aaaa@0000:02:00.0".into();
        assert_eq!(check(Some(&other), true, calibrated), Ok(()));
        other.identity = "GPU-aaab@0000:01:00.0".into();
        assert_eq!(
            check(Some(&other), true, calibrated),
            Err(NotRestored::IdentityMismatch)
        );
        let mut other = saved;
        other.ignored_processes.clear();
        assert_eq!(
            check(Some(&other), true, calibrated),
            Err(NotRestored::IgnoreListChanged)
        );
    }

    #[test]
    fn fixed_mode_keeps_the_saved_calibration_for_a_switch_back() {
        let mut config = two_lent_gpus().config.clone();
        config.owner.gpu_pressure.mode = PressureMode::Fixed;
        config.owner.gpu_pressure.threshold_pct = 50.0;
        let state = AppState::new(config).unwrap();
        let mut engine = engine_for(&state);
        let saved = StoredCalibration {
            ignored_processes: engine.cfg.ignore.clone(),
            ..saved_gpu0()
        };
        engine.store.devices.insert("gpu0".into(), saved.clone());
        let sample = batch(&["0x00000000_0x000000a0", "0x00000000_0x000000b0"], vec![]);
        let log = crate::test_log::capture();
        engine.apply_batch(&state, &sample, &Default::default(), false, 0);
        let logged = log.contents();
        drop(log);
        assert!(
            logged.contains("resource=\"gpu0\" reason=\"fixed mode; saved calibration kept\""),
            "{logged}"
        );
        let verdict = state.pressure_view("gpu0").unwrap().value.unwrap();
        assert_eq!(verdict.threshold_pct, 50.0);
        assert_eq!(engine.store.devices["gpu0"], saved);
        assert!(!engine.store.devices.contains_key("gpu1"));
    }

    /// A scratch directory with `mode`, holding nothing yet.
    #[cfg(unix)]
    fn cache_directory_with_mode(name: &str, mode: u32) -> PathBuf {
        use std::os::unix::fs::PermissionsExt;
        let directory = std::env::temp_dir().join(format!(
            "vacancyd-cache-{name}-{}",
            crate::setup::nonce().unwrap()
        ));
        std::fs::create_dir(&directory).unwrap();
        std::fs::set_permissions(&directory, std::fs::Permissions::from_mode(mode)).unwrap();
        directory
    }

    #[cfg(unix)]
    #[test]
    fn a_configured_cache_in_a_directory_others_can_write_is_neither_read_nor_written() {
        let directory = cache_directory_with_mode("shared", 0o777);
        let cache = directory.join("pressure.json");
        // What another account could have planted: a valid saved calibration.
        let mut planted = PressureStore::default();
        planted.devices.insert("gpu0".into(), saved_gpu0());
        planted.save(&cache).unwrap();
        let planted_bytes = std::fs::read(&cache).unwrap();
        let mut config = two_lent_gpus().config.clone();
        config.owner.gpu_pressure.state_file = cache.display().to_string();
        config.owner.gpu_pressure.ignore = vec!["wallpaper".into()];
        let state = AppState::new(config).unwrap();

        let log = crate::test_log::capture();
        let mut engine = engine_for(&state);
        let sample = batch(&["0x00000000_0x000000a0", "0x00000000_0x000000b0"], vec![]);
        engine.apply_batch(&state, &sample, &Default::default(), false, 0);
        engine.save();
        let logged = log.contents();
        drop(log);

        assert!(logged.contains("pressure cache not used"), "{logged}");
        assert!(
            logged.contains("resource=\"gpu0\" reason=\"no saved entry\""),
            "{logged}"
        );
        assert_eq!(std::fs::read(&cache).unwrap(), planted_bytes);
        std::fs::remove_dir_all(directory).unwrap();
    }

    #[cfg(unix)]
    #[test]
    fn check_config_warns_only_about_a_configured_cache_others_can_write() {
        let shared = cache_directory_with_mode("check-shared", 0o777);
        let private = cache_directory_with_mode("check-private", 0o700);
        let config_path = private.join("vacancyd.toml");
        let warned = |state_file: &str| {
            let mut cfg = crate::config::GpuPressureConfig {
                state_file: state_file.into(),
                ..Default::default()
            };
            let log = crate::test_log::capture();
            warn_shared_cache_directory(&cfg, &config_path);
            cfg.enabled = false;
            warn_shared_cache_directory(&cfg, &config_path);
            log.contents()
                .matches("will not use this pressure cache")
                .count()
        };

        assert_eq!(warned(&shared.join("p.json").display().to_string()), 1);
        assert_eq!(warned(&private.join("p.json").display().to_string()), 0);
        // The service would refuse to create one there, so it is reported.
        assert_eq!(
            warned(&shared.join("new").join("p.json").display().to_string()),
            1
        );
        assert_eq!(
            warned(&private.join("new").join("p.json").display().to_string()),
            0
        );
        assert_eq!(warned(""), 0, "the default sits beside the configuration");
        std::fs::remove_dir_all(shared).unwrap();
        std::fs::remove_dir_all(private).unwrap();
    }

    #[cfg(windows)]
    #[test]
    fn a_configured_cache_outside_the_protected_folder_is_neither_created_nor_used() {
        // Private to Administrators and LocalSystem, but not inside the
        // protected folder.
        let outside = crate::secure_file::protected_directory_outside_test_root();
        for cache in [
            outside.join("pressure.json"),
            outside.join("cache").join("pressure.json"),
        ] {
            let reason = prepare_cache_directory(&cache).unwrap_err();
            assert!(reason.contains("is not inside"), "{reason}");

            let cfg = crate::config::GpuPressureConfig {
                state_file: cache.display().to_string(),
                ..Default::default()
            };
            let log = crate::test_log::capture();
            warn_shared_cache_directory(&cfg, &outside.join("vacancyd.toml"));
            let logged = log.contents();
            drop(log);
            assert!(
                logged.contains("will not use this pressure cache"),
                "{logged}"
            );
        }
        assert!(!outside.join("cache").exists());
        std::fs::remove_dir(outside).unwrap();
    }

    #[cfg(unix)]
    #[test]
    fn a_missing_configured_cache_directory_is_created_private_and_used() {
        use std::os::unix::fs::PermissionsExt;
        let parent = cache_directory_with_mode("parent", 0o700);
        let cache = parent.join("cache").join("pressure.json");
        let mut config = two_lent_gpus().config.clone();
        config.owner.gpu_pressure.state_file = cache.display().to_string();
        let state = AppState::new(config).unwrap();

        let mut engine = engine_for(&state);
        engine.save();

        let mode = std::fs::metadata(cache.parent().unwrap())
            .unwrap()
            .permissions()
            .mode();
        assert_eq!(mode & 0o777, 0o700, "mode {mode:o}");
        assert!(cache.exists());
        std::fs::remove_dir_all(parent).unwrap();
    }

    #[cfg(unix)]
    #[test]
    fn a_missing_cache_directory_under_a_parent_others_control_is_never_created() {
        // Another account could replace the new directory with a link to
        // one of the service's own before its permissions are changed.
        let parent = cache_directory_with_mode("open-parent", 0o777);
        let cache = parent.join("cache").join("pressure.json");
        let mut config = two_lent_gpus().config.clone();
        config.owner.gpu_pressure.state_file = cache.display().to_string();
        let state = AppState::new(config).unwrap();

        let log = crate::test_log::capture();
        let mut engine = engine_for(&state);
        engine.save();
        let logged = log.contents();
        drop(log);

        assert!(logged.contains("pressure cache not used"), "{logged}");
        assert!(logged.contains("writable by group or others"), "{logged}");
        assert!(!parent.join("cache").exists());
        std::fs::remove_dir_all(parent).unwrap();
    }

    #[test]
    fn a_renumbered_pci_bus_keeps_a_uuid_selected_gpus_calibration() {
        // The same card, found at a new PCI address after a restart, used
        // to restore nothing and recalibrate from zero.
        let mut config = two_lent_gpus().config.clone();
        config.owner.gpu_pressure.warmup_samples = 4;
        let state = AppState::new(config).unwrap();
        let mut engine = engine_for(&state);
        engine.cfg.ignore = vec!["wallpaper".into()];
        engine.store.devices.insert(
            "gpu0".into(),
            StoredCalibration {
                identity: "GPU-aaaa@0000:0e:00.0".into(),
                ..saved_gpu0()
            },
        );
        let sample = batch(&["0x00000000_0x000000a0", "0x00000000_0x000000b0"], vec![]);

        let log = crate::test_log::capture();
        engine.apply_batch(&state, &sample, &Default::default(), false, 0);
        let logged = log.contents();
        drop(log);

        let gpu0 = logged
            .lines()
            .find(|line| line.contains("resource=\"gpu0\""))
            .unwrap_or_default();
        assert!(
            gpu0.contains("restored validated GPU calibration"),
            "{logged}"
        );
        let verdict = state.pressure_view("gpu0").unwrap().value.unwrap();
        assert!(verdict.calibrated);
        assert_eq!(verdict.threshold_pct, 32.5);
    }

    #[test]
    fn a_pci_move_while_running_keeps_the_uuid_selected_tracker() {
        let mut config = two_lent_gpus().config.clone();
        config.owner.gpu_pressure.warmup_samples = 4;
        let state = AppState::new(config).unwrap();
        let mut engine = engine_for(&state);
        let sample = batch(&["0x00000000_0x000000a0", "0x00000000_0x000000b0"], vec![]);
        for step in 0..5u64 {
            engine.apply_batch(&state, &sample, &Default::default(), false, step * 5_000);
        }
        assert!(
            state
                .pressure_view("gpu0")
                .unwrap()
                .value
                .unwrap()
                .calibrated
        );
        state.resources["gpu0"]
            .adapter_mapping
            .write()
            .unwrap()
            .record_success(AdapterMapping {
                pci: "0000:0f:00.0".parse().unwrap(),
                luid: "0x00000000_0x000000a0".into(),
                source: AdapterMappingSource::Probe,
            });

        let log = crate::test_log::capture();
        engine.apply_batch(&state, &sample, &Default::default(), false, 25_000);
        let logged = log.contents();
        drop(log);

        assert!(!logged.contains("resource=\"gpu0\""), "{logged}");
        let verdict = state.pressure_view("gpu0").unwrap().value.unwrap();
        assert!(verdict.calibrated);
        assert_eq!(verdict.learned_samples, 6);
    }

    #[test]
    fn an_index_selector_says_its_calibration_is_never_cached() {
        let mut config = two_lent_gpus().config.clone();
        config.resources[0].selector = Some("0".into());
        let state = AppState::new(config).unwrap();
        let mut engine = engine_for(&state);
        let sample = batch(&["0x00000000_0x000000a0", "0x00000000_0x000000b0"], vec![]);
        let log = crate::test_log::capture();
        engine.apply_batch(&state, &sample, &Default::default(), false, 0);
        let logged = log.contents();
        drop(log);
        assert!(
            logged.contains("resource=\"gpu0\" reason=\"index selector not cached\""),
            "{logged}"
        );
    }

    #[test]
    fn each_sample_is_published_as_floats_kept_in_history_and_logged_at_debug() {
        let state = two_lent_gpus();
        let mut engine = engine_for(&state);
        let processes = listeners::ProcessTable::default();
        let gpu0 = "0x00000000_0x000000a0";
        let gpu1 = "0x00000000_0x000000b0";
        let busy = batch(
            &[gpu0, gpu1],
            vec![gpu_procs::ProcGpuSample {
                pid: 42,
                luid: gpu0.to_string(),
                util_pct: 12.75,
            }],
        );
        let log = crate::test_log::capture_at(tracing::level_filters::LevelFilter::DEBUG);
        engine.apply_batch(&state, &busy, &processes, true, 0);
        let logged = log.contents();
        drop(log);

        let record = logged
            .lines()
            .find(|line| line.contains("pressure sample") && line.contains("resource=\"gpu0\""))
            .unwrap_or_else(|| panic!("{logged}"));
        for field in [
            "vacancyd::pressure",
            "non_lender_pct=12.75",
            "profile=\"unattended\"",
            "threshold_pct=",
            "target_pct=",
            "baseline_pct=",
            "calibrated=true",
            "calibration_samples=1",
            "calibration_required=1",
            "engaged=false",
            "above_for_s=0",
            "mode=\"calibrated\"",
        ] {
            assert!(record.contains(field), "{field} missing from {record}");
        }

        let metrics = state.metrics.render();
        assert!(
            metrics.contains("vacancyd_resource_pressure_pct{resource=\"gpu0\"} 12.75"),
            "{metrics}"
        );
        // The learned quantile is the upper edge of 12.75's half-percent bin.
        assert!(
            metrics.contains(
                "vacancyd_resource_pressure_baseline_pct{profile=\"unattended\",resource=\"gpu0\"} 13"
            ),
            "{metrics}"
        );
        assert!(
            metrics.contains(
                "vacancyd_resource_pressure_target_pct{profile=\"unattended\",resource=\"gpu0\"} 33"
            ),
            "{metrics}"
        );
        // The threshold in force is that target: 13 + 20, above the floor.
        let threshold = state
            .pressure_view("gpu0")
            .unwrap()
            .value
            .unwrap()
            .threshold_pct;
        assert!(
            metrics.contains(&format!(
                "vacancyd_resource_pressure_threshold_pct{{profile=\"unattended\",resource=\"gpu0\"}} {threshold}"
            )),
            "{metrics}"
        );

        // The export summarises what each profile has learned.
        let export = serde_json::to_value(crate::diagnostics::collect(&state)).unwrap();
        let learned = &export["pressure_history"]["gpu0"]["learned"];
        assert_eq!(learned["unattended"]["samples"], 1);
        assert_eq!(learned["unattended"]["threshold_in_force_pct"], 33.0);
        assert_eq!(learned["unattended"]["target_pct"], 33.0);
        assert_eq!(learned["unattended"]["quantiles"][3]["q"], 0.75);
        assert_eq!(learned["unattended"]["quantiles"][3]["pct"], 13.0);
        assert_eq!(learned["attended"]["samples"], 0);

        let history = state.resources["gpu0"].pressure_history.lock().unwrap();
        let recorded = history.records();
        assert_eq!(recorded.len(), 1);
        assert!(recorded[0].at_unix_ms > 0);
        assert!(matches!(
            recorded[0].reading,
            crate::pressure_history::Reading::Ready {
                non_lender_pct: 12.75,
                profile: Profile::Unattended,
                baseline_pct: Some(13.0),
                calibrated: true,
                ..
            }
        ));
        assert!(state.resources["mem"]
            .pressure_history
            .lock()
            .unwrap()
            .records()
            .is_empty());
    }

    #[test]
    fn an_uncalibrated_profile_publishes_no_baseline() {
        let mut config = two_lent_gpus().config.clone();
        config.owner.gpu_pressure.warmup_samples = 4;
        let state = AppState::new(config).unwrap();
        let mut engine = engine_for(&state);
        let sample = batch(&["0x00000000_0x000000a0", "0x00000000_0x000000b0"], vec![]);
        engine.apply_batch(&state, &sample, &Default::default(), false, 0);
        let metrics = state.metrics.render();
        assert!(
            !metrics.contains("vacancyd_resource_pressure_baseline_pct{"),
            "{metrics}"
        );
        assert!(metrics.contains("vacancyd_resource_pressure_pct{resource=\"gpu0\"} 0"));
    }

    #[tokio::test]
    async fn a_reclaim_for_an_unknown_reading_logs_why_the_resource_was_refused() {
        let state = two_lent_gpus();
        let mut engine = engine_for(&state);
        let processes = listeners::ProcessTable::default();
        let both = batch(
            &["0x00000000_0x000000a0", "0x00000000_0x000000b0"],
            Vec::new(),
        );
        engine.apply_batch(&state, &both, &processes, false, 0);
        state.resources["gpu0"]
            .adapter_mapping
            .write()
            .unwrap()
            .record_failure("duplicate PCI identity");
        assert!(engine
            .apply_batch(&state, &both, &processes, false, 5_000)
            .is_empty());
        let refused = engine.overdue_unknown(&state, 20_000);
        assert_eq!(
            refused,
            [RefusedResource::unknown(
                "gpu0",
                "ready",
                "GPU adapter identity is unavailable: duplicate PCI identity"
            )
            .unknown_for(20_000)]
        );

        let state = std::sync::Arc::new(state);
        let log = crate::test_log::capture();
        crate::yield_control::on_resources_refused(state.clone(), refused, false).await;
        let logged = log.contents();
        drop(log);
        let line = logged
            .lines()
            .find(|line| line.contains("reclaiming borrowed work"))
            .unwrap_or_else(|| panic!("{logged}"));
        assert!(line.contains("trigger=\"resource-refused\""), "{line}");
        assert!(
            line.contains(
                "refused=\"gpu0 unknown (ready reading invalidated: GPU adapter identity is unavailable: duplicate PCI identity; for 20.0s)\""
            ),
            "{line}"
        );
        let events = state.reclaim_log.lock().unwrap().events();
        assert_eq!(events.len(), 1);
        assert_eq!(events[0].resources[0].resource, "gpu0");
        assert_eq!(
            events[0].upstreams.len(),
            0,
            "no upstream held work, so none was scheduled"
        );
    }

    #[test]
    fn a_rising_edge_says_whether_pressure_engaged_or_calibration_is_missing() {
        let state = two_lent_gpus();
        let mut engine = engine_for(&state);
        let processes = listeners::ProcessTable::default();
        let gpu0 = "0x00000000_0x000000a0";
        let gpu1 = "0x00000000_0x000000b0";
        engine.apply_batch(
            &state,
            &batch(&[gpu0, gpu1], Vec::new()),
            &processes,
            false,
            0,
        );
        let busy = batch(
            &[gpu0, gpu1],
            vec![gpu_procs::ProcGpuSample {
                pid: 42,
                luid: gpu0.to_string(),
                util_pct: 95.0,
            }],
        );
        for now_ms in [5_000, 10_000, 15_000] {
            engine.apply_batch(&state, &busy, &processes, false, now_ms);
        }
        let refused = engine.apply_batch(&state, &busy, &processes, false, 20_000);
        assert_eq!(refused.len(), 1);
        assert!(matches!(
            refused[0].cause,
            crate::reclaim_log::RefusalCause::Engaged { non_lender_pct, .. } if non_lender_pct == 95.0
        ));
    }

    #[test]
    fn both_profiles_publish_what_the_tracker_holds_and_only_the_observed_one_is_engaged() {
        let mut config = two_lent_gpus().config.clone();
        config.owner.gpu_pressure.warmup_samples = 4;
        let state = AppState::new(config).unwrap();
        let mut engine = engine_for(&state);
        let processes = listeners::ProcessTable::default();
        let quiet = batch(
            &["0x00000000_0x000000a0", "0x00000000_0x000000b0"],
            Vec::new(),
        );
        let series = |name: &str, profile: &str| {
            format!("{name}{{profile=\"{profile}\",resource=\"gpu0\"}}")
        };
        let value = |metrics: &str, name: &str, profile: &str| {
            let series = series(name, profile);
            metrics
                .lines()
                .find_map(|line| line.strip_prefix(&series))
                .map(|value| value.trim().to_owned())
        };
        engine.apply_batch(&state, &quiet, &processes, false, 0);
        engine.apply_batch(&state, &quiet, &processes, false, 5_000);
        let metrics = state.metrics.render();
        let samples = "vacancyd_resource_calibration_samples";
        assert_eq!(value(&metrics, samples, "attended").as_deref(), Some("2"));
        assert_eq!(value(&metrics, samples, "unattended").as_deref(), Some("0"));
        assert_eq!(
            value(&metrics, "vacancyd_resource_calibrated", "unattended").as_deref(),
            Some("0")
        );
        assert_eq!(
            value(&metrics, "vacancyd_resource_pressure_engaged", "attended").as_deref(),
            Some("0")
        );
        assert_eq!(
            value(&metrics, "vacancyd_resource_pressure_engaged", "unattended"),
            None
        );

        // Locking switches profile; the unfinished attended samples are
        // kept, and the attended series still says so while unobserved.
        engine.apply_batch(&state, &quiet, &processes, true, 10_000);
        let metrics = state.metrics.render();
        assert_eq!(value(&metrics, samples, "attended").as_deref(), Some("2"));
        assert_eq!(value(&metrics, samples, "unattended").as_deref(), Some("1"));
        assert_eq!(
            value(&metrics, "vacancyd_resource_pressure_engaged", "attended"),
            None
        );
        assert_eq!(
            value(&metrics, "vacancyd_resource_pressure_engaged", "unattended").as_deref(),
            Some("0")
        );
        // The floor applies to both while neither has a baseline.
        assert_eq!(
            value(
                &metrics,
                "vacancyd_resource_pressure_threshold_pct",
                "attended"
            ),
            value(
                &metrics,
                "vacancyd_resource_pressure_threshold_pct",
                "unattended"
            )
        );
        assert!(!metrics.contains("vacancyd_resource_pressure_baseline_pct{"));
    }

    #[test]
    fn an_engaged_resource_publishes_one_on_its_engaged_gauge() {
        let state = two_lent_gpus();
        let mut engine = engine_for(&state);
        let processes = listeners::ProcessTable::default();
        let gpu0 = "0x00000000_0x000000a0";
        let gpu1 = "0x00000000_0x000000b0";
        engine.apply_batch(
            &state,
            &batch(&[gpu0, gpu1], Vec::new()),
            &processes,
            false,
            0,
        );
        let busy = batch(
            &[gpu0, gpu1],
            vec![gpu_procs::ProcGpuSample {
                pid: 42,
                luid: gpu0.to_string(),
                util_pct: 95.0,
            }],
        );
        for now_ms in [5_000, 10_000, 15_000, 20_000] {
            engine.apply_batch(&state, &busy, &processes, false, now_ms);
        }
        let metrics = state.metrics.render();
        assert!(metrics.contains(
            "vacancyd_resource_pressure_engaged{profile=\"attended\",resource=\"gpu0\"} 1"
        ));
        assert!(metrics.contains(
            "vacancyd_resource_pressure_engaged{profile=\"attended\",resource=\"gpu1\"} 0"
        ));
    }

    #[test]
    fn the_busiest_owner_processes_reach_only_the_diagnostics_export() {
        let state = two_lent_gpus();
        let mut engine = engine_for(&state);
        let gpu0 = "0x00000000_0x000000a0";
        let gpu1 = "0x00000000_0x000000b0";
        let processes = listeners::ProcessTable::from_entries(&[
            (42, 1, None, "private-game"),
            (43, 2, None, "private-editor"),
        ]);
        let busy = batch(
            &[gpu0, gpu1],
            vec![
                gpu_procs::ProcGpuSample {
                    pid: 43,
                    luid: gpu0.to_string(),
                    util_pct: 7.5,
                },
                gpu_procs::ProcGpuSample {
                    pid: 42,
                    luid: gpu0.to_string(),
                    util_pct: 20.0,
                },
            ],
        );
        let log = crate::test_log::capture();
        engine.apply_batch(&state, &busy, &processes, false, 0);
        let logged = log.contents();
        drop(log);

        let top = state.resources["gpu0"]
            .pressure_history
            .lock()
            .unwrap()
            .top_processes();
        assert_eq!(
            top.iter()
                .map(|process| (process.name.as_str(), process.pct))
                .collect::<Vec<_>>(),
            [("private-game", 20.0), ("private-editor", 7.5)]
        );
        assert!(state.resources["gpu1"]
            .pressure_history
            .lock()
            .unwrap()
            .top_processes()
            .is_empty());
        let status = serde_json::to_string(&crate::api::status_body(&state)).unwrap();
        let metrics = state.metrics.render();
        for public in [&logged, &status, &metrics] {
            assert!(!public.contains("private-"), "{public}");
        }
        let export = serde_json::to_value(crate::diagnostics::collect(&state)).unwrap();
        assert_eq!(
            export["pressure_history"]["gpu0"]["top_processes"][0],
            serde_json::json!({"name": "private-game", "pct": 20.0})
        );
    }

    #[test]
    fn an_unknown_reading_is_published_and_recorded_with_its_reason() {
        let mut config = two_lent_gpus().config.clone();
        config.owner.gpu_pressure.warmup_samples = 4;
        let state = AppState::new(config).unwrap();
        let mut engine = engine_for(&state);
        let processes = listeners::ProcessTable::default();
        let gpu0 = "0x00000000_0x000000a0";
        let gpu1 = "0x00000000_0x000000b0";
        let both = batch(&[gpu0, gpu1], Vec::new());
        engine.apply_batch(&state, &both, &processes, false, 0);
        engine.apply_batch(&state, &both, &processes, false, 5_000);
        let metrics = state.metrics.render();
        assert!(metrics.contains("vacancyd_resource_pressure_pct{resource=\"gpu1\"} 0"));
        assert!(metrics.contains(
            "vacancyd_resource_pressure_engaged{profile=\"attended\",resource=\"gpu1\"} 0"
        ));
        assert!(metrics.contains(
            "vacancyd_resource_calibration_samples{profile=\"attended\",resource=\"gpu1\"} 2"
        ));

        // gpu1 drops out of the counter batch.
        engine.apply_batch(
            &state,
            &batch(&[gpu0], Vec::new()),
            &processes,
            false,
            10_000,
        );
        let metrics = state.metrics.render();
        assert!(!metrics.contains("vacancyd_resource_pressure_pct{resource=\"gpu1\"}"));
        assert!(!metrics.contains(
            "vacancyd_resource_pressure_engaged{profile=\"attended\",resource=\"gpu1\"}"
        ));
        // One missed sample does not interrupt the tracker, so unfinished
        // calibration is kept, and the gauge says so.
        assert!(metrics.contains(
            "vacancyd_resource_calibration_samples{profile=\"attended\",resource=\"gpu1\"} 2"
        ));
        // gpu0 is untouched.
        assert!(metrics.contains("vacancyd_resource_pressure_pct{resource=\"gpu0\"} 0"));

        let records = state.resources["gpu1"]
            .pressure_history
            .lock()
            .unwrap()
            .records();
        assert_eq!(records.len(), 3);
        let last = serde_json::to_value(records.last().unwrap()).unwrap();
        assert_eq!(last["quality"], "unknown");
        assert_eq!(
            last["reason"],
            "adapter was absent from the GPU counter batch"
        );
    }

    /// Record enough capacity that admission turns on pressure alone.
    fn admitting(state: &AppState) {
        state.mark_presence_sampled();
        for resource in state.resources.values() {
            resource
                .capacity
                .write()
                .unwrap()
                .record_success(crate::plugin::probe::Capacity {
                    free_bytes: 64 * 1024 * 1024 * 1024,
                    ..Default::default()
                });
        }
    }

    const GPU0: &str = "0x00000000_0x000000a0";
    const GPU1: &str = "0x00000000_0x000000b0";

    #[test]
    fn one_failed_sample_inside_the_grace_refuses_new_work_but_reclaims_nothing() {
        let state = two_lent_gpus();
        admitting(&state);
        let mut engine = engine_for(&state);
        let processes = listeners::ProcessTable::default();
        let quiet = batch(&[GPU0, GPU1], Vec::new());
        assert!(engine
            .apply_batch(&state, &quiet, &processes, false, 0)
            .is_empty());
        assert!(crate::gate::admit(&state, &state.config.upstreams[0], None).is_ok());
        engine.apply_batch(&state, &quiet, &processes, false, 4_000);

        engine.mark_all_unknown(&state, "GPU attribution sample failed: busy", 9_000);
        assert!(engine.overdue_unknown(&state, 9_000).is_empty());
        // Admission stays fail-closed: new work is refused at once.
        let refusal = crate::gate::admit(&state, &state.config.upstreams[0], None).unwrap_err();
        assert!(
            refusal.detail.contains("pressure is unknown"),
            "{refusal:?}"
        );

        // The reading recovers 8.4 seconds later, 13.4 s after the last good
        // sample: inside the grace.
        assert!(engine.overdue_unknown(&state, 17_399).is_empty());
        assert!(engine
            .apply_batch(&state, &quiet, &processes, false, 17_400)
            .is_empty());
        assert!(crate::gate::admit(&state, &state.config.upstreams[0], None).is_ok());
        // The episode is over; nothing fires later on its account.
        assert!(engine.overdue_unknown(&state, 60_000).is_empty());
    }

    #[test]
    fn an_unknown_reading_reclaims_once_it_outlasts_the_grace() {
        let state = two_lent_gpus();
        let mut engine = engine_for(&state);
        let processes = listeners::ProcessTable::default();
        let quiet = batch(&[GPU0, GPU1], Vec::new());
        engine.apply_batch(&state, &quiet, &processes, false, 0);
        let grace = grace_ms(&state);
        assert_eq!(grace, 20_000);

        // The grace counts from the last good sample, at 0.
        engine.mark_all_unknown(&state, "GPU attribution sample failed: gone", 5_000);
        assert!(engine.overdue_unknown(&state, grace - 1).is_empty());
        // Still failing: the episode keeps its start and its first reason.
        engine.mark_all_unknown(&state, "GPU attribution sample failed: again", 10_000);
        let overdue = engine.overdue_unknown(&state, grace);
        let expected: Vec<RefusedResource> = ["gpu0", "gpu1"]
            .iter()
            .map(|id| {
                RefusedResource::unknown(id, "ready", "GPU attribution sample failed: gone")
                    .unknown_for(grace)
            })
            .collect();
        assert_eq!(overdue, expected);
        assert_eq!(
            overdue[0].to_string(),
            "gpu0 unknown (ready reading invalidated: GPU attribution sample failed: gone; for 20.0s)"
        );
        // Once per episode, however long it lasts.
        engine.mark_all_unknown(&state, "GPU attribution sample failed: gone", 25_000);
        assert!(engine.overdue_unknown(&state, 60_000).is_empty());
        // Recovery after the reclaim raises nothing; a new episode does.
        assert!(engine
            .apply_batch(&state, &quiet, &processes, false, 65_000)
            .is_empty());
        engine.mark_all_unknown(&state, "GPU attribution sample failed: gone", 70_000);
        assert!(engine
            .overdue_unknown(&state, 65_000 + grace - 1)
            .is_empty());
        assert_eq!(
            ids(engine.overdue_unknown(&state, 65_000 + grace)),
            vec!["gpu0", "gpu1"]
        );
    }

    #[test]
    fn a_reading_that_was_not_admitting_work_has_no_grace_to_run_out() {
        let state = two_lent_gpus();
        let mut engine = engine_for(&state);
        // Never sampled, so it never admitted anything to reclaim.
        engine.mark_all_unknown(&state, "GPU attribution sample failed", 0);
        assert!(engine.overdue_unknown(&state, 60_000).is_empty());
    }

    #[test]
    fn losing_calibration_inside_the_grace_still_reclaims_at_once() {
        let mut cfg = two_lent_gpus().config.clone();
        cfg.owner.gpu_pressure.warmup_samples = 4;
        // An index selector: on another slot it may name another card, so
        // the move below loses the calibration. A UUID selector would keep it.
        cfg.resources[0].selector = Some("0".into());
        let state = AppState::new(cfg).unwrap();
        let mut engine = engine_for(&state);
        let processes = listeners::ProcessTable::default();
        let quiet = batch(&[GPU0, GPU1], Vec::new());
        for now in [0, 5_000, 10_000, 15_000] {
            assert!(engine
                .apply_batch(&state, &quiet, &processes, false, now)
                .is_empty());
        }
        assert!(
            state
                .pressure_view("gpu0")
                .unwrap()
                .value
                .unwrap()
                .calibrated
        );
        state.resources["gpu0"]
            .adapter_mapping
            .write()
            .unwrap()
            .record_failure("adapter not found");
        assert!(engine
            .apply_batch(&state, &quiet, &processes, false, 20_000)
            .is_empty());
        // A card comes back on another slot: its calibration starts over,
        // which reclaims at once rather than at the end of the grace.
        state.resources["gpu0"]
            .adapter_mapping
            .write()
            .unwrap()
            .record_success(AdapterMapping {
                pci: "0000:03:00.0".parse().unwrap(),
                luid: GPU0.into(),
                source: AdapterMappingSource::Probe,
            });
        let refused = engine.apply_batch(&state, &quiet, &processes, false, 21_000);
        assert_eq!(ids(refused.clone()), vec!["gpu0"]);
        assert!(matches!(
            refused[0].cause,
            crate::reclaim_log::RefusalCause::NotCalibrated { .. }
        ));
        assert!(engine.overdue_unknown(&state, 60_000).is_empty());
    }

    /// Records what a reclaim asked of the upstream, and never reports
    /// borrowed work finished.
    #[derive(Default)]
    struct Recorder {
        cancels: std::sync::atomic::AtomicUsize,
        unloads: std::sync::atomic::AtomicUsize,
    }

    struct RecordingAdapter(std::sync::Arc<Recorder>);

    impl crate::plugin::adapter::Adapter for RecordingAdapter {
        fn kind(&self) -> &'static str {
            "recording"
        }
        fn capabilities(&self) -> crate::plugin::adapter::AdapterCapabilities {
            crate::plugin::adapter::AdapterCapabilities {
                can_unload: true,
                reports_model: false,
                can_cancel: true,
            }
        }
        fn classify(&self, _: &http::Method, _: &str) -> crate::plugin::adapter::RequestClass {
            crate::plugin::adapter::RequestClass::Work
        }
        fn cancel<'a>(
            &'a self,
            _: &'a reqwest::Client,
            _: &'a str,
            ids: &'a [String],
            _: &'a [String],
        ) -> futures_util::future::BoxFuture<
            'a,
            anyhow::Result<crate::plugin::adapter::CancelOutcome>,
        > {
            Box::pin(async move {
                self.0
                    .cancels
                    .fetch_add(1, std::sync::atomic::Ordering::SeqCst);
                Ok(crate::plugin::adapter::CancelOutcome {
                    interrupted: ids.to_vec(),
                    ..Default::default()
                })
            })
        }
        fn unload_all<'a>(
            &'a self,
            _: &'a reqwest::Client,
            _: &'a str,
        ) -> futures_util::future::BoxFuture<'a, anyhow::Result<usize>> {
            Box::pin(async move {
                self.0
                    .unloads
                    .fetch_add(1, std::sync::atomic::Ordering::SeqCst);
                Ok(1)
            })
        }
    }

    /// The incident's shape: a borrowed ComfyUI prompt admitted on a clear
    /// reading, with `drain` and a 30-second window, and model unloading on.
    async fn incident() -> (
        std::sync::Arc<AppState>,
        PressureEngine,
        std::sync::Arc<Recorder>,
    ) {
        let mut cfg: crate::config::Config = toml::from_str(&format!(
            "{TWO_CARDS}\n[[upstream]]\nid = \"comfyui\"\nlisten = \"0.0.0.0:8189\"\n\
             target = \"http://127.0.0.1:8188\"\nkind = \"comfyui\"\nresource = \"gpu0\"\n\
             on_owner_return = \"drain\"\ndrain_timeout_s = 30\nyield_on_veto = true\n"
        ))
        .unwrap();
        cfg.upstreams.retain(|u| u.id == "comfyui");
        cfg.owner.gpu_pressure.warmup_samples = 1;
        let mut state = AppState::new(cfg).unwrap();
        let recorder = std::sync::Arc::new(Recorder::default());
        state.upstreams.get_mut("comfyui").unwrap().adapter =
            Box::new(RecordingAdapter(recorder.clone()));
        admitting(&state);
        state.upstreams["comfyui"]
            .models
            .write()
            .unwrap()
            .record_success(crate::lifecycle::Models::default());
        let state = std::sync::Arc::new(state);
        let mut engine = engine_for(&state);
        let quiet = batch(&[GPU0], Vec::new());
        assert!(engine
            .apply_batch(&state, &quiet, &Default::default(), false, 0)
            .is_empty());
        let upstream = state.config.upstreams[0].clone();
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
            "prompt-12".into(),
            crate::admission::Acknowledgement::Confirmed,
        );
        (state, engine, recorder)
    }

    /// One collector cycle at `now_ms`: a failed sample (`None`) or the
    /// owner's load on gpu0, then any reclaim it raises, delivered as the
    /// collector delivers it.
    fn cycle(
        state: &std::sync::Arc<AppState>,
        engine: &mut PressureEngine,
        now_ms: u64,
        owner_pct: Option<f64>,
    ) -> Option<tokio::task::JoinHandle<()>> {
        let mut rising = match owner_pct {
            None => {
                engine.mark_all_unknown(state, "GPU attribution sample failed: busy", now_ms);
                Vec::new()
            }
            Some(pct) => {
                let load = (pct > 0.0).then(|| gpu_procs::ProcGpuSample {
                    pid: 42,
                    luid: GPU0.into(),
                    util_pct: pct,
                });
                let sample = batch(&[GPU0], load.into_iter().collect());
                engine.apply_batch(state, &sample, &Default::default(), false, now_ms)
            }
        };
        rising.extend(engine.overdue_unknown(state, now_ms));
        (!rising.is_empty()).then(|| {
            tokio::spawn(crate::yield_control::on_resources_refused(
                state.clone(),
                rising,
                state.veto.status().0,
            ))
        })
    }

    /// Sleep until `now_ms` on the collector's clock, which these tests
    /// start with the paused runtime clock.
    fn at_collector_time() -> impl Fn(u64) -> tokio::time::Sleep {
        let start = tokio::time::Instant::now();
        move |now_ms| tokio::time::sleep_until(start + std::time::Duration::from_millis(now_ms))
    }

    fn live_entries(state: &AppState) -> Vec<String> {
        state
            .admission
            .lock()
            .unwrap()
            .snapshot()
            .into_iter()
            .map(|w| w.phase.to_string())
            .collect()
    }

    /// How often an attribution sample actually lands at the default
    /// 5-second poll: the poll interval plus the read's own duration, as
    /// measured on a Windows host over 30 days.
    const CADENCE: u64 = 7_900;

    /// The incident as it happened, on the collector's clock with 0 at
    /// 04:41:24.838: good samples every 7.9 s, the prompt admitted at 6.97 s
    /// (04:41:31.806), the last good sample at 7.9 s (04:41:32.738), the
    /// one failed sample at 15.8 s (04:41:40.638) and the recovery at
    /// 24.244 s (04:41:49.082), 16.3 s after the last good sample.
    #[tokio::test(start_paused = true)]
    async fn incident_replay_one_failed_sample_does_not_interrupt_the_borrowed_job() {
        let (state, mut engine, recorder) = incident().await;
        let at = at_collector_time();
        let rc = state.config.resources[0].clone();
        at(CADENCE).await;
        assert!(cycle(&state, &mut engine, CADENCE, Some(0.0)).is_none());
        at(15_800).await;
        assert!(cycle(&state, &mut engine, 15_800, None).is_none());
        assert!(crate::gate::pressure_refusal(&state, &rc).is_some());
        at(24_244).await;
        assert!(cycle(&state, &mut engine, 24_244, Some(0.0)).is_none());
        assert!(crate::gate::pressure_refusal(&state, &rc).is_none());
        // Well past where the 30-second drain would have ended.
        for now_ms in (24_244 + CADENCE..=120_000).step_by(CADENCE as usize) {
            at(now_ms).await;
            assert!(cycle(&state, &mut engine, now_ms, Some(0.0)).is_none());
        }
        use std::sync::atomic::Ordering::SeqCst;
        assert_eq!(recorder.cancels.load(SeqCst), 0);
        assert_eq!(recorder.unloads.load(SeqCst), 0);
        assert!(state.reclaim_log.lock().unwrap().events().is_empty());
        assert_eq!(live_entries(&state), vec!["tracking"]);
    }

    /// Two missed samples in a row: the recovery comes 23.7 s after the last
    /// good sample, past the gap limit, so it raises the reclaim itself,
    /// which stands down at once on the quiet reading it brought.
    #[tokio::test(start_paused = true)]
    async fn incident_replay_with_a_longer_gap_reclaims_and_stands_down_at_once() {
        let (state, mut engine, recorder) = incident().await;
        let at = at_collector_time();
        for now_ms in [CADENCE, 2 * CADENCE] {
            at(now_ms).await;
            assert!(cycle(&state, &mut engine, now_ms, None).is_none());
        }
        at(3 * CADENCE).await;
        let reclaim = cycle(&state, &mut engine, 3 * CADENCE, Some(0.0))
            .expect("an unobserved gap past the limit reclaims on recovery");
        tokio::time::timeout(std::time::Duration::from_secs(1), reclaim)
            .await
            .unwrap()
            .unwrap();
        use std::sync::atomic::Ordering::SeqCst;
        assert_eq!(recorder.cancels.load(SeqCst), 0);
        assert_eq!(recorder.unloads.load(SeqCst), 0);
        assert_eq!(live_entries(&state), vec!["tracking"]);
        let events = state.reclaim_log.lock().unwrap().events();
        assert!(matches!(
            events[0].resources[0].cause,
            crate::reclaim_log::RefusalCause::Unknown {
                unknown_for_ms: Some(23_700),
                ..
            }
        ));
        assert!(events[0].upstreams[0].abandoned_after_ms.is_some());
    }

    #[tokio::test(start_paused = true)]
    async fn incident_replay_a_reading_recovered_inside_the_drain_stands_the_reclaim_down() {
        let (state, mut engine, recorder) = incident().await;
        let at = at_collector_time();
        use std::sync::atomic::Ordering::SeqCst;
        // Three missed samples: the grace runs out on the third, 23.7 s
        // after the last good sample, before any recovery.
        let mut reclaim = None;
        for now_ms in [CADENCE, 2 * CADENCE, 3 * CADENCE] {
            at(now_ms).await;
            assert!(reclaim.is_none(), "reclaimed before the grace ran out");
            reclaim = cycle(&state, &mut engine, now_ms, None);
        }
        let reclaim = reclaim.expect("the grace ran out at 23.7 s");
        // The reading recovers one sample into the 30-second drain.
        at(4 * CADENCE).await;
        assert!(cycle(&state, &mut engine, 4 * CADENCE, Some(0.0)).is_none());
        tokio::time::timeout(std::time::Duration::from_secs(1), reclaim)
            .await
            .expect("the drain stands down when its trigger clears")
            .unwrap();
        assert_eq!(recorder.cancels.load(SeqCst), 0);
        assert_eq!(recorder.unloads.load(SeqCst), 0);
        assert_eq!(live_entries(&state), vec!["tracking"]);
        let events = state.reclaim_log.lock().unwrap().events();
        assert_eq!(events.len(), 1);
        let abandoned = events[0].upstreams[0].abandoned_after_ms.unwrap();
        assert!(
            (CADENCE..CADENCE + 1_000).contains(&abandoned),
            "{abandoned}"
        );
        assert!(matches!(
            events[0].resources[0].cause,
            crate::reclaim_log::RefusalCause::Unknown {
                unknown_for_ms: Some(23_700),
                ..
            }
        ));

        // A later outage that lasts reclaims as before, with the drain
        // running to its deadline.
        let mut later = None;
        for k in 5..=8 {
            at(k * CADENCE).await;
            later = later.or(cycle(&state, &mut engine, k * CADENCE, None));
        }
        let later = later.expect("the second outage outlasted its grace");
        tokio::time::timeout(std::time::Duration::from_secs(120), later)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(recorder.cancels.load(SeqCst), 1);
        assert_eq!(recorder.unloads.load(SeqCst), 1);
        let events = state.reclaim_log.lock().unwrap().events();
        assert_eq!(events.len(), 2);
        assert_eq!(events[1].upstreams[0].abandoned_after_ms, None);
    }

    /// Samples at the measured cadence where every `run + 1`-th one is valid
    /// at `owner_pct` and the rest fail, from 7.9 s to about 166 s. Returns
    /// the reclaims raised, with the collector time of the first.
    async fn failure_runs(
        state: &std::sync::Arc<AppState>,
        engine: &mut PressureEngine,
        run: u64,
        owner_pct: f64,
    ) -> (Vec<tokio::task::JoinHandle<()>>, Option<u64>) {
        let at = at_collector_time();
        let mut reclaims = Vec::new();
        let mut first = None;
        for k in 1..=21 {
            let now_ms = k * CADENCE;
            at(now_ms).await;
            let sample = (k % (run + 1) == 0).then_some(owner_pct);
            if let Some(reclaim) = cycle(state, engine, now_ms, sample) {
                first = first.or(Some(now_ms));
                reclaims.push(reclaim);
            }
        }
        (reclaims, first)
    }

    /// One missed sample between valid ones leaves 15.8 s between them,
    /// inside the 20-second gap limit, so the owner's sustained load still
    /// engages: the missed sample neither interrupts the tracker nor
    /// reclaims on its own.
    #[tokio::test(start_paused = true)]
    async fn one_missed_sample_between_busy_samples_still_engages() {
        let (state, mut engine, recorder) = incident().await;
        let sustain_ms = state.config.owner.gpu_pressure.sustain_s * 1_000;
        let (reclaims, first) = failure_runs(&state, &mut engine, 1, 95.0).await;
        assert_eq!(reclaims.len(), 1, "one reclaim for one engagement");
        // The first busy sample is at 15.8 s; it engages once load has been
        // seen for sustain_s, at the next valid sample after that.
        let first = first.unwrap();
        assert!(first <= 2 * CADENCE + sustain_ms + 2 * CADENCE, "{first}");
        let events = state.reclaim_log.lock().unwrap().events();
        assert!(matches!(
            events[0].resources[0].cause,
            crate::reclaim_log::RefusalCause::Engaged { .. }
        ));
        for reclaim in reclaims {
            tokio::time::timeout(std::time::Duration::from_secs(120), reclaim)
                .await
                .unwrap()
                .unwrap();
        }
        use std::sync::atomic::Ordering::SeqCst;
        assert_eq!(recorder.cancels.load(SeqCst), 1);
    }

    #[tokio::test(start_paused = true)]
    async fn one_missed_sample_between_quiet_samples_never_reclaims() {
        let (state, mut engine, recorder) = incident().await;
        let (reclaims, _) = failure_runs(&state, &mut engine, 1, 5.0).await;
        assert!(reclaims.is_empty());
        use std::sync::atomic::Ordering::SeqCst;
        assert_eq!(recorder.cancels.load(SeqCst), 0);
        assert!(state.reclaim_log.lock().unwrap().events().is_empty());
        assert_eq!(live_entries(&state), vec!["tracking"]);
    }

    /// Two missed samples leave 23.7 s between valid ones, past the gap
    /// limit: each such outage reclaims once, on the sample that ends it.
    #[tokio::test(start_paused = true)]
    async fn two_missed_samples_between_busy_samples_reclaim() {
        let (state, mut engine, recorder) = incident().await;
        let (reclaims, first) = failure_runs(&state, &mut engine, 2, 95.0).await;
        assert_eq!(first, Some(3 * CADENCE));
        for reclaim in reclaims {
            tokio::time::timeout(std::time::Duration::from_secs(120), reclaim)
                .await
                .unwrap()
                .unwrap();
        }
        // The owner is busy whenever the card can be read, so no drain
        // stands down: each reclaim that reached the job cancelled it.
        use std::sync::atomic::Ordering::SeqCst;
        assert!(recorder.cancels.load(SeqCst) >= 1);
        let events = state.reclaim_log.lock().unwrap().events();
        assert!(events.iter().all(|event| event
            .upstreams
            .iter()
            .all(|u| u.abandoned_after_ms.is_none())));
    }

    #[tokio::test(start_paused = true)]
    async fn two_missed_samples_between_quiet_samples_reclaim_once_per_outage() {
        let (state, mut engine, recorder) = incident().await;
        // Seven outages, each ended by the valid sample at 3k × 7.9 s.
        let (reclaims, _) = failure_runs(&state, &mut engine, 2, 5.0).await;
        assert_eq!(reclaims.len(), 7);
        for reclaim in reclaims {
            tokio::time::timeout(std::time::Duration::from_secs(120), reclaim)
                .await
                .unwrap()
                .unwrap();
        }
        // Each stood down on the quiet sample that ended its outage.
        use std::sync::atomic::Ordering::SeqCst;
        assert_eq!(recorder.cancels.load(SeqCst), 0);
        let events = state.reclaim_log.lock().unwrap().events();
        assert_eq!(events.len(), 7);
        assert!(events
            .iter()
            .all(|event| event.upstreams[0].abandoned_after_ms.is_some()));
    }

    #[tokio::test(start_paused = true)]
    async fn a_reading_that_recovers_above_the_threshold_does_not_stand_the_reclaim_down() {
        let (state, mut engine, recorder) = incident().await;
        let at = at_collector_time();
        let mut reclaim = None;
        for now_ms in [CADENCE, 2 * CADENCE, 3 * CADENCE] {
            at(now_ms).await;
            reclaim = reclaim.or(cycle(&state, &mut engine, now_ms, None));
        }
        let reclaim = reclaim.expect("the grace ran out at 23.7 s");
        // Back, but carrying the owner's load: above the threshold and not
        // yet sustained long enough to engage.
        at(4 * CADENCE).await;
        assert!(cycle(&state, &mut engine, 4 * CADENCE, Some(95.0)).is_none());
        let verdict = state.pressure_view("gpu0").unwrap().value.unwrap();
        assert!(!verdict.engaged && verdict.non_lender_pct > verdict.threshold_pct);
        assert!(crate::gate::pressure_refusal(&state, &state.config.resources[0]).is_none());
        tokio::time::timeout(std::time::Duration::from_secs(120), reclaim)
            .await
            .unwrap()
            .unwrap();
        use std::sync::atomic::Ordering::SeqCst;
        assert_eq!(recorder.cancels.load(SeqCst), 1);
        let events = state.reclaim_log.lock().unwrap().events();
        assert_eq!(events[0].upstreams[0].abandoned_after_ms, None);
    }

    #[test]
    fn engaged_pressure_does_not_release_on_quiet_it_only_partly_observed() {
        let state = two_lent_gpus();
        let mut engine = engine_for(&state);
        let processes = listeners::ProcessTable::default();
        let quiet = batch(&[GPU0, GPU1], Vec::new());
        let busy = batch(
            &[GPU0, GPU1],
            vec![gpu_procs::ProcGpuSample {
                pid: 42,
                luid: GPU0.into(),
                util_pct: 95.0,
            }],
        );
        engine.apply_batch(&state, &quiet, &processes, false, 0);
        for now_ms in [5_000, 10_000, 15_000, 20_000] {
            engine.apply_batch(&state, &busy, &processes, false, now_ms);
        }
        let engaged =
            |state: &AppState| state.pressure_view("gpu0").unwrap().value.unwrap().engaged;
        assert!(engaged(&state));
        let release_ms = state.config.owner.release_after_s * 1_000;
        // Quiet, but every other sample is missed, for twice the release time.
        let mut now_ms = 20_000;
        while now_ms < 20_000 + 2 * release_ms {
            now_ms += 5_000;
            engine.mark_all_unknown(&state, "GPU attribution sample failed", now_ms);
            now_ms += 5_000;
            engine.apply_batch(&state, &quiet, &processes, false, now_ms);
            assert!(
                engaged(&state),
                "released at {now_ms} ms on unobserved quiet"
            );
        }
        // Quiet that is observed throughout releases as usual.
        let observed_from = now_ms;
        while engaged(&state) {
            now_ms += 5_000;
            engine.apply_batch(&state, &quiet, &processes, false, now_ms);
            assert!(now_ms <= observed_from + release_ms + 5_000);
        }
    }
}
