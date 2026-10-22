//! Turning attributed GPU utilisation into a presence verdict.
//!
//! [`super::super::probes::gpu_procs`] answers "how much of this card is work
//! vacancyd did not admit?". This module decides whether that number means the
//! owner wants their machine back.
//!
//! # Why the threshold is learned rather than configured
//!
//! There is no percentage that is right for everyone. A quiet single-monitor
//! desktop idles near zero; a four-monitor machine with a compositing browser
//! and an RGB control panel can sit above 20% having done nothing. Shipping one
//! number means shipping a default that either never fires on a busy desktop or
//! fires constantly on a busy one, and asking every adopter to measure their own
//! floor is the same maintenance burden as the process list this signal exists
//! to replace.
//!
//! So the threshold is **learned**, per device and per attendance state, from
//! what the card ordinarily carries, and the knobs remain for anyone who wants
//! to bound or override the result.
//!
//! # What is learned
//!
//! Every valid reading goes into a histogram of half-percent bins, engaged
//! readings included. Before a reading is added, the weights already there
//! decay with a half-life measured in *observed* time: the interval since the
//! previous reading of the same profile, never more than the gap that still
//! counts as continuous. Downtime, locked hours seen by the other profile and
//! failed reads therefore age nothing; a week switched off is not a week
//! forgotten.
//!
//! The target threshold is a high quantile of that histogram plus a margin,
//! clamped between the floor and the cap. A high quantile is deliberate. The
//! threshold has to sit above what the owner's desktop ordinarily does,
//! including its busier hours — a 25–39% compositing band that recurs every
//! afternoon is the machine's normal, not the owner returning. A low quantile
//! of a bimodal day learns the quiet mode and then refuses through the whole
//! busy one.
//!
//! # Why the cap is the guard
//!
//! Learning from engaged readings means a long render or game is learned too,
//! and an estimator left to itself would teach the work away: the threshold
//! would climb until the render no longer registered. The cap is what stops
//! that. Calibration may never learn a threshold above `cap_pct`, and real
//! work — games, rendering, encoding — runs far above it, so whatever the
//! histogram has seen, sustained heavy load still engages.
//!
//! # Rise slowly, fall at once
//!
//! The threshold in force follows the target asymmetrically. A lower target
//! applies immediately: protecting the owner sooner is always safe. A higher
//! one is approached at a bounded rate per observed hour, so an hour of heavy
//! work can raise the threshold by at most an hour's allowance even below the
//! cap, and a run of nuisance refusals clears over hours rather than at once.
//!
//! # Why attendance is tracked separately
//!
//! A locked session composites far less than an active one, so a single
//! histogram learned mostly while someone is at the desk would be too high to
//! detect anything on an unattended machine — precisely where the long render
//! left running overnight needs protecting. Attended and unattended readings
//! are therefore learned separately and never mixed.

use std::collections::BTreeMap;

#[derive(Debug, Default, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum PressureMode {
    #[default]
    Calibrated,
    /// Retired. Still parsed so an older configuration keeps loading, and run
    /// as [`Self::Calibrated`]; see [`Self::effective`].
    Adaptive,
    Fixed,
}

impl PressureMode {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Calibrated => "calibrated",
            Self::Adaptive => "adaptive",
            Self::Fixed => "fixed",
        }
    }

    /// The mode that actually runs. `adaptive` was retired when calibration
    /// learned to follow the machine; a configuration naming it runs as
    /// calibrated.
    pub fn effective(self) -> Self {
        match self {
            Self::Adaptive => Self::Calibrated,
            mode => mode,
        }
    }
}

/// Tuning for [`PressureTracker`]. All percentages are of one device.
#[derive(Debug, Clone, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct PressureConfig {
    pub mode: PressureMode,
    /// The highest threshold calibration may learn. The guard against a
    /// long-running workload teaching itself away: real work runs far above
    /// it, so it engages whatever the histogram has seen.
    pub cap_pct: f64,
    /// Explicit engage threshold in fixed mode. No learning in that mode.
    pub threshold_pct: f64,
    /// Added to the learned quantile to get the target threshold.
    pub margin_pct: f64,
    /// The engage threshold is never allowed below this, however quiet the
    /// machine looks. Without a floor, a genuinely idle machine learns a
    /// threshold near zero and then treats ordinary desktop noise — a scroll,
    /// a notification animation — as its owner returning.
    ///
    /// The default has to clear the idle load of a **display-driving** GPU, not
    /// a compute one. Measured on the reference machine, the card rendering the
    /// desktop sits at 11–17% doing nothing while the compute card sits at
    /// 5–9%. A floor of 15 was tried first and was wrong: during warmup, before
    /// a baseline exists, the display card read 16% and engaged the veto
    /// against its own wallpaper. Real work — games, rendering, encoding — runs
    /// above 90%, so there is a wide gap to sit in.
    pub floor_pct: f64,
    /// Seconds the reading must stay above the threshold before pressure
    /// engages for this resource.
    pub sustain_s: u64,
    /// Seconds the reading must remain below the threshold before pressure
    /// clears. This was previously supplied by the machine-wide veto; it must
    /// live in each tracker once pressure is enforced per resource.
    pub release_after_s: u64,
    /// Quantile of the learned histogram the target is built on, 0 < q < 1.
    pub learn_quantile: f64,
    /// Hours of observed time after which a reading counts half as much.
    pub learn_half_life_h: f64,
    /// How far the threshold in force may rise per observed hour, in
    /// percentage points. A fall is never limited.
    pub raise_pct_per_h: f64,
    /// Samples a profile needs from empty before its first threshold is set,
    /// and before a histogram replaces a migrated threshold. Lending is
    /// refused during first calibration, even when the latest reading is
    /// quiet.
    pub warmup_samples: usize,
    /// The longest interval between readings that still counts as one
    /// continuous observation.
    pub max_gap_ms: u64,
}

impl Default for PressureConfig {
    fn default() -> Self {
        Self {
            mode: PressureMode::Calibrated,
            cap_pct: 40.0,
            threshold_pct: 0.0,
            margin_pct: 20.0,
            floor_pct: 25.0,
            sustain_s: 15,
            release_after_s: 120,
            learn_quantile: 0.75,
            learn_half_life_h: 72.0,
            raise_pct_per_h: 1.0,
            warmup_samples: 60,
            max_gap_ms: 15_000,
        }
    }
}

impl PressureConfig {
    /// The threshold a learned quantile asks for: the margin above it,
    /// clamped between the floor and the cap.
    fn target_for(&self, quantile: f64) -> f64 {
        self.clamp_threshold(quantile + self.margin_pct)
    }

    /// Bound a threshold to what calibration may hold. `max` then `min`
    /// rather than `clamp`, which panics on an inverted range; validation
    /// keeps the floor below the cap in a loaded configuration.
    fn clamp_threshold(&self, pct: f64) -> f64 {
        pct.max(self.floor_pct).min(self.cap_pct)
    }
}

/// What one observation concluded, including the reasoning, so `/status` can
/// show an operator why the daemon thinks what it thinks.
#[derive(Debug, Clone, PartialEq, serde::Serialize)]
pub struct PressureVerdict {
    pub mode: PressureMode,
    /// Observation quality and estimator readiness are separate facts.
    pub calibrated: bool,
    pub calibration_samples: usize,
    pub calibration_required: usize,
    pub calibration_hint: Option<String>,
    /// Attributed utilisation from work outside the lender process set.
    pub non_lender_pct: f64,
    pub engaged: bool,
    /// The threshold in force for this sample.
    pub threshold_pct: f64,
    /// The learned quantile, or `None` until the histogram holds
    /// `warmup_samples` readings.
    pub baseline_pct: Option<f64>,
    /// The threshold the histogram asks for. The threshold in force moves
    /// toward it: at once when lower, at a bounded rate when higher.
    pub target_pct: Option<f64>,
    /// Valid readings the histogram has learned from, undecayed.
    pub learned_samples: u64,
    /// Observed seconds those readings span.
    pub learned_s: u64,
    /// Seconds the reading has been continuously above the threshold.
    pub above_for_s: u64,
}

