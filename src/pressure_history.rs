//! A bounded, always-on record of recent GPU pressure samples per resource.
//!
//! It exists so an owner who notices an odd refusal can export what led up
//! to it without having had debug logging on at the time. It is memory only
//! and starts empty after a restart.
//!
//! # Size
//!
//! Each resource keeps [`SPAN`] of samples at its pressure poll cadence:
//! 4,320 samples at the default `owner.gpu_pressure.poll_ms` of 5,000. The
//! poll floor of one second caps it at 21,600 samples. A record is at most
//! 56 bytes, so a resource holds about 240 KiB at the default cadence and at
//! most about 1.2 MiB. An unknown reading's reason, at most
//! [`MAX_REASON_CHARS`] characters, is shared with the record before it when
//! it repeats, so a lasting failure adds no text per sample. A resource
//! without pressure tracking allocates nothing.

use std::collections::VecDeque;
use std::sync::Arc;
use std::time::{Duration, SystemTime};

use serde::Serialize;

/// How far back the record reaches at the configured poll cadence.
pub const SPAN: Duration = Duration::from_secs(6 * 60 * 60);
/// The fastest cadence the pressure sampler runs at, whatever is configured.
const MIN_POLL_MS: u64 = 1_000;

/// Samples kept for a pressure poll every `poll_ms` milliseconds.
pub fn capacity_for(poll_ms: u64) -> usize {
    let span_ms = SPAN.as_millis() as u64;
    span_ms.div_ceil(poll_ms.max(MIN_POLL_MS)) as usize
}

/// The attendance profile a sample was learned and judged in.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Profile {
    Attended,
    Unattended,
}

impl Profile {
    pub fn of(unattended: bool) -> Self {
        if unattended {
            Self::Unattended
        } else {
            Self::Attended
        }
    }

    pub fn as_str(self) -> &'static str {
        match self {
            Self::Attended => "attended",
            Self::Unattended => "unattended",
        }
    }
}

/// The longest reason an unknown reading keeps.
pub const MAX_REASON_CHARS: usize = 200;

/// One pressure sample and the decision it produced, or why there was no
/// usable reading. Serialized flat, with `quality` saying which.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct PressureRecord {
    /// When the sample was taken, in milliseconds since the Unix epoch.
    pub at_unix_ms: u64,
    #[serde(flatten)]
    pub reading: Reading,
}

#[derive(Debug, Clone, PartialEq, Serialize)]
#[serde(tag = "quality", rename_all = "snake_case")]
pub enum Reading {
    Ready {
        /// Utilisation by work vacancyd did not admit, in percent of the
        /// device.
        non_lender_pct: f64,
        profile: Profile,
        /// The engage threshold in force for this sample.
        threshold_pct: f64,
        /// The learned floor, absent while the profile is still calibrating.
        baseline_pct: Option<f64>,
        calibrated: bool,
        engaged: bool,
    },
    /// The reading was invalidated; admission refuses on it.
    Unknown {
        #[serde(serialize_with = "serialize_shared")]
        reason: Arc<str>,
    },
}

fn serialize_shared<S: serde::Serializer>(
    text: &Arc<str>,
    serializer: S,
) -> Result<S::Ok, S::Error> {
    serializer.serialize_str(text)
}

fn now_unix_ms() -> u64 {
    SystemTime::now()
        .duration_since(SystemTime::UNIX_EPOCH)
        .map_or(0, |since| since.as_millis() as u64)
}

impl PressureRecord {
    pub fn now(verdict: &crate::plugin::pressure::PressureVerdict, profile: Profile) -> Self {
        Self {
            at_unix_ms: now_unix_ms(),
            reading: Reading::Ready {
                non_lender_pct: verdict.non_lender_pct,
                profile,
                threshold_pct: verdict.threshold_pct,
                baseline_pct: verdict.baseline_pct,
                calibrated: verdict.calibrated,
                engaged: verdict.engaged,
            },
        }
    }
}

/// One process counted in a sample's non-lender percentage. Process names
/// are the owner's private activity: they appear in the diagnostics export
/// only, never in `/status`, metrics or logs below debug.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct NonLenderProcess {
    pub name: String,
    pub pct: f64,
}

/// Processes kept per sample, busiest first.
pub const TOP_PROCESSES: usize = 5;
/// A longer process name is cut, so one name cannot bloat the export.
const MAX_PROCESS_NAME: usize = 260;

