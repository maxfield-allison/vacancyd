//! Why borrowed work was reclaimed.
//!
//! A reclaim starts from one of two triggers: the owner veto engaging, or
//! GPU pressure refusing resources that admitted work. Each reclaim is
//! logged with its trigger and, for a pressure refusal, each refused
//! resource and why it was refused. The most recent reclaims are also kept
//! in a bounded ring for the diagnostics export, together with the
//! upstreams and ledger entries they reached and what became of them.

use std::collections::VecDeque;
use std::fmt;
use std::time::SystemTime;

use serde::Serialize;

use crate::plugin::pressure::PressureVerdict;

/// Reclaims kept for the diagnostics export. Each is a rising edge, so even
/// a flapping signal takes a while to fill this.
pub const RING_EVENTS: usize = 64;
/// Release outcomes kept per upstream in one reclaim. Each names at most
/// the entries that upstream owned when the reclaim began.
const MAX_RELEASES: usize = 32;

/// What started a reclaim.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
pub enum Trigger {
    #[serde(rename = "owner-veto")]
    OwnerVeto,
    #[serde(rename = "resource-refused")]
    ResourceRefused,
}

impl Trigger {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::OwnerVeto => "owner-veto",
            Self::ResourceRefused => "resource-refused",
        }
    }
}

/// Why a resource that had been admitting work now refuses it.
#[derive(Debug, Clone, PartialEq, Serialize)]
#[serde(tag = "reason", rename_all = "snake_case")]
pub enum RefusalCause {
    /// Pressure from work vacancyd did not admit stayed above the threshold.
    Engaged {
        non_lender_pct: f64,
        threshold_pct: f64,
    },
    /// The attendance profile in force has not completed its first
    /// calibration.
    NotCalibrated {
        calibration_samples: usize,
        calibration_required: usize,
        /// The estimator's own explanation, such as how it is collecting.
        #[serde(skip_serializing_if = "Option::is_none")]
        hint: Option<String>,
    },
    /// The sample was not a percentage of the device (not finite, or
    /// outside 0 to 100), so nothing could be judged from it.
    Invalid { detail: String },
    /// The pressure reading can no longer be trusted. `quality` is what the
    /// reading was when it was invalidated (`ready` or `stale`); `detail`
    /// is why. Running work is reclaimed only once the reading has stayed
    /// unknown for the pressure tracker's gap limit; `unknown_for_ms` says how
    /// long it had been unknown when it was.
    Unknown {
        quality: &'static str,
        detail: String,
        #[serde(skip_serializing_if = "Option::is_none")]
        unknown_for_ms: Option<u64>,
    },
}

/// One refused resource and why.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct RefusedResource {
    pub resource: String,
    #[serde(flatten)]
    pub cause: RefusalCause,
}

impl RefusedResource {
    /// The cause a fresh verdict gives for refusing: an invalid sample,
    /// engaged pressure, or otherwise missing calibration.
    pub fn from_verdict(resource: &str, verdict: &PressureVerdict) -> Self {
        let valid =
            verdict.non_lender_pct.is_finite() && (0.0..=100.0).contains(&verdict.non_lender_pct);
        let cause = if !valid {
            RefusalCause::Invalid {
                detail: verdict
                    .calibration_hint
                    .clone()
                    .unwrap_or_else(|| "invalid pressure sample".into()),
            }
        } else if verdict.engaged {
            RefusalCause::Engaged {
                non_lender_pct: verdict.non_lender_pct,
                threshold_pct: verdict.threshold_pct,
            }
        } else {
            RefusalCause::NotCalibrated {
                calibration_samples: verdict.calibration_samples,
                calibration_required: verdict.calibration_required,
                hint: verdict.calibration_hint.clone(),
            }
        };
        Self {
            resource: resource.into(),
            cause,
        }
    }

    pub fn unknown(resource: &str, quality: &'static str, detail: &str) -> Self {
        Self {
            resource: resource.into(),
            cause: RefusalCause::Unknown {
                quality,
                detail: detail.into(),
                unknown_for_ms: None,
            },
        }
    }

    /// The same refusal, saying how long its reading had been unknown.
    /// Other causes are returned unchanged.
    pub fn unknown_for(mut self, elapsed_ms: u64) -> Self {
        if let RefusalCause::Unknown { unknown_for_ms, .. } = &mut self.cause {
            *unknown_for_ms = Some(elapsed_ms);
        }
        self
    }
}

