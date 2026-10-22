//! Borrowed GPU work the pressure signal cannot see.
//!
//! GPU pressure subtracts the processes serving each upstream's target and
//! their descendants (see [`crate::probes::listeners`]). That works only when
//! the process tree listening on the target is the one doing the GPU work.
//! When the listener only relays connections, for example into a virtual
//! machine or a container, the work runs in a process outside that tree. It
//! then counts as owner load, and can engage pressure and reclaim the very
//! borrower that caused it.
//!
//! vacancyd does not guess which other process is the borrower's: whatever
//! it subtracted on a guess would also hide the owner's own work run the
//! same way. It reports the mismatch instead, and changes nothing that
//! pressure counts. An upstream is reported once its own admitted work has
//! run through [`UNSEEN_AFTER_SAMPLES`] attribution samples in which no
//! process of its tree used any GPU, and cleared by the first sample in which
//! one does. Samples taken while none of its work runs neither count nor
//! clear, so short jobs separated by idle time still add up.

use std::collections::{BTreeMap, BTreeSet};

/// Samples of running work with a silent process tree before an upstream is
/// reported: about fifteen seconds at the default five-second poll. One or
/// two such samples are ordinary while a server loads a model or waits on
/// its client.
pub const UNSEEN_AFTER_SAMPLES: u32 = 3;

/// One attribution sample, as far as one upstream is concerned.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Sample {
    /// The upstream's own admitted work was running, and the sample covered
    /// every GPU that work borrows.
    pub working: bool,
    /// A process serving the upstream's target, or a descendant, used a GPU.
    pub tree_used_gpu: bool,
}

/// A change in an upstream's warning about unseen GPU work.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Change {
    /// Its work has now run unseen for [`UNSEEN_AFTER_SAMPLES`] samples.
    Unseen,
    /// A reported upstream's process tree was seen using a GPU.
    Seen,
}

/// Per upstream, how many samples its running work has gone unseen and
/// whether it is reported.
#[derive(Debug, Default)]
pub struct UnseenWork {
    silent: BTreeMap<String, u32>,
    reported: BTreeSet<String>,
}

impl UnseenWork {
    /// Record one sample for `upstream`, returning the change it caused.
    pub fn observe(&mut self, upstream: &str, sample: Sample) -> Option<Change> {
        if sample.tree_used_gpu {
            self.silent.remove(upstream);
            return self.reported.remove(upstream).then_some(Change::Seen);
        }
        if !sample.working {
            return None;
        }
        let silent = self.silent.entry(upstream.to_owned()).or_default();
        *silent = silent.saturating_add(1);
        (*silent >= UNSEEN_AFTER_SAMPLES && self.reported.insert(upstream.to_owned()))
            .then_some(Change::Unseen)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const SILENT_WORK: Sample = Sample {
        working: true,
        tree_used_gpu: false,
    };
    const IDLE: Sample = Sample {
        working: false,
        tree_used_gpu: false,
    };
    const SEEN: Sample = Sample {
        working: true,
        tree_used_gpu: true,
    };

    #[test]
    fn running_work_whose_tree_never_uses_a_gpu_is_reported_once_on_the_third_sample() {
        let mut unseen = UnseenWork::default();

        assert_eq!(unseen.observe("voice", SILENT_WORK), None);
        assert_eq!(unseen.observe("voice", SILENT_WORK), None);
        assert_eq!(unseen.observe("voice", SILENT_WORK), Some(Change::Unseen));
        assert_eq!(unseen.observe("voice", SILENT_WORK), None);
    }

    #[test]
    fn samples_without_running_work_neither_count_nor_clear() {
        let mut unseen = UnseenWork::default();

        assert_eq!(unseen.observe("voice", SILENT_WORK), None);
        assert_eq!(unseen.observe("voice", IDLE), None);
        assert_eq!(unseen.observe("voice", SILENT_WORK), None);
        assert_eq!(unseen.observe("voice", IDLE), None);
        assert_eq!(unseen.observe("voice", SILENT_WORK), Some(Change::Unseen));
        assert_eq!(unseen.observe("voice", IDLE), None, "stays reported");
    }

    #[test]
    fn a_tree_seen_using_a_gpu_restarts_the_count_and_clears_a_report() {
        let mut unseen = UnseenWork::default();
        unseen.observe("comfyui", SILENT_WORK);
        unseen.observe("comfyui", SILENT_WORK);

        // A model load is ordinary silence: the work then shows up.
        assert_eq!(unseen.observe("comfyui", SEEN), None);
        assert_eq!(unseen.observe("comfyui", SILENT_WORK), None);
        assert_eq!(unseen.observe("comfyui", SILENT_WORK), None);
        assert_eq!(unseen.observe("comfyui", SILENT_WORK), Some(Change::Unseen));

        // The owner using the same server also proves the tree is visible.
        let owner_use = Sample {
            working: false,
            tree_used_gpu: true,
        };
        assert_eq!(unseen.observe("comfyui", owner_use), Some(Change::Seen));
        assert_eq!(unseen.observe("comfyui", owner_use), None);
    }

    #[test]
    fn upstreams_are_counted_separately() {
        let mut unseen = UnseenWork::default();
        unseen.observe("voice", SILENT_WORK);
        unseen.observe("voice", SILENT_WORK);

        assert_eq!(unseen.observe("ollama", SILENT_WORK), None);
        assert_eq!(unseen.observe("voice", SILENT_WORK), Some(Change::Unseen));
    }
}