/// The busiest non-lender processes in `counted`, summed per process,
/// busiest first, at most [`TOP_PROCESSES`]. A process whose name could not
/// be read is shown by its id.
pub fn top_processes(
    counted: &[&crate::probes::gpu_procs::ProcGpuSample],
    name_of: impl Fn(u32) -> Option<String>,
) -> Vec<NonLenderProcess> {
    let mut by_pid: Vec<(u32, f64)> = Vec::new();
    for sample in counted {
        match by_pid.iter_mut().find(|(pid, _)| *pid == sample.pid) {
            Some((_, pct)) => *pct += sample.util_pct,
            None => by_pid.push((sample.pid, sample.util_pct)),
        }
    }
    by_pid.sort_by(|a, b| b.1.total_cmp(&a.1).then(a.0.cmp(&b.0)));
    by_pid
        .into_iter()
        .take(TOP_PROCESSES)
        .map(|(pid, pct)| NonLenderProcess {
            name: name_of(pid)
                .map(|name| name.chars().take(MAX_PROCESS_NAME).collect())
                .unwrap_or_else(|| format!("pid {pid}")),
            pct,
        })
        .collect()
}

/// Each attendance profile's learned histogram, in figures, as of the
/// latest sample.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct LearnedProfiles {
    pub attended: crate::plugin::pressure::LearnedSummary,
    pub unattended: crate::plugin::pressure::LearnedSummary,
}

/// The most recent records, oldest first, up to a fixed count, the busiest
/// non-lender processes of the latest sample, and what has been learned.
#[derive(Debug)]
pub struct PressureHistory {
    records: VecDeque<PressureRecord>,
    limit: usize,
    top_processes: Vec<NonLenderProcess>,
    learned: Option<LearnedProfiles>,
}

impl PressureHistory {
    pub fn new(limit: usize) -> Self {
        Self {
            records: VecDeque::new(),
            limit: limit.max(1),
            top_processes: Vec::new(),
            learned: None,
        }
    }

    /// Append a record, dropping the oldest once the history is full.
    pub fn push(&mut self, record: PressureRecord) {
        if self.records.len() >= self.limit {
            self.records.pop_front();
        } else if self.records.capacity() == 0 {
            // Allocate once, at the bound, on the first sample, so the
            // buffer neither regrows nor overshoots its documented size.
            self.records.reserve_exact(self.limit);
        }
        self.records.push_back(record);
    }

    /// Record that the reading is unknown now, and why. The latest sample's
    /// busiest processes no longer describe anything and are cleared.
    pub fn push_unknown(&mut self, reason: &str) {
        let bounded: String = reason.chars().take(MAX_REASON_CHARS).collect();
        let shared = match self.records.back() {
            Some(PressureRecord {
                reading: Reading::Unknown { reason },
                ..
            }) if **reason == *bounded => reason.clone(),
            _ => Arc::from(bounded),
        };
        self.push(PressureRecord {
            at_unix_ms: now_unix_ms(),
            reading: Reading::Unknown { reason: shared },
        });
        self.top_processes.clear();
    }

    /// Replace the latest sample's busiest processes.
    pub fn set_top_processes(&mut self, processes: Vec<NonLenderProcess>) {
        self.top_processes = processes;
    }

    pub fn top_processes(&self) -> Vec<NonLenderProcess> {
        self.top_processes.clone()
    }

    /// Replace the learned histograms' summaries. Kept through unknown
    /// readings, which change nothing that has been learned.
    pub fn set_learned(
        &mut self,
        attended: crate::plugin::pressure::LearnedSummary,
        unattended: crate::plugin::pressure::LearnedSummary,
    ) {
        self.learned = Some(LearnedProfiles {
            attended,
            unattended,
        });
    }

    pub fn learned(&self) -> Option<LearnedProfiles> {
        self.learned.clone()
    }

    pub fn limit(&self) -> usize {
        self.limit
    }