impl fmt::Display for RefusedResource {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let resource = &self.resource;
        match &self.cause {
            RefusalCause::Engaged {
                non_lender_pct,
                threshold_pct,
            } => write!(
                f,
                "{resource} engaged at {non_lender_pct:.1}% (threshold {threshold_pct:.1}%)"
            ),
            RefusalCause::NotCalibrated {
                calibration_samples,
                calibration_required,
                hint,
            } => {
                write!(
                    f,
                    "{resource} not calibrated ({calibration_samples}/{calibration_required} samples"
                )?;
                match hint {
                    Some(hint) => write!(f, ": {hint})"),
                    None => write!(f, ")"),
                }
            }
            RefusalCause::Invalid { detail } => write!(f, "{resource} invalid reading ({detail})"),
            RefusalCause::Unknown {
                quality,
                detail,
                unknown_for_ms,
            } => {
                write!(
                    f,
                    "{resource} unknown ({quality} reading invalidated: {detail}"
                )?;
                if let Some(ms) = unknown_for_ms {
                    write!(f, "; for {:.1}s", *ms as f64 / 1000.0)?;
                }
                write!(f, ")")
            }
        }
    }
}

/// The cause shared by everything one reclaim does.
#[derive(Debug, Clone, PartialEq)]
pub struct ReclaimCause {
    pub trigger: Trigger,
    /// The owner veto's own cause, for an owner-veto reclaim.
    pub veto_cause: Option<String>,
    /// The refused resources, for a resource-refused reclaim.
    pub resources: Vec<RefusedResource>,
}

impl ReclaimCause {
    /// The refused resources and reasons on one line, for a log field.
    pub fn refused(&self) -> String {
        self.resources
            .iter()
            .map(ToString::to_string)
            .collect::<Vec<_>>()
            .join("; ")
    }
}

/// One reclaim as the diagnostics export shows it.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct ReclaimEvent {
    /// Increases with each reclaim, and appears as `reclaim` in its log
    /// lines.
    pub id: u64,
    pub at_unix_ms: u64,
    pub trigger: Trigger,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub owner_veto_cause: Option<String>,
    pub resources: Vec<RefusedResource>,
    pub upstreams: Vec<UpstreamReclaim>,
}

/// What one reclaim did to one upstream.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct UpstreamReclaim {
    pub upstream: String,
    /// Ledger entries the upstream owned when the reclaim began.
    pub entries: Vec<u64>,
    /// `cancel`, `drain` or `finish`.
    pub on_owner_return: &'static str,
    /// Whether the upstream's models are unloaded afterwards.
    pub unload: bool,
    /// Entries whose cancellation was confirmed, and how they stopped.
    pub released: Vec<Released>,
    /// Set when the reclaim stood down for this upstream because its trigger
    /// cleared while it waited: nothing was cancelled or unloaded. How long
    /// after the reclaim began, in milliseconds.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub abandoned_after_ms: Option<u64>,
    /// Set when the unload was skipped because the upstream was running or
    /// queueing work vacancyd did not admit.
    #[serde(skip_serializing_if = "std::ops::Not::not")]
    pub unload_skipped_for_foreign_work: bool,
}

#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct Released {
    pub entries: Vec<u64>,
    pub how: String,
}

/// The most recent reclaims, oldest first.
#[derive(Debug, Default)]
pub struct ReclaimLog {
    events: VecDeque<ReclaimEvent>,
    next_id: u64,
}

impl ReclaimLog {
    /// Keep a reclaim that is starting now, and return its id.
    pub fn record(&mut self, cause: &ReclaimCause, upstreams: Vec<UpstreamReclaim>) -> u64 {
        self.next_id += 1;
        if self.events.len() >= RING_EVENTS {
            self.events.pop_front();
        }
        self.events.push_back(ReclaimEvent {
            id: self.next_id,
            at_unix_ms: SystemTime::now()
                .duration_since(SystemTime::UNIX_EPOCH)
                .map_or(0, |since| since.as_millis() as u64),
            trigger: cause.trigger,
            owner_veto_cause: cause.veto_cause.clone(),
            resources: cause.resources.clone(),
            upstreams,
        });
        self.next_id
    }