/// Bins are half a percentage point wide. Bin `i` holds readings from
/// `i / 2` up to but not including `(i + 1) / 2`; the last holds exactly 100.
pub const BINS: usize = 201;
const BIN_WIDTH_PCT: f64 = 0.5;
const MS_PER_HOUR: f64 = 3_600_000.0;
/// Saved weights keep this many decimal places. A thousandth of a reading
/// changes no quantile, and the cache stays small.
const SAVED_WEIGHT_SCALE: f64 = 1_000.0;
/// The largest total weight a saved histogram may hold. Each reading adds 1
/// and decays, so the total converges to `half_life / (ln 2 × cadence)`: at
/// the longest half-life (30 days) and fastest cadence (1 s) about 3.7
/// million. Anything above this was not written by a tracker. Bounding it
/// also keeps a hand-edited weight from overflowing to infinity, which JSON
/// cannot hold and which would make the next save unreadable for every GPU.
const MAX_SAVED_WEIGHT: f64 = 1e7;
/// Observed time a profile migrated from a frozen baseline must learn from
/// before its histogram may lower the threshold. The frozen baseline came
/// from a few quiet minutes, and an eight-minute histogram is that same
/// noisy anchor again: a card whose saved 13% gave 33% would drop to the
/// 25% floor on its first quiet minutes and then take about fifteen hours
/// to climb back. A day covers a machine's ordinary cycle once. Rises are
/// rate-limited as always.
const MIGRATED_MATURITY_MS: u64 = 24 * 60 * 60 * 1000;

/// The highest `cap_pct` a tracker runs with. At 100 a render that holds a
/// card near 99% would, at the rise limit, learn itself away within a day;
/// 90 stays under real work while leaving room above a busy desktop.
pub const MAX_CAP_PCT: f64 = 90.0;

fn bin_of(pct: f64) -> usize {
    ((pct / BIN_WIDTH_PCT).floor() as usize).min(BINS - 1)
}

/// A bin's upper edge. Quantiles report it, so a learned value is never
/// below a reading it stands for.
fn upper_edge(bin: usize) -> f64 {
    ((bin + 1) as f64 * BIN_WIDTH_PCT).min(100.0)
}

/// Decaying weights over [`BINS`] half-percent bins.
#[derive(Debug, Clone, PartialEq)]
struct Histogram {
    weights: Vec<f64>,
}

impl Default for Histogram {
    fn default() -> Self {
        Self {
            weights: vec![0.0; BINS],
        }
    }
}

impl Histogram {
    fn total(&self) -> f64 {
        self.weights.iter().sum()
    }

    /// Weighted nearest-rank quantile, reported as the bin's upper edge.
    fn quantile(&self, q: f64) -> Option<f64> {
        let total = self.total();
        // Weights are finite and non-negative by construction.
        if total <= 0.0 {
            return None;
        }
        let rank = q.clamp(0.0, 1.0) * total;
        let mut cumulative = 0.0;
        for (bin, weight) in self.weights.iter().enumerate() {
            cumulative += weight;
            if *weight > 0.0 && cumulative >= rank {
                return Some(upper_edge(bin));
            }
        }
        // Summation order can leave the running sum a rounding error short
        // of the total; the answer is then the highest occupied bin.
        self.weights
            .iter()
            .rposition(|weight| *weight > 0.0)
            .map(upper_edge)
    }

    fn sparse(&self) -> BTreeMap<u16, f64> {
        self.weights
            .iter()
            .enumerate()
            .filter_map(|(bin, weight)| {
                let kept = (weight * SAVED_WEIGHT_SCALE).round() / SAVED_WEIGHT_SCALE;
                (kept > 0.0).then_some((bin as u16, kept))
            })
            .collect()
    }

    /// `None` when any bin is out of range, any weight is not a finite,
    /// non-negative number, or the total exceeds [`MAX_SAVED_WEIGHT`]: a
    /// damaged histogram is not partly trusted.
    fn from_sparse(bins: &BTreeMap<u16, f64>) -> Option<Self> {
        let mut histogram = Self::default();
        for (bin, weight) in bins {
            if usize::from(*bin) >= BINS || !(0.0..=MAX_SAVED_WEIGHT).contains(weight) {
                return None;
            }
            histogram.weights[usize::from(*bin)] = *weight;
        }
        (histogram.total() <= MAX_SAVED_WEIGHT).then_some(histogram)
    }
}

/// One attendance profile's learning.
#[derive(Debug, Clone, Default)]
struct Learning {
    histogram: Histogram,
    /// Valid readings added, undecayed.
    samples: u64,
    /// Observed time those readings span: the sum of the intervals that
    /// decayed the histogram.
    observed_ms: u64,
    /// The engage threshold calibration holds. `None` until the first
    /// calibration completes or a saved one is restored.
    in_force: Option<f64>,
    /// When this profile's previous valid reading was taken. Not saved: the
    /// clock restarts with the process, and time not observed decays nothing.
    last_ms: Option<u64>,
    /// The threshold in force came from a frozen baseline and the histogram
    /// has not yet learned from [`MIGRATED_MATURITY_MS`]; until then it may
    /// not lower that threshold. Saved, so a restart keeps the protection.
    migrated: bool,
}

impl Learning {
    /// Add one valid reading and move the threshold in force.
    fn learn(&mut self, cfg: &PressureConfig, now_ms: u64, pct: f64) {
        let elapsed_ms = self
            .last_ms
            .map_or(0, |at| now_ms.saturating_sub(at).min(cfg.max_gap_ms));
        self.last_ms = Some(now_ms);
        if elapsed_ms > 0 {
            let half_lives = elapsed_ms as f64 / (cfg.learn_half_life_h * MS_PER_HOUR);
            let factor = 0.5_f64.powf(half_lives);
            for weight in &mut self.histogram.weights {
                *weight *= factor;
            }
        }
        self.histogram.weights[bin_of(pct)] += 1.0;
        self.samples = self.samples.saturating_add(1);
        self.observed_ms = self.observed_ms.saturating_add(elapsed_ms);
        self.migrated &= self.observed_ms < MIGRATED_MATURITY_MS;

        let Some(target) = self.quantile(cfg).map(|q| cfg.target_for(q)) else {
            return;
        };
        self.in_force = Some(match self.in_force {
            // The first calibration, from empty: no history to rise from.
            None => target,
            Some(current) if target <= current && self.migrated => current,
            Some(current) if target <= current => target,
            Some(current) => {
                let allowance = cfg.raise_pct_per_h * elapsed_ms as f64 / MS_PER_HOUR;
                (current + allowance).min(target)
            }
        });
    }

    /// The learned quantile, once the histogram holds `warmup_samples`
    /// readings. Before then a migrated threshold stands unchanged.
    fn quantile(&self, cfg: &PressureConfig) -> Option<f64> {
        (self.samples >= cfg.warmup_samples as u64)
            .then(|| self.histogram.quantile(cfg.learn_quantile))
            .flatten()
    }

    /// Forget when the previous reading was taken, so the next one ages
    /// nothing: the time in between was not observed. Learning, finished or
    /// not, is kept; the cap is the guard against what it learned.
    fn forget_clock(&mut self) {
        self.last_ms = None;
    }

    /// Replace this profile's learning with a saved one, if it is a valid,
    /// completed calibration.
    fn restore(&mut self, cfg: &PressureConfig, saved: &LearnedProfile) -> Restored {
        let in_range = |pct: &f64| pct.is_finite() && (0.0..=100.0).contains(pct);
        let (in_force, kind) = match (saved.threshold_in_force, saved.legacy_baseline_pct) {
            (None, None) => return Restored::Empty,
            (Some(threshold), _) => (
                Some(threshold)
                    .filter(in_range)
                    .map(|t| cfg.clamp_threshold(t)),
                Restored::Learned,
            ),
            (None, Some(baseline)) => (
                Some(baseline).filter(in_range).map(|b| cfg.target_for(b)),
                Restored::LegacyBaseline,
            ),
        };
        match (in_force, Histogram::from_sparse(&saved.bins)) {
            (Some(in_force), Some(histogram)) => {
                *self = Self {
                    histogram,
                    samples: saved.samples,
                    observed_ms: saved.observed_ms,
                    in_force: Some(in_force),
                    last_ms: None,
                    migrated: kind == Restored::LegacyBaseline || saved.migrated,
                };
                kind
            }
            _ => Restored::Invalid,
        }
    }