    /// A copy of the records, oldest first.
    pub fn records(&self) -> Vec<PressureRecord> {
        self.records.iter().cloned().collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn record(at_unix_ms: u64) -> PressureRecord {
        PressureRecord {
            at_unix_ms,
            reading: Reading::Ready {
                non_lender_pct: 12.5,
                profile: Profile::Attended,
                threshold_pct: 32.5,
                baseline_pct: Some(12.5),
                calibrated: true,
                engaged: false,
            },
        }
    }

    #[test]
    fn six_hours_at_the_poll_cadence_within_the_one_second_floor() {
        assert_eq!(capacity_for(5_000), 4_320);
        assert_eq!(capacity_for(1_000), 21_600);
        assert_eq!(capacity_for(1), 21_600);
        assert_eq!(capacity_for(7_000), 3_086);
        assert_eq!(capacity_for(u64::MAX), 1);
        assert!(std::mem::size_of::<PressureRecord>() <= 56);
    }

    #[test]
    fn the_oldest_record_leaves_first_once_full() {
        let mut history = PressureHistory::new(3);
        assert_eq!(history.records.capacity(), 0);
        for at in 1..=5 {
            history.push(record(at));
        }
        assert_eq!(
            history
                .records()
                .iter()
                .map(|record| record.at_unix_ms)
                .collect::<Vec<_>>(),
            [3, 4, 5]
        );
        assert_eq!(history.limit(), 3);
        assert!(history.records.capacity() < 8);
    }

    #[test]
    fn the_busiest_processes_are_summed_per_process_capped_and_named() {
        use crate::probes::gpu_procs::ProcGpuSample;
        let sample = |pid, util_pct| ProcGpuSample {
            pid,
            luid: "luid".into(),
            util_pct,
        };
        let samples: Vec<ProcGpuSample> = vec![
            sample(10, 5.0),
            sample(11, 30.0),
            sample(10, 30.0),
            sample(12, 1.0),
            sample(13, 2.0),
            sample(14, 3.0),
            sample(15, 4.0),
        ];
        let counted: Vec<&ProcGpuSample> = samples.iter().collect();
        let long = "x".repeat(400);
        let top = top_processes(&counted, |pid| match pid {
            10 => Some("game".into()),
            11 => Some(long.clone()),
            _ => None,
        });
        assert_eq!(top.len(), TOP_PROCESSES);
        assert_eq!(
            top[0],
            NonLenderProcess {
                name: "game".into(),
                pct: 35.0
            }
        );
        assert_eq!(top[1].name.len(), MAX_PROCESS_NAME);
        assert_eq!(top[1].pct, 30.0);
        assert_eq!(
            top[2..].iter().map(|p| p.name.as_str()).collect::<Vec<_>>(),
            ["pid 15", "pid 14", "pid 13"]
        );
        assert!(top_processes(&[], |_| None).is_empty());
    }

    #[test]
    fn records_serialize_with_named_profiles_and_absent_baselines() {
        let calibrating = PressureRecord {
            at_unix_ms: 7,
            reading: Reading::Ready {
                non_lender_pct: 12.5,
                profile: Profile::Unattended,
                threshold_pct: 32.5,
                baseline_pct: None,
                calibrated: false,
                engaged: false,
            },
        };
        assert_eq!(
            serde_json::to_value(&calibrating).unwrap(),
            serde_json::json!({
                "at_unix_ms": 7, "quality": "ready", "non_lender_pct": 12.5,
                "profile": "unattended", "threshold_pct": 32.5, "baseline_pct": null,
                "calibrated": false, "engaged": false
            })
        );
    }

    #[test]
    fn unknown_readings_keep_a_bounded_reason_shared_while_it_repeats() {
        let mut history = PressureHistory::new(10);
        history.set_top_processes(vec![NonLenderProcess {
            name: "game".into(),
            pct: 40.0,
        }]);
        history.push(record(1));
        let long = "x".repeat(MAX_REASON_CHARS + 50);
        history.push_unknown(&long);
        history.push_unknown(&long);
        history.push_unknown("adapter was absent from the GPU counter batch");
        assert!(history.top_processes().is_empty());
        let records = history.records();
        let reason = |index: usize| match &history.records[index].reading {
            Reading::Unknown { reason } => reason.clone(),
            Reading::Ready { .. } => panic!("record {index} is ready"),
        };
        assert_eq!(reason(1).chars().count(), MAX_REASON_CHARS);
        assert!(Arc::ptr_eq(&reason(1), &reason(2)));
        assert!(!Arc::ptr_eq(&reason(2), &reason(3)));
        let last = serde_json::to_value(&records[3]).unwrap();
        assert_eq!(last["quality"], "unknown");
        assert_eq!(
            last["reason"],
            "adapter was absent from the GPU counter batch"
        );
        assert!(last.get("non_lender_pct").is_none());
    }
}