    /// Add confirmed releases to a reclaim still in the ring.
    pub fn note_release(&mut self, id: u64, upstream: &str, entries: &[u64], how: &str) {
        let Some(reclaim) = self.upstream_mut(id, upstream) else {
            return;
        };
        if reclaim.released.len() < MAX_RELEASES {
            reclaim.released.push(Released {
                entries: entries.to_vec(),
                how: how.into(),
            });
        }
    }

    /// Note that a reclaim still in the ring stood down for `upstream`.
    pub fn note_abandoned(&mut self, id: u64, upstream: &str, after_ms: u64) {
        if let Some(reclaim) = self.upstream_mut(id, upstream) {
            reclaim.abandoned_after_ms = Some(after_ms);
        }
    }

    /// Note that a reclaim still in the ring left `upstream`'s models loaded
    /// because it held work vacancyd did not admit.
    pub fn note_unload_skipped(&mut self, id: u64, upstream: &str) {
        if let Some(reclaim) = self.upstream_mut(id, upstream) {
            reclaim.unload_skipped_for_foreign_work = true;
        }
    }

    fn upstream_mut(&mut self, id: u64, upstream: &str) -> Option<&mut UpstreamReclaim> {
        self.events
            .iter_mut()
            .find(|event| event.id == id)
            .and_then(|event| event.upstreams.iter_mut().find(|u| u.upstream == upstream))
    }

    /// The most recent reclaim that reached `upstream`, for work whose
    /// cancellation completes after the reclaim itself.
    pub fn latest_for(&self, upstream: &str) -> Option<(u64, ReclaimCause)> {
        self.events
            .iter()
            .rev()
            .find(|event| event.upstreams.iter().any(|u| u.upstream == upstream))
            .map(|event| {
                (
                    event.id,
                    ReclaimCause {
                        trigger: event.trigger,
                        veto_cause: event.owner_veto_cause.clone(),
                        resources: event.resources.clone(),
                    },
                )
            })
    }