    fn snapshot(&self) -> LearnedProfile {
        match self.in_force {
            // Only completed calibration is kept across a restart; an
            // unfinished first calibration starts again from empty.
            None => LearnedProfile::default(),
            Some(in_force) => LearnedProfile {
                bins: self.histogram.sparse(),
                samples: self.samples,
                observed_ms: self.observed_ms,
                threshold_in_force: Some(in_force),
                legacy_baseline_pct: None,
                migrated: self.migrated,
            },
        }
    }
}

/// Per-device engage-edge detector with a learned threshold.
///
/// One of these per lendable GPU. The caller supplies a monotonic clock in
/// milliseconds rather than reading one internally, so that the multi-hour
/// learning behaviour can be tested without waiting multiple hours.
#[derive(Debug)]
pub struct PressureTracker {
    cfg: PressureConfig,
    attended: Learning,
    unattended: Learning,
    above_since_ms: Option<u64>,
    below_since_ms: Option<u64>,
    engaged: bool,
    last_observation: Option<(u64, bool)>,
}

impl PressureTracker {
    pub fn new(cfg: PressureConfig) -> Self {
        Self {
            cfg,
            attended: Learning::default(),
            unattended: Learning::default(),
            above_since_ms: None,
            below_since_ms: None,
            engaged: false,
            last_observation: None,
        }
    }

    fn learning(&self, unattended: bool) -> &Learning {
        if unattended {
            &self.unattended
        } else {
            &self.attended
        }
    }

    /// Record one reading.
    ///
    /// `pct` is this device's non-lender utilisation; `unattended` is true when
    /// the session is locked, logged out or disconnected.
    pub fn observe(&mut self, now_ms: u64, pct: f64, unattended: bool) -> PressureVerdict {
        // A missing read is not evidence that a sustain/release interval was
        // continuously quiet or busy. Nor can two attendance profiles share it.
        if self.last_observation.is_some_and(|(at, profile)| {
            profile != unattended || now_ms < at || now_ms.saturating_sub(at) > self.cfg.max_gap_ms
        }) {
            self.interrupt();
        }
        self.last_observation = Some((now_ms, unattended));
        let valid = pct.is_finite() && (0.0..=100.0).contains(&pct);
        let learning = if unattended {
            &mut self.unattended
        } else {
            &mut self.attended
        };
        if !valid {
            // A failed read is not observed time.
            learning.forget_clock();
        } else if self.cfg.mode != PressureMode::Fixed {
            learning.learn(&self.cfg, now_ms, pct);
        }
        let learned = learned_state(&self.cfg, self.learning(unattended));
        let calibrated = valid && learned.calibrated;
        let calibration_hint = if !valid {
            Some("invalid pressure sample; waiting for a valid reading".into())
        } else if !calibrated {
            Some("collecting samples for this attendance profile (locked and unlocked calibrate separately)".into())
        } else if learned.baseline_pct.is_some_and(|q| q > self.cfg.cap_pct) {
            Some("learned load is above the calibration cap; target held at cap".into())
        } else {
            None
        };

        let threshold = learned.threshold_pct;

        if !calibrated {
            self.above_since_ms = None;
            self.below_since_ms = None;
        } else if pct >= threshold {
            self.below_since_ms = None;
            let above_since = *self.above_since_ms.get_or_insert(now_ms);
            if now_ms.saturating_sub(above_since) / 1000 >= self.cfg.sustain_s {
                self.engaged = true;
            }
        } else {
            self.above_since_ms = None;
            if self.engaged {
                let below_since = self.below_since_ms.get_or_insert(now_ms);
                if now_ms.saturating_sub(*below_since) / 1000 >= self.cfg.release_after_s {
                    self.engaged = false;
                    self.below_since_ms = None;
                }
            }
        }
        let above_for_s = self
            .above_since_ms
            .map(|t| now_ms.saturating_sub(t) / 1000)
            .unwrap_or(0);

        PressureVerdict {
            mode: self.cfg.mode,
            calibrated,
            calibration_samples: learned.calibration_samples,
            calibration_required: self.cfg.warmup_samples,
            calibration_hint,
            non_lender_pct: pct,
            engaged: self.engaged,
            threshold_pct: threshold,
            baseline_pct: learned.baseline_pct,
            target_pct: learned.target_pct,
            learned_samples: learned.learned_samples,
            learned_s: learned.learned_s,
            above_for_s,
        }
    }

    /// The learned state of one attendance profile, whether or not it is
    /// the profile being observed. Reads only; it changes nothing.
    pub fn profile_state(&self, unattended: bool) -> ProfileState {
        learned_state(&self.cfg, self.learning(unattended))
    }

    /// One profile's histogram in figures, for the diagnostics export.
    pub fn summary(&self, unattended: bool) -> LearnedSummary {
        let learning = self.learning(unattended);
        let state = learned_state(&self.cfg, learning);
        LearnedSummary {
            samples: learning.samples,
            learned_s: learning.observed_ms / 1000,
            total_weight: learning.histogram.total(),
            quantiles: SUMMARY_QUANTILES
                .iter()
                .filter_map(|q| {
                    learning
                        .histogram
                        .quantile(*q)
                        .map(|pct| QuantilePoint { q: *q, pct })
                })
                .collect(),
            baseline_pct: state.baseline_pct,
            target_pct: state.target_pct,
            threshold_in_force_pct: learning.in_force,
        }
    }

    /// What this tracker has learned, in a form worth keeping across restarts.
    pub fn snapshot(&self) -> PressureSnapshot {
        PressureSnapshot {
            attended: self.attended.snapshot(),
            unattended: self.unattended.snapshot(),
        }
    }

    /// Restore only validated, completed calibration. Never fabricates
    /// samples: a legacy baseline becomes a starting threshold with an empty
    /// histogram, converted with this tracker's margin, floor and cap. A
    /// restored threshold is bounded by this tracker's floor and cap.
    /// Returns what each profile, attended then unattended, restored.
    pub fn seed(&mut self, snap: &PressureSnapshot) -> [Restored; 2] {
        [
            self.attended.restore(&self.cfg, &snap.attended),
            self.unattended.restore(&self.cfg, &snap.unattended),
        ]
    }

    /// A sample was missed. Quiet that was not observed cannot count towards
    /// release, so the release timer restarts; the sustain timer runs on,
    /// since load the owner keeps up through a miss is still the owner's.
    /// [`Self::observe`] interrupts both if valid samples end up further
    /// apart than `max_gap_ms`.
    pub fn note_missed_sample(&mut self) {
        self.below_since_ms = None;
    }

    pub fn interrupt(&mut self) {
        self.above_since_ms = None;
        self.below_since_ms = None;
        self.last_observation = None;
        self.attended.forget_clock();
        self.unattended.forget_clock();
    }
}

/// What [`PressureTracker::seed`] restored for one profile.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Restored {
    /// Nothing was saved for it.
    Empty,
    /// A learned histogram and its threshold in force.
    Learned,
    /// A baseline saved by an earlier version, converted to a starting
    /// threshold.
    LegacyBaseline,
    /// Something was saved, but not a usable calibration.
    Invalid,
}

/// The quantiles the diagnostics export reports for each histogram.
pub const SUMMARY_QUANTILES: [f64; 7] = [0.1, 0.25, 0.5, 0.75, 0.9, 0.95, 0.99];

#[derive(Debug, Clone, PartialEq, serde::Serialize)]
pub struct QuantilePoint {
    pub q: f64,
    pub pct: f64,
}

/// One profile's histogram in figures. Quantiles are bin upper edges.
#[derive(Debug, Clone, PartialEq, serde::Serialize)]
pub struct LearnedSummary {
    pub samples: u64,
    pub learned_s: u64,
    pub total_weight: f64,
    /// Empty while the histogram holds nothing.
    pub quantiles: Vec<QuantilePoint>,
    /// The configured learning quantile, once warm.
    pub baseline_pct: Option<f64>,
    pub target_pct: Option<f64>,
    pub threshold_in_force_pct: Option<f64>,
}

/// What one attendance profile has learned under `cfg`: the learned
/// quantile, the target it asks for, the threshold in force, and
/// calibration progress. `calibrated` here ignores the current sample;
/// [`PressureTracker::observe`] also requires that sample to be valid. The
/// one source for both the observed verdict and
/// [`PressureTracker::profile_state`].
fn learned_state(cfg: &PressureConfig, learning: &Learning) -> ProfileState {
    if cfg.mode == PressureMode::Fixed {
        return ProfileState {
            calibrated: true,
            calibration_samples: cfg.warmup_samples,
            threshold_pct: cfg.threshold_pct,
            baseline_pct: None,
            target_pct: None,
            learned_samples: 0,
            learned_s: 0,
        };
    }
    let baseline_pct = learning.quantile(cfg);
    ProfileState {
        calibrated: learning.in_force.is_some(),
        calibration_samples: if learning.in_force.is_some() {
            cfg.warmup_samples
        } else {
            (learning.samples as usize).min(cfg.warmup_samples)
        },
        threshold_pct: learning.in_force.unwrap_or(cfg.floor_pct),
        baseline_pct,
        target_pct: baseline_pct.map(|q| cfg.target_for(q)),
        learned_samples: learning.samples,
        learned_s: learning.observed_ms / 1000,
    }
}

/// One attendance profile's learned state, for publishing the profile that
/// is not being observed.
#[derive(Debug, Clone, PartialEq)]
pub struct ProfileState {
    pub calibrated: bool,
    pub calibration_samples: usize,
    /// The threshold in force, or the floor before calibration.
    pub threshold_pct: f64,
    /// The learned quantile, once warm.
    pub baseline_pct: Option<f64>,
    pub target_pct: Option<f64>,
    pub learned_samples: u64,
    pub learned_s: u64,
}

/// One profile as persisted.
#[derive(Debug, Clone, Default, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct LearnedProfile {
    /// Occupied histogram bins, by index, with their decayed weights.
    #[serde(default)]
    pub bins: BTreeMap<u16, f64>,
    #[serde(default)]
    pub samples: u64,
    #[serde(default)]
    pub observed_ms: u64,
    pub threshold_in_force: Option<f64>,
    /// A baseline carried over from a schema 2 or 3 cache, converted to a
    /// starting threshold when it is restored. Never written by a save.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub legacy_baseline_pct: Option<f64>,
    /// The threshold came from such a baseline and the histogram has not yet
    /// learned enough to lower it.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub migrated: bool,
}

impl LearnedProfile {
    fn legacy(baseline: Option<f64>) -> Self {
        Self {
            legacy_baseline_pct: baseline,
            ..Self::default()
        }
    }

    /// Whether anything here could become a calibration.
    pub fn is_empty(&self) -> bool {
        self.threshold_in_force.is_none() && self.legacy_baseline_pct.is_none()
    }
}

/// One device's learning, as persisted.
#[derive(Debug, Clone, Default, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct PressureSnapshot {
    pub attended: LearnedProfile,
    pub unattended: LearnedProfile,
}

impl PressureSnapshot {
    pub fn is_empty(&self) -> bool {
        self.attended.is_empty() && self.unattended.is_empty()
    }
}

/// Learned histograms for every device, keyed by resource id.
///
/// Persistence here is best-effort by design. This file is a cache of something
/// the daemon can re-derive by watching for a while, so a missing, unreadable or
/// corrupt one must cost a warm start and nothing more — never a failed
/// startup.
#[derive(Debug, Clone, Default, serde::Serialize, serde::Deserialize)]
pub struct PressureStore {
    pub schema: u32,
    /// The schema of the file this was read from, when it was migrated.
    /// Cleared by the next save, which writes the current schema.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub migrated_from: Option<u32>,
    pub devices: BTreeMap<String, StoredCalibration>,
}

/// A device's saved learning, restored only for the same GPU and ignore
/// list. Nothing else decides what the histogram measured: the quantile,
/// margin, floor, cap, half-life and rise rate are applied to it afterwards.
#[derive(Debug, Clone, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct StoredCalibration {
    /// The GPU's UUID selector. Index selectors are not cached. Caches
    /// written by earlier versions hold `<uuid>@<pci>`, which still matches
    /// the same UUID at any PCI address.
    pub identity: String,
    pub ignored_processes: Vec<String>,
    pub learned: PressureSnapshot,
}

/// A baseline per profile, as schemas 2 and 3 saved it.
#[derive(serde::Deserialize)]
struct LegacyBaselines {
    attended_pct: Option<f64>,
    unattended_pct: Option<f64>,
}

/// Schema 3 keyed a frozen baseline on the estimator settings; schema 2 on
/// the whole tracker configuration and every borrower port. Neither key
/// means anything to a histogram, so only identity, ignore list and
/// baselines are read.
#[derive(serde::Deserialize)]
struct LegacyCalibration {
    identity: String,
    ignored_processes: Vec<String>,
    baseline: LegacyBaselines,
}

#[derive(serde::Deserialize)]
struct LegacyStore {
    devices: BTreeMap<String, LegacyCalibration>,
}

/// A real file holds a few KiB per GPU.
pub const MAX_STORE_BYTES: u64 = 1024 * 1024;

/// What reading the saved calibration file found.
#[derive(Debug)]
pub enum StoreRead {
    Loaded(PressureStore),
    Absent,
    Unreadable,
    TooLarge,
    Invalid,
}

impl StoreRead {
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::Loaded(_) => "loaded",
            Self::Absent => "absent",
            Self::Unreadable => "unreadable",
            Self::TooLarge => "too_large",
            Self::Invalid => "invalid",
        }
    }
}

impl PressureStore {
    pub const SCHEMA: u32 = 4;

    /// Read the file through one handle and stop past [`MAX_STORE_BYTES`],
    /// so a file that grows between a size check and the read cannot slip
    /// past the bound. A schema 2 or 3 file is migrated in memory and says so
    /// in `migrated_from` until the next save rewrites it.
    pub fn read(path: &std::path::Path) -> StoreRead {
        use std::io::Read;
        let file = match crate::secure_file::open_regular_read(path) {
            Ok(file) => file,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return StoreRead::Absent,
            Err(_) => return StoreRead::Unreadable,
        };
        let mut bytes = Vec::new();
        if file
            .take(MAX_STORE_BYTES + 1)
            .read_to_end(&mut bytes)
            .is_err()
        {
            return StoreRead::Unreadable;
        }
        if bytes.len() as u64 > MAX_STORE_BYTES {
            return StoreRead::TooLarge;
        }
        Self::parse(&bytes).map_or(StoreRead::Invalid, StoreRead::Loaded)
    }

    fn parse(bytes: &[u8]) -> Option<Self> {
        #[derive(serde::Deserialize)]
        struct Schema {
            schema: u32,
        }
        match serde_json::from_slice::<Schema>(bytes).ok()?.schema {
            Self::SCHEMA => serde_json::from_slice(bytes).ok(),
            legacy @ (2 | 3) => {
                let old: LegacyStore = serde_json::from_slice(bytes).ok()?;
                let devices = old
                    .devices
                    .into_iter()
                    .map(|(id, d)| {
                        let calibration = StoredCalibration {
                            identity: d.identity,
                            ignored_processes: d.ignored_processes,
                            learned: PressureSnapshot {
                                attended: LearnedProfile::legacy(d.baseline.attended_pct),
                                unattended: LearnedProfile::legacy(d.baseline.unattended_pct),
                            },
                        };
                        (id, calibration)
                    })
                    .collect();
                Some(Self {
                    schema: Self::SCHEMA,
                    migrated_from: Some(legacy),
                    devices,
                })
            }
            _ => None,
        }
    }

    /// The saved calibrations, or none. Anything but a missing file is
    /// logged, because it silently costs every GPU a recalibration.
    pub fn load(path: &std::path::Path) -> Self {
        match Self::read(path) {
            StoreRead::Loaded(store) => store,
            StoreRead::Absent => Self::default(),
            refused => {
                tracing::warn!(
                    path = %path.display(),
                    state = refused.as_str(),
                    "saved GPU calibration ignored; calibrating from new samples"
                );
                Self::default()
            }
        }
    }