    pub fn events(&self) -> Vec<ReclaimEvent> {
        self.events.iter().cloned().collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn upstream(id: &str) -> UpstreamReclaim {
        UpstreamReclaim {
            upstream: id.into(),
            entries: vec![1],
            on_owner_return: "cancel",
            unload: false,
            released: Vec::new(),
            abandoned_after_ms: None,
            unload_skipped_for_foreign_work: false,
        }
    }

    #[test]
    fn causes_read_plainly_and_serialize_with_their_reason() {
        let engaged = RefusedResource {
            resource: "gpu0".into(),
            cause: RefusalCause::Engaged {
                non_lender_pct: 41.25,
                threshold_pct: 30.0,
            },
        };
        let calibrating = RefusedResource {
            resource: "gpu1".into(),
            cause: RefusalCause::NotCalibrated {
                calibration_samples: 12,
                calibration_required: 60,
                hint: None,
            },
        };
        let unknown = RefusedResource::unknown("gpu2", "stale", "presence is unresolved");
        assert_eq!(
            engaged.to_string(),
            "gpu0 engaged at 41.2% (threshold 30.0%)"
        );
        assert_eq!(
            calibrating.to_string(),
            "gpu1 not calibrated (12/60 samples)"
        );
        assert_eq!(
            unknown.to_string(),
            "gpu2 unknown (stale reading invalidated: presence is unresolved)"
        );
        assert_eq!(
            serde_json::to_value(&unknown).unwrap(),
            serde_json::json!({
                "resource": "gpu2", "reason": "unknown",
                "quality": "stale", "detail": "presence is unresolved"
            })
        );
        let cause = ReclaimCause {
            trigger: Trigger::ResourceRefused,
            veto_cause: None,
            resources: vec![engaged, calibrating],
        };
        assert_eq!(
            cause.refused(),
            "gpu0 engaged at 41.2% (threshold 30.0%); gpu1 not calibrated (12/60 samples)"
        );
        assert_eq!(
            serde_json::to_value(Trigger::OwnerVeto).unwrap(),
            "owner-veto"
        );
    }

    fn verdict(non_lender_pct: f64, engaged: bool, calibrated: bool) -> PressureVerdict {
        PressureVerdict {
            mode: Default::default(),
            calibrated,
            calibration_samples: 60,
            calibration_required: 60,
            calibration_hint: (!calibrated).then(|| {
                if non_lender_pct.is_finite() && (0.0..=100.0).contains(&non_lender_pct) {
                    "collecting samples for this attendance profile".into()
                } else {
                    "invalid pressure sample; waiting for a valid reading".into()
                }
            }),
            non_lender_pct,
            engaged,
            threshold_pct: 25.0,
            baseline_pct: None,
            target_pct: None,
            learned_samples: 0,
            learned_s: 0,
            above_for_s: 0,
        }
    }

    #[test]
    fn an_invalid_sample_is_not_reported_as_missing_calibration() {
        for pct in [f64::NAN, -1.0, 140.0] {
            let refused = RefusedResource::from_verdict("gpu0", &verdict(pct, false, false));
            assert_eq!(
                refused.to_string(),
                "gpu0 invalid reading (invalid pressure sample; waiting for a valid reading)",
                "{pct}"
            );
            assert!(matches!(refused.cause, RefusalCause::Invalid { .. }));
        }
        // A sample that kept an earlier engagement is still invalid first.
        let held = RefusedResource::from_verdict("gpu0", &verdict(f64::NAN, true, false));
        assert!(matches!(held.cause, RefusalCause::Invalid { .. }));

        let capped = RefusedResource::from_verdict("gpu0", &verdict(55.0, false, false));
        assert_eq!(
            capped.to_string(),
            "gpu0 not calibrated (60/60 samples: collecting samples for this attendance profile)"
        );
        let engaged = RefusedResource::from_verdict("gpu0", &verdict(90.0, true, true));
        assert_eq!(
            engaged.to_string(),
            "gpu0 engaged at 90.0% (threshold 25.0%)"
        );
    }

    #[test]
    fn an_unknown_reading_says_how_long_it_lasted_and_outcomes_are_additive() {
        let refused =
            RefusedResource::unknown("gpu0", "ready", "sample failed").unknown_for(15_250);
        assert_eq!(
            refused.to_string(),
            "gpu0 unknown (ready reading invalidated: sample failed; for 15.2s)"
        );
        assert_eq!(
            serde_json::to_value(&refused).unwrap(),
            serde_json::json!({
                "resource": "gpu0", "reason": "unknown", "quality": "ready",
                "detail": "sample failed", "unknown_for_ms": 15250
            })
        );
        // Only an unknown reading has a duration to carry.
        let engaged = RefusedResource::from_verdict("gpu0", &verdict(90.0, true, true));
        assert_eq!(engaged.clone().unknown_for(1), engaged);

        let mut log = ReclaimLog::default();
        let cause = ReclaimCause {
            trigger: Trigger::ResourceRefused,
            veto_cause: None,
            resources: vec![refused],
        };
        let id = log.record(&cause, vec![upstream("comfyui")]);
        let plain = serde_json::to_value(&log.events()[0].upstreams[0]).unwrap();
        assert!(plain.get("abandoned_after_ms").is_none(), "{plain}");
        assert!(
            plain.get("unload_skipped_for_foreign_work").is_none(),
            "{plain}"
        );
        log.note_abandoned(id, "comfyui", 5_100);
        log.note_unload_skipped(id, "comfyui");
        log.note_abandoned(id, "missing", 1);
        let noted = serde_json::to_value(&log.events()[0].upstreams[0]).unwrap();
        assert_eq!(noted["abandoned_after_ms"], 5_100);
        assert_eq!(noted["unload_skipped_for_foreign_work"], true);
    }

    #[test]
    fn the_ring_keeps_the_latest_reclaims_and_their_releases() {
        let mut log = ReclaimLog::default();
        let veto = ReclaimCause {
            trigger: Trigger::OwnerVeto,
            veto_cause: Some("owner active".into()),
            resources: Vec::new(),
        };
        let first = log.record(&veto, vec![upstream("ollama")]);
        log.note_release(first, "ollama", &[1], "interrupted");
        log.note_release(first, "missing", &[1], "interrupted");
        assert_eq!(
            log.events()[0].upstreams[0].released,
            [Released {
                entries: vec![1],
                how: "interrupted".into()
            }]
        );
        assert_eq!(log.latest_for("ollama"), Some((first, veto.clone())));
        assert_eq!(log.latest_for("comfyui"), None);

        for _ in 0..RING_EVENTS {
            log.record(&veto, vec![upstream("comfyui")]);
        }
        let events = log.events();
        assert_eq!(events.len(), RING_EVENTS);
        assert_eq!(events[0].id, first + 1);
        // The first reclaim has left the ring; a late release is dropped.
        log.note_release(first, "ollama", &[1], "already ended");
        assert_eq!(log.latest_for("ollama"), None);
    }
}