    /// Write atomically and durably — a torn file read on the next boot
    /// would be a corrupt prior, which is worse than no prior at all.
    /// Compact rather than indented: the histograms are most of the file,
    /// and the diagnostics export shows it indented.
    pub fn save(&mut self, path: &std::path::Path) -> std::io::Result<()> {
        self.schema = Self::SCHEMA;
        self.migrated_from = None;
        crate::durable::write(path, &serde_json::to_vec(self)?)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn cfg() -> PressureConfig {
        PressureConfig {
            sustain_s: 10,
            release_after_s: 10,
            warmup_samples: 4,
            floor_pct: 15.0,
            margin_pct: 20.0,
            ..Default::default()
        }
    }

    /// Feed `n` samples five seconds apart, returning the clock afterwards.
    fn feed(t: &mut PressureTracker, from_ms: u64, n: usize, pct: f64, unattended: bool) -> u64 {
        let mut now = from_ms;
        for _ in 0..n {
            t.observe(now, pct, unattended);
            now += 5_000;
        }
        now
    }

    const HOUR_OF_SAMPLES: usize = 720;

    #[test]
    fn warmup_uses_the_floor_and_reports_no_baseline() {
        let mut t = PressureTracker::new(cfg());
        let v = t.observe(0, 9.0, false);
        assert_eq!(v.baseline_pct, None);
        assert_eq!(v.target_pct, None);
        assert_eq!(v.threshold_pct, 15.0);
        assert!(!v.engaged);
    }

    #[test]
    fn a_noisy_desktop_learns_a_threshold_above_its_own_load() {
        let mut t = PressureTracker::new(cfg());
        // Busy compositing machine: 14% is *idle* here.
        let now = feed(&mut t, 0, 20, 14.0, false);
        let v = t.observe(now, 14.0, false);
        // The bin's upper edge, so the learned value is never below a reading.
        assert_eq!(v.baseline_pct, Some(14.5));
        assert_eq!(v.target_pct, Some(34.5));
        assert_eq!(v.threshold_pct, 34.5);
        assert!(!v.engaged);
    }

    #[test]
    fn sustained_load_engages_only_after_the_sustain_window() {
        let mut t = PressureTracker::new(cfg());
        let mut now = feed(&mut t, 0, 20, 8.0, false); // quiet baseline
                                                       // A game starts. sustain_s is 10, samples are 5s apart.
        let first = t.observe(now, 95.0, false);
        assert!(!first.engaged, "must not engage on the first sample");
        now += 5_000;
        assert!(!t.observe(now, 95.0, false).engaged, "5s < 10s");
        now += 5_000;
        let v = t.observe(now, 95.0, false);
        assert!(v.engaged);
        assert_eq!(v.above_for_s, 10);
    }

    #[test]
    fn engaged_pressure_releases_only_after_its_own_grace_period() {
        let mut c = cfg();
        c.release_after_s = 20;
        let mut tracker = PressureTracker::new(c);
        let mut now = feed(&mut tracker, 0, 20, 8.0, false);
        tracker.observe(now, 95.0, false);
        now += 10_000;
        assert!(tracker.observe(now, 95.0, false).engaged);

        let below_since = now + 5_000;
        assert!(tracker.observe(below_since, 8.0, false).engaged);
        assert!(tracker.observe(below_since + 15_000, 8.0, false).engaged);
        assert!(!tracker.observe(below_since + 20_000, 8.0, false).engaged);
    }

    #[test]
    fn a_transient_spike_never_engages() {
        let mut t = PressureTracker::new(cfg());
        let mut now = feed(&mut t, 0, 20, 8.0, false);
        // One 5s sample of a thumbnail render. Evicting a 12 GiB model for
        // this would cost far more than it saves.
        now = {
            t.observe(now, 90.0, false);
            now + 5_000
        };
        let v = t.observe(now, 8.0, false);
        assert!(!v.engaged);
        assert_eq!(
            v.above_for_s, 0,
            "the timer resets on the first sample below"
        );
    }

    /// Replay days that alternate twelve hours at 5–14% with twelve hours at
    /// 25–39%, five seconds apart, calling `check` with each busy-band
    /// verdict and the day it fell on.
    fn replay_bimodal_days(
        t: &mut PressureTracker,
        days: usize,
        mut check: impl FnMut(usize, &PressureVerdict),
    ) -> u64 {
        let mut now = 0;
        for day in 0..days {
            for i in 0..12 * HOUR_OF_SAMPLES {
                t.observe(now, 5.0 + (i % 10) as f64, false);
                now += 5_000;
            }
            for i in 0..12 * HOUR_OF_SAMPLES {
                let v = t.observe(now, 25.0 + (i % 15) as f64, false);
                check(day, &v);
                now += 5_000;
            }
        }
        now
    }

    #[test]
    fn a_bimodal_day_converges_to_the_cap_and_stops_refusing_its_busy_band() {
        let mut t = PressureTracker::new(PressureConfig::default());
        let mut refused = [0; 4];
        let now = replay_bimodal_days(&mut t, 4, |day, v| {
            refused[day] += usize::from(v.engaged);
        });
        // The first calibration happened in the quiet morning, so the first
        // afternoon refuses while the threshold climbs toward the cap.
        assert!(refused[0] > 0);
        let refused_after_day_one: usize = refused[1..].iter().sum();
        assert_eq!(
            refused_after_day_one, 0,
            "the afternoon's ordinary load is learned, not refused"
        );
        let v = t.observe(now, 10.0, false);
        assert_eq!(v.target_pct, Some(40.0));
        assert_eq!(v.threshold_pct, 40.0);
        assert!(v.learned_s >= 4 * 24 * 3600 - 5);

        // Real work still engages: it runs far above the cap.
        let mut now = now + 5_000;
        let mut engaged = false;
        for _ in 0..4 {
            engaged = t.observe(now, 95.0, false).engaged;
            now += 5_000;
        }
        assert!(engaged, "15 s of sustained 95% load engages");
    }

    #[test]
    fn a_long_render_cannot_raise_the_threshold_above_the_cap() {
        let mut t = PressureTracker::new(PressureConfig::default());
        let now = feed(&mut t, 0, 60, 8.0, false);
        let calibrated = t.observe(now, 8.0, false);
        assert_eq!(calibrated.threshold_pct, 28.5);
        let mut now = now + 5_000;
        let mut highest: f64 = 0.0;
        // Six hours of render: the target hits the cap almost at once, and
        // the threshold climbs at most a point an hour toward it.
        for i in 0..6 * HOUR_OF_SAMPLES {
            let v = t.observe(now, 95.0, false);
            highest = highest.max(v.threshold_pct);
            if i >= 3 {
                assert!(v.engaged, "sample {i}: {v:?}");
            }
            now += 5_000;
        }
        assert!(highest <= 28.5 + 6.0 + 1e-9, "{highest}");
        // Two days of it reach the cap and stop there, still engaged.
        for _ in 0..42 * HOUR_OF_SAMPLES {
            let v = t.observe(now, 95.0, false);
            assert!(v.threshold_pct <= 40.0 && v.engaged, "{v:?}");
            now += 5_000;
        }
        let v = t.observe(now, 95.0, false);
        assert_eq!(v.threshold_pct, 40.0);
        assert_eq!(v.baseline_pct, Some(95.5));
        assert_eq!(
            v.calibration_hint.as_deref(),
            Some("learned load is above the calibration cap; target held at cap")
        );
    }

    #[test]
    fn the_threshold_rises_at_most_its_allowance_per_observed_hour_and_falls_at_once() {
        let mut t = PressureTracker::new(cfg());
        // 9.75% sits in the bin whose upper edge is 10: a target of 30.
        let now = feed(&mut t, 0, 4, 9.75, false);
        let mut now = now;
        let mut v = t.observe(now, 9.75, false);
        assert_eq!(v.threshold_pct, 30.0);
        // A busier hour: the target jumps to the cap of 40.
        for _ in 0..HOUR_OF_SAMPLES {
            now += 5_000;
            v = t.observe(now, 25.0, false);
        }
        assert_eq!(v.target_pct, Some(40.0));
        assert!(v.threshold_pct > 30.9, "{v:?}");
        assert!(v.threshold_pct <= 31.0 + 1e-9, "{v:?}");

        // Quiet again. Until quiet outweighs three quarters of the
        // histogram the target holds and the threshold keeps climbing; once
        // the target falls below the threshold, the threshold follows at once.
        let mut fell = false;
        for _ in 0..10 * HOUR_OF_SAMPLES {
            now += 5_000;
            let before = v.threshold_pct;
            v = t.observe(now, 2.0, false);
            let target = v.target_pct.unwrap();
            if target < before {
                assert_eq!(v.threshold_pct, target);
                assert!(target <= 30.0, "{v:?}");
                fell = true;
                break;
            }
            assert!(v.threshold_pct - before <= 5.0 / 3600.0 + 1e-12);
        }
        assert!(fell, "{v:?}");
    }

    #[test]
    fn weights_halve_after_one_half_life_of_observed_time_and_gaps_do_not_decay() {
        let mut c = cfg();
        c.learn_half_life_h = 10.0 / 3600.0; // ten seconds
        c.max_gap_ms = 15_000;
        // Calibrated from the first reading, so interruptions keep it.
        c.warmup_samples = 1;
        let mut t = PressureTracker::new(c);
        t.observe(0, 50.0, false);
        t.observe(10_000, 10.0, false);
        let halved = |t: &PressureTracker| t.attended.histogram.weights[bin_of(50.0)];
        assert!((halved(&t) - 0.5).abs() < 1e-12, "{}", halved(&t));
        assert_eq!(t.attended.histogram.weights[bin_of(10.0)], 1.0);
        assert_eq!(t.attended.observed_ms, 10_000);

        // A minute without a reading is not observed time.
        t.observe(70_000, 10.0, false);
        let before = halved(&t);
        assert!((before - 0.5).abs() < 1e-12);
        assert_eq!(t.attended.histogram.weights[bin_of(10.0)], 2.0);
        assert_eq!(t.attended.observed_ms, 10_000);

        // Nor is time spent observing the other profile.
        t.observe(75_000, 1.0, true);
        t.observe(80_000, 10.0, false);
        assert_eq!(halved(&t), before);
        assert_eq!(t.attended.samples, 4);
    }

    #[test]
    fn attended_and_unattended_learn_and_calibrate_independently() {
        let mut t = PressureTracker::new(cfg());
        // Desk time is noisy...
        let now = feed(&mut t, 0, 20, 30.0, false);
        assert!(t.profile_state(false).calibrated);
        assert!(!t.profile_state(true).calibrated);
        // ...but a locked machine composites almost nothing.
        let first = t.observe(now, 1.0, true);
        assert!(!first.calibrated, "the locked profile calibrates itself");
        assert_eq!(first.calibration_samples, 1);
        let now = feed(&mut t, now + 5_000, 20, 1.0, true);

        let locked = t.observe(now, 1.0, true);
        assert_eq!(locked.baseline_pct, Some(1.5));
        assert_eq!(locked.threshold_pct, 21.5);

        let attended = t.observe(now + 5_000, 30.0, false);
        assert_eq!(attended.baseline_pct, Some(30.5));
        assert_eq!(attended.threshold_pct, 40.0, "held at the cap");
    }

    #[test]
    fn switching_profile_and_failed_reads_keep_an_unfinished_first_calibration() {
        let mut t = PressureTracker::new(cfg());
        let now = feed(&mut t, 0, 4, 8.0, false);
        let now = feed(&mut t, now, 2, 1.0, true);
        assert_eq!(t.profile_state(true).calibration_samples, 2);
        let v = t.observe(now, 8.0, false);
        assert!(v.calibrated, "the completed attended calibration stays");
        assert_eq!(t.profile_state(true).calibration_samples, 2);
        // A failed read and a long gap interrupt; neither discards.
        assert!(!t.observe(now + 5_000, f64::NAN, true).calibrated);
        assert!(!t.observe(now + 10_000, 1.0, true).calibrated);
        t.interrupt();
        let v = t.observe(now + 100_000, 1.0, true);
        assert!(v.calibrated, "four valid readings, however interrupted");
        assert_eq!(v.learned_samples, 4);
    }

    #[test]
    fn a_failed_read_ages_nothing() {
        let mut c = cfg();
        c.learn_half_life_h = 10.0 / 3600.0; // ten seconds
        c.warmup_samples = 1;
        let mut t = PressureTracker::new(c);
        t.observe(0, 50.0, false);
        t.observe(5_000, f64::NAN, false);
        t.observe(10_000, 10.0, false);
        assert_eq!(t.attended.histogram.weights[bin_of(50.0)], 1.0);
        assert_eq!(t.attended.observed_ms, 0);
        t.observe(15_000, 101.0, false);
        t.observe(20_000, 10.0, false);
        assert_eq!(t.attended.histogram.weights[bin_of(50.0)], 1.0);
        assert_eq!(t.attended.samples, 3);
    }

    #[test]
    fn an_overnight_render_is_detected_on_an_unattended_machine() {
        let mut t = PressureTracker::new(cfg());
        let mut now = feed(&mut t, 0, 20, 1.0, true); // locked and quiet
                                                      // A render left running. Against the *locked* threshold this is
                                                      // unmistakable; against a desk-time one it might not have been.
        for _ in 0..3 {
            t.observe(now, 60.0, true);
            now += 5_000;
        }
        assert!(t.observe(now, 60.0, true).engaged);
    }

    #[test]
    fn fixed_mode_is_ready_without_calibration() {
        let mut c = cfg();
        c.mode = PressureMode::Fixed;
        c.threshold_pct = 70.0;
        let mut t = PressureTracker::new(c);
        let now = feed(&mut t, 0, 20, 30.0, false);
        let v = t.observe(now, 50.0, false);
        assert_eq!(v.threshold_pct, 70.0);
        assert_eq!(v.baseline_pct, None);
        assert_eq!(v.target_pct, None);
        assert!(v.calibrated);
        assert!(!v.engaged, "50 is below the override");
        assert!(t.snapshot().is_empty(), "fixed mode learns nothing");
    }

    #[test]
    fn a_retired_adaptive_mode_runs_as_calibrated() {
        assert_eq!(PressureMode::Adaptive.effective(), PressureMode::Calibrated);
        assert_eq!(PressureMode::Fixed.effective(), PressureMode::Fixed);
    }

    #[test]
    fn a_learned_threshold_survives_a_restart() {
        let mut before = PressureTracker::new(cfg());
        let now = feed(&mut before, 0, 20, 16.0, false);
        let learned = before.observe(now, 16.0, false);
        assert_eq!(learned.threshold_pct, 36.5);

        // Restart: new tracker, clock back to zero, prior restored.
        let snap = before.snapshot();
        let mut after = PressureTracker::new(cfg());
        assert_eq!(
            after.observe(0, 16.0, false).threshold_pct,
            15.0,
            "without a prior it would fall back to the floor and veto a busy desktop"
        );

        let mut after = PressureTracker::new(cfg());
        assert_eq!(after.seed(&snap), [Restored::Learned, Restored::Empty]);
        let v = after.observe(0, 16.0, false);
        assert_eq!(v.baseline_pct, Some(16.5));
        assert_eq!(v.threshold_pct, 36.5, "warm immediately after a restart");
        assert_eq!(v.learned_samples, 22);
        assert!(!v.engaged);
    }

    #[test]
    fn tuning_keeps_a_saved_histogram_and_bounds_its_threshold() {
        let mut before = PressureTracker::new(cfg());
        let now = feed(&mut before, 0, 20, 16.0, false);
        before.observe(now, 16.0, false);
        let snap = before.snapshot();
        for tuned in [
            PressureConfig {
                learn_quantile: 0.5,
                ..cfg()
            },
            PressureConfig {
                learn_half_life_h: 1.0,
                raise_pct_per_h: 5.0,
                ..cfg()
            },
            PressureConfig {
                margin_pct: 10.0,
                ..cfg()
            },
            PressureConfig {
                cap_pct: 30.0,
                ..cfg()
            },
        ] {
            let mut after = PressureTracker::new(tuned.clone());
            assert_eq!(after.seed(&snap), [Restored::Learned, Restored::Empty]);
            assert_eq!(after.attended.samples, 21, "{tuned:?}");
            let v = after.observe(0, 16.0, false);
            assert!(v.calibrated, "{tuned:?}");
            assert!(v.threshold_pct <= tuned.cap_pct, "{tuned:?}: {v:?}");
        }
    }

    #[test]
    fn a_legacy_baseline_lends_at_once_and_its_histogram_takes_over_after_warmup() {
        let snap = PressureSnapshot {
            attended: LearnedProfile::legacy(Some(12.0)),
            unattended: LearnedProfile::legacy(Some(35.0)),
        };
        let mut t = PressureTracker::new(cfg());
        assert_eq!(
            t.seed(&snap),
            [Restored::LegacyBaseline, Restored::LegacyBaseline]
        );
        let v = t.observe(0, 2.0, false);
        assert!(v.calibrated, "no new samples needed");
        assert_eq!(v.threshold_pct, 32.0, "12 + 20 margin");
        assert_eq!(v.baseline_pct, None, "nothing learned yet");
        assert_eq!(
            t.profile_state(true).threshold_pct,
            40.0,
            "35 + 20 is held at the cap"
        );
        // Below warmup the starting threshold stands, and so it does past
        // warmup while the histogram is young.
        let now = feed(&mut t, 5_000, 1, 2.0, false);
        assert_eq!(t.observe(now, 2.0, false).threshold_pct, 32.0);
        let v = t.observe(now + 5_000, 2.0, false);
        assert_eq!(v.baseline_pct, Some(2.5));
        assert_eq!(v.target_pct, Some(22.5));
        assert_eq!(v.threshold_pct, 32.0);
        let saved = t.snapshot();
        assert_eq!(saved.attended.threshold_in_force, Some(32.0));
        assert_eq!(saved.attended.legacy_baseline_pct, None);
        assert!(saved.attended.migrated);
        assert_eq!(
            saved.unattended.threshold_in_force,
            Some(40.0),
            "a converted baseline is saved as a threshold"
        );
    }

    #[test]
    fn a_migrated_threshold_is_not_lowered_until_a_day_has_been_observed() {
        let snap = PressureSnapshot {
            attended: LearnedProfile::legacy(Some(13.0)),
            unattended: LearnedProfile::default(),
        };
        let mut t = PressureTracker::new(PressureConfig::default());
        t.seed(&snap);
        // Eight quiet minutes: warm, with a target at the floor.
        let now = feed(&mut t, 0, 96, 2.0, false);
        let v = t.observe(now, 2.0, false);
        assert_eq!(v.target_pct, Some(25.0));
        assert_eq!(v.threshold_pct, 33.0, "13 + 20, kept");

        // A restart inside the day keeps the protection and the clock.
        let saved = t.snapshot();
        assert!(saved.attended.migrated);
        let mut t = PressureTracker::new(PressureConfig::default());
        assert_eq!(t.seed(&saved), [Restored::Learned, Restored::Empty]);
        let mut now = 0;
        let mut v = t.observe(now, 2.0, false);
        while v.learned_s < 24 * 3600 - 10 {
            now += 5_000;
            v = t.observe(now, 2.0, false);
            assert_eq!(v.threshold_pct, 33.0, "{v:?}");
        }
        // Once a day is observed the histogram's target applies at once.
        for _ in 0..3 {
            now += 5_000;
            v = t.observe(now, 2.0, false);
        }
        assert_eq!(v.threshold_pct, 25.0);
        assert!(!t.snapshot().attended.migrated);
    }

    #[test]
    fn a_migrated_threshold_still_rises_at_the_limited_rate() {
        let snap = PressureSnapshot {
            attended: LearnedProfile::legacy(Some(5.0)),
            unattended: LearnedProfile::default(),
        };
        let mut t = PressureTracker::new(PressureConfig::default());
        t.seed(&snap);
        let now = feed(&mut t, 0, HOUR_OF_SAMPLES, 30.0, false);
        let v = t.observe(now, 30.0, false);
        assert_eq!(v.target_pct, Some(40.0));
        assert!(
            v.threshold_pct > 25.0 && v.threshold_pct <= 26.0 + 1e-9,
            "{v:?}"
        );
    }

    #[test]
    fn an_implausible_saved_weight_is_an_invalid_profile() {
        for bins in [
            BTreeMap::from([(4, MAX_SAVED_WEIGHT * 2.0)]),
            BTreeMap::from([(4, MAX_SAVED_WEIGHT), (5, MAX_SAVED_WEIGHT)]),
        ] {
            let mut t = PressureTracker::new(cfg());
            let saved = LearnedProfile {
                bins,
                threshold_in_force: Some(30.0),
                ..Default::default()
            };
            assert_eq!(
                t.seed(&PressureSnapshot {
                    attended: saved,
                    unattended: LearnedProfile::default(),
                }),
                [Restored::Invalid, Restored::Empty]
            );
        }
        // A day at the fastest cadence and longest half-life is well inside.
        let mut c = cfg();
        c.learn_half_life_h = 720.0;
        let mut t = PressureTracker::new(c.clone());
        let mut now = 0;
        for _ in 0..86_400 {
            t.observe(now, 5.0, false);
            now += 1_000;
        }
        let mut restored = PressureTracker::new(c);
        assert_eq!(
            restored.seed(&t.snapshot()),
            [Restored::Learned, Restored::Empty]
        );
    }

    #[test]
    fn store_round_trips_and_tolerates_a_corrupt_file() {
        // Saving creates the directory, which on Windows must be inside the
        // protected folder that tests stand in for with their own.
        let dir = crate::secure_file::test_root()
            .join(format!("vacancyd-pressure-{}", std::process::id()));
        let path = dir.join("state.json");
        let mut tracker = PressureTracker::new(cfg());
        feed(&mut tracker, 0, 20, 12.0, false);
        let mut s = PressureStore::default();
        s.devices.insert(
            "gpu0".into(),
            StoredCalibration {
                identity: "GPU-example@0000:01:00.0".into(),
                ignored_processes: vec![],
                learned: tracker.snapshot(),
            },
        );
        s.save(&path).unwrap();
        let loaded = PressureStore::load(&path);
        assert_eq!(loaded.schema, PressureStore::SCHEMA);
        assert_eq!(loaded.devices["gpu0"], s.devices["gpu0"]);
        let bins = &loaded.devices["gpu0"].learned.attended.bins;
        assert_eq!(bins.keys().collect::<Vec<_>>(), [&24]);
        assert!((bins[&24] - 20.0).abs() < 0.01, "{bins:?}");
        assert!(
            std::fs::metadata(&path).unwrap().len() < 512,
            "a saved histogram is sparse and compact"
        );

        let mut old = serde_json::to_value(&s).unwrap();
        old["schema"] = 1.into();
        std::fs::write(&path, old.to_string()).unwrap();
        assert!(matches!(PressureStore::read(&path), StoreRead::Invalid));
        assert!(PressureStore::load(&path).devices.is_empty());

        // A half-written file must degrade to "no prior", never to a panic.
        std::fs::write(&path, b"{\"devices\": {\"gpu0\": ").unwrap();
        assert!(PressureStore::load(&path).devices.is_empty());
        // So must one that was never there, or one too large to be real.
        assert!(matches!(
            PressureStore::read(&dir.join("absent.json")),
            StoreRead::Absent
        ));
        std::fs::write(&path, vec![b' '; MAX_STORE_BYTES as usize + 1]).unwrap();
        assert!(matches!(PressureStore::read(&path), StoreRead::TooLarge));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[cfg(unix)]
    #[test]
    fn store_refuses_symbolic_links_and_named_pipes() {
        use std::io::Write;
        use std::os::unix::{ffi::OsStrExt, fs::OpenOptionsExt};
        let directory = std::env::temp_dir().join(format!(
            "vacancyd-cache-read-{}",
            crate::setup::nonce().unwrap()
        ));
        std::fs::create_dir(&directory).unwrap();
        let regular = directory.join("regular.json");
        std::fs::write(&regular, br#"{"schema":4,"devices":{}}"#).unwrap();
        let link = directory.join("linked.json");
        std::os::unix::fs::symlink(&regular, &link).unwrap();
        assert!(matches!(PressureStore::read(&link), StoreRead::Unreadable));

        let pipe = directory.join("pipe.json");
        let name = std::ffi::CString::new(pipe.as_os_str().as_bytes()).unwrap();
        // SAFETY: the path is NUL-terminated and belongs to this test's directory.
        assert_eq!(unsafe { libc::mkfifo(name.as_ptr(), 0o600) }, 0);
        let writer_path = pipe.clone();
        let writer = std::thread::spawn(move || {
            // Release an old blocking reader, so the regression fails instead of hanging.
            std::thread::sleep(std::time::Duration::from_millis(50));
            if let Ok(mut file) = std::fs::OpenOptions::new()
                .write(true)
                .custom_flags(libc::O_NONBLOCK)
                .open(writer_path)
            {
                let _ = file.write_all(br#"{"schema":4,"devices":{}}"#);
            }
        });
        let result = PressureStore::read(&pipe);
        writer.join().unwrap();
        assert!(matches!(result, StoreRead::Unreadable));
        std::fs::remove_dir_all(directory).unwrap();
    }

    #[test]
    fn a_full_histogram_stays_a_few_kib() {
        let mut t = PressureTracker::new(cfg());
        let mut now = 0;
        for unattended in [false, true] {
            for i in 0..2_000 {
                t.observe(now, (i % 201) as f64 / 2.0, unattended);
                now += 5_000;
            }
        }
        let mut store = PressureStore::default();
        store.devices.insert(
            "gpu0".into(),
            StoredCalibration {
                identity: "GPU-example@0000:01:00.0".into(),
                ignored_processes: vec![],
                learned: t.snapshot(),
            },
        );
        let dir = crate::secure_file::test_root()
            .join(format!("vacancyd-pressure-size-{}", std::process::id()));
        let path = dir.join("state.json");
        store.save(&path).unwrap();
        let size = std::fs::metadata(&path).unwrap().len();
        let _ = std::fs::remove_dir_all(&dir);
        assert!(size < 8 * 1024, "{size} bytes");
    }

    /// The device entries schemas 2 and 3 wrote, keyed on settings a
    /// histogram does not use.
    fn legacy_cache(schema: u32) -> serde_json::Value {
        let key = if schema == 2 {
            serde_json::json!({
                "mode": "calibrated", "cap_pct": 40.0, "threshold_pct": 0.0,
                "margin_pct": 20.0, "floor_pct": 25.0, "sustain_s": 15,
                "release_after_s": 120, "baseline_window_s": 21600,
                "baseline_quantile": 0.25, "warmup_samples": 60,
                "max_gap_ms": 15000
            })
        } else {
            serde_json::json!({
                "mode": "adaptive", "baseline_quantile": 0.25,
                "baseline_window_s": 21600, "warmup_samples": 60
            })
        };
        let mut device = serde_json::json!({
            "identity": "GPU-aaaa@0000:01:00.0",
            "ignored_processes": ["wallpaper"],
            "borrower_ports": [8189, 11435],
            "baseline": { "attended_pct": 13.0, "unattended_pct": null }
        });
        device[if schema == 2 { "config" } else { "estimator" }] = key;
        serde_json::json!({ "schema": schema, "devices": { "gpu0": device } })
    }

    #[test]
    fn schema_2_and_3_caches_keep_their_baselines_through_the_upgrade() {
        for schema in [2, 3] {
            let store = PressureStore::parse(legacy_cache(schema).to_string().as_bytes()).unwrap();
            // Diagnostics show the migrated layout and where it came from.
            assert_eq!(store.schema, PressureStore::SCHEMA);
            assert_eq!(store.migrated_from, Some(schema));
            let gpu0 = &store.devices["gpu0"];
            assert_eq!(gpu0.ignored_processes, ["wallpaper"]);
            assert_eq!(gpu0.learned.attended, LearnedProfile::legacy(Some(13.0)));
            assert!(gpu0.learned.unattended.is_empty());

            let mut t = PressureTracker::new(PressureConfig::default());
            assert_eq!(
                t.seed(&gpu0.learned),
                [Restored::LegacyBaseline, Restored::Empty]
            );
            let v = t.observe(0, 5.0, false);
            assert!(v.calibrated, "schema {schema}");
            assert_eq!(v.threshold_pct, 33.0, "schema {schema}");
        }
    }

    #[test]
    fn a_busy_first_calibration_starts_at_the_cap_and_still_engages() {
        let mut tracker = PressureTracker::new(cfg());
        let now = feed(&mut tracker, 0, 3, 95.0, false);
        let v = tracker.observe(now, 95.0, false);
        assert!(v.calibrated);
        assert_eq!(v.threshold_pct, 40.0);
        assert_eq!(
            v.calibration_hint.as_deref(),
            Some("learned load is above the calibration cap; target held at cap")
        );
        let now = feed(&mut tracker, now + 5_000, 2, 95.0, false);
        assert!(tracker.observe(now, 95.0, false).engaged);
    }

    #[test]
    fn an_unfinished_calibration_is_not_persistable() {
        let mut tracker = PressureTracker::new(cfg());
        let now = feed(&mut tracker, 0, 3, 8.0, false);
        assert!(!tracker.observe(now, 8.0, true).calibrated);
        assert!(tracker.snapshot().is_empty());
    }

    #[test]
    fn bad_samples_and_bad_priors_never_look_ready() {
        for invalid in [f64::NAN, f64::INFINITY, -1.0, 101.0] {
            let mut priors = vec![
                LearnedProfile {
                    threshold_in_force: Some(invalid),
                    ..Default::default()
                },
                LearnedProfile::legacy(Some(invalid)),
            ];
            if invalid != 101.0 {
                // Any finite, non-negative weight is valid.
                priors.push(LearnedProfile {
                    threshold_in_force: Some(30.0),
                    bins: BTreeMap::from([(4, invalid)]),
                    ..Default::default()
                });
            }
            for saved in priors {
                let mut t = PressureTracker::new(cfg());
                let restored = t.seed(&PressureSnapshot {
                    attended: saved.clone(),
                    unattended: LearnedProfile::default(),
                });
                assert_eq!(restored, [Restored::Invalid, Restored::Empty], "{saved:?}");
                assert!(!t.observe(0, invalid, false).calibrated);
                assert!(t.snapshot().is_empty());
            }
        }
        let mut t = PressureTracker::new(cfg());
        let out_of_range = LearnedProfile {
            threshold_in_force: Some(30.0),
            bins: BTreeMap::from([(BINS as u16, 1.0)]),
            ..Default::default()
        };
        assert_eq!(
            t.seed(&PressureSnapshot {
                attended: out_of_range,
                unattended: LearnedProfile::default(),
            }),
            [Restored::Invalid, Restored::Empty]
        );
    }

    #[test]
    fn failed_reads_and_long_gaps_do_not_count_toward_release() {
        let mut t = PressureTracker::new(cfg());
        let now = feed(&mut t, 0, 4, 8.0, false);
        t.observe(now, 95.0, false);
        assert!(t.observe(now + 10_000, 95.0, false).engaged);
        t.observe(now + 15_000, 8.0, false);
        t.interrupt();
        assert!(t.observe(now + 25_000, 8.0, false).engaged);
        assert!(t.observe(now + 100_000, 8.0, false).engaged);
        assert!(!t.observe(now + 110_000, 8.0, false).engaged);
    }

    #[test]
    fn a_summary_reports_quantiles_weight_and_thresholds() {
        let mut t = PressureTracker::new(cfg());
        assert!(t.summary(false).quantiles.is_empty());
        let now = feed(&mut t, 0, 10, 8.0, false);
        feed(&mut t, now, 10, 30.0, false);
        let summary = t.summary(false);
        assert_eq!(summary.samples, 20);
        assert_eq!(summary.quantiles.len(), SUMMARY_QUANTILES.len());
        assert_eq!(summary.quantiles[0], QuantilePoint { q: 0.1, pct: 8.5 });
        assert_eq!(summary.quantiles[6], QuantilePoint { q: 0.99, pct: 30.5 });
        assert!(summary.total_weight > 19.99 && summary.total_weight <= 20.0);
        assert_eq!(summary.target_pct, Some(40.0));
        assert!(summary.threshold_in_force_pct.is_some());
        assert_eq!(t.summary(true).samples, 0);
    }
}
