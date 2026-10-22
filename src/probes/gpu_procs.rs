//! Per-process GPU attribution — "how much of this card is *someone else's* work?"
//!
//! The owner veto's original signal was a process name list, which does not
//! scale: it needs a new entry for every game, misses launcher-wrapped
//! executables, and says nothing at all about Blender, DaVinci Resolve, CAD or
//! a local fine-tune — all of which are exactly the work the veto exists to
//! protect. The question was never "is a game running", it is "does someone
//! other than the borrower want this hardware".
//!
//! Answering that needs utilisation attributed to processes, split per card,
//! with the lent-to services subtracted. On Windows the only source that covers
//! **both** graphics and compute is the `GPU Engine` performance counter set,
//! whose instances are named
//! `pid_<pid>_luid_<hi>_<lo>_phys_<n>_eng_<n>_engtype_<kind>`.
//!
//! Read in process through the Performance Data Helper (PDH) API, by the
//! counter's English path through `PdhAddEnglishCounterW`. Counter object and
//! counter names can be translated on a non-English Windows: German Windows
//! 11 displays `\Processor(*)` as `\Prozessor(*)`, and `typeperf` given the
//! English name fails there. The `GPU Engine` set happened to be left
//! untranslated on the German build tested, but that is the provider's
//! choice, not a promise, so the English path is resolved explicitly and
//! works on every display language. The
//! instance names are generated from process and adapter identifiers and are
//! never translated. A read-only query costs nothing on the card. It is not
//! free on the CPU, roughly a thousand counter instances on a two-card
//! desktop, so it is polled on its own slower cadence, exactly as the media
//! probe is.
//!
//! **A boolean is the wrong shape and this module deliberately does not offer
//! one.** A live Windows desktop *always* has non-lender GPU work: `dwm`
//! composites continuously on every adapter, and a hardware-accelerated browser
//! joins it. Measured on the reference machine while idle, `dwm` alone was 9.6%
//! on one card and 3.7% on the other. "Any non-lender process is using the GPU"
//! is therefore permanently true, and a veto built on it would never release.
//! Callers get a percentage and choose a threshold.

#[cfg(any(windows, test))]
use std::collections::HashMap;
use std::collections::HashSet;
#[cfg(windows)]
use std::time::Duration;

use crate::probes::listeners::ProcessIdentity;

/// One process's utilisation of one adapter, summed across that adapter's
/// engines.
#[derive(Debug, Clone, PartialEq)]
pub struct ProcGpuSample {
    pub pid: u32,
    /// Adapter LUID, normalised to lowercase (`0x00000000_0x00013fa9`).
    pub luid: String,
    /// Percent of the adapter, 0..=100.
    pub util_pct: f64,
}

/// One coherent read of the GPU Engine counter set.
///
/// `processes` contains only positive utilisation, while `observed_luids`
/// also contains adapters whose readable counters were all zero. Keeping the
/// latter is what lets a caller distinguish a measured idle adapter from an
/// adapter omitted by a partial counter batch.
#[derive(Debug, Clone, PartialEq)]
pub struct ProcGpuBatch {
    pub processes: Vec<ProcGpuSample>,
    pub observed_luids: HashSet<String>,
}

/// Build one batch from `(instance name, value)` readings of the
/// `GPU Engine(*)\Utilization Percentage` counter, where `None` is an
/// instance PDH could not read.
///
/// Anything that does not parse is skipped rather than failing the sample: a
/// counter instance can disappear between the two collections a rate counter
/// needs, because the process exited, and one dead instance must not cost the
/// whole reading.
///
/// A readable zero records the adapter in [`ProcGpuBatch::observed_luids`] but
/// creates no process row. An unreadable value records neither. Engine
/// utilisations are **summed** per `(pid, adapter)` and capped at 100.
/// Summing rather than taking the maximum is deliberate: a process running 3D
/// and compute concurrently is using more of the card than either figure alone,
/// and for a signal that protects the owner, over-estimating demand is the safe
/// direction to err in.
///
/// Returns `None` when no adapter was observed at all, which a caller must
/// treat as an unreadable sample rather than an idle machine.
#[cfg(any(windows, test))]
pub fn batch_from_readings<'a>(
    readings: impl IntoIterator<Item = (&'a str, Option<f64>)>,
) -> Option<ProcGpuBatch> {
    let mut acc: HashMap<(u32, String), f64> = HashMap::new();
    let mut observed_luids = HashSet::new();
    for (instance, value) in readings {
        let Some((pid, luid)) = parse_instance(instance) else {
            continue;
        };
        let Some(v) = value else {
            continue;
        };
        if !v.is_finite() || v < 0.0 {
            continue;
        }
        observed_luids.insert(luid.clone());
        if v == 0.0 {
            continue;
        }
        *acc.entry((pid, luid)).or_default() += v;
    }

    let mut out: Vec<ProcGpuSample> = acc
        .into_iter()
        .map(|((pid, luid), util)| ProcGpuSample {
            pid,
            luid,
            util_pct: util.min(100.0),
        })
        .collect();
    out.sort_by(|a, b| {
        b.util_pct
            .partial_cmp(&a.util_pct)
            .unwrap_or(std::cmp::Ordering::Equal)
            .then_with(|| a.pid.cmp(&b.pid))
    });
    (!observed_luids.is_empty()).then_some(ProcGpuBatch {
        processes: out,
        observed_luids,
    })
}

/// Pull `(pid, luid)` out of a `GPU Engine` instance name such as
/// `pid_21428_luid_0x00000000_0x00013FA9_phys_0_eng_0_engtype_3D`.
///
/// The LUID is lowercased so that the counter's uppercase hex and the
/// lowercase form used elsewhere in Windows tooling compare equal — they name
/// the same adapter and have been observed to differ in case on one machine.
#[cfg(any(windows, test))]
fn parse_instance(instance: &str) -> Option<(u32, String)> {
    let rest = instance.strip_prefix("pid_")?;
    let (pid, rest) = rest.split_once("_luid_")?;
    let luid = rest.split_once("_phys_")?.0;
    Some((pid.parse().ok()?, luid.to_ascii_lowercase()))
}

/// The samples on one adapter from processes that are **not** lenders and
/// **not** ignored. [`total_pct`] turns them into the pressure reading.
///
/// `lenders` are listener processes plus their descendants, qualified by
/// process start time so PID reuse cannot inherit an exclusion. Without this
/// subtraction the signal is self-defeating: borrowed inference drives
/// utilisation up, reads as pressure, unloads itself, and then repeats.
///
/// `ignore_names` is the escape hatch for a process whose GPU use should never
/// count — a wallpaper engine, a monitoring overlay, a second inference server
/// the user runs outside vacancyd. Matched case-insensitively as a substring of
/// the process name, mirroring `owner.media_ignore`.
pub fn non_lender_samples<'a>(
    samples: &'a [ProcGpuSample],
    luid: &str,
    lenders: &HashSet<ProcessIdentity>,
    ignore_names: &[String],
    identity_of: &dyn Fn(u32) -> Option<ProcessIdentity>,
    name_of: &dyn Fn(u32) -> Option<String>,
) -> Vec<&'a ProcGpuSample> {
    let luid = luid.to_ascii_lowercase();
    let mut counted = Vec::new();
    for s in samples.iter().filter(|s| s.luid == luid) {
        if identity_of(s.pid).is_some_and(|identity| lenders.contains(&identity)) {
            continue;
        }
        if !ignore_names.is_empty() {
            if let Some(name) = name_of(s.pid) {
                let name = name.to_ascii_lowercase();
                if ignore_names
                    .iter()
                    .any(|ig| !ig.is_empty() && name.contains(&ig.to_ascii_lowercase()))
                {
                    continue;
                }
            }
        }
        counted.push(s);
    }
    counted
}

/// The non-lender percentage of `counted`, capped at one whole device.
pub fn total_pct(counted: &[&ProcGpuSample]) -> f64 {
    counted
        .iter()
        .fold(0.0, |total, sample| total + sample.util_pct)
        .min(100.0)
}

/// [`non_lender_samples`] totalled by [`total_pct`], as the engine reads it.
#[cfg(test)]
pub fn non_lender_pct(
    samples: &[ProcGpuSample],
    luid: &str,
    lenders: &HashSet<ProcessIdentity>,
    ignore_names: &[String],
    identity_of: &dyn Fn(u32) -> Option<ProcessIdentity>,
    name_of: &dyn Fn(u32) -> Option<String>,
) -> f64 {
    total_pct(&non_lender_samples(
        samples,
        luid,
        lenders,
        ignore_names,
        identity_of,
        name_of,
    ))
}

/// Sample the `GPU Engine` counters.
///
/// Returns an error — never `Ok(empty)` — when the reading could not be
/// taken. The distinction matters: an empty batch would mean "nothing is
/// using the GPU", which would read as vacancy, and a failed counter query
/// must never be allowed to say that. Callers treat the error as "unknown"
/// and, consistently with how unknown idle and unknown media are handled
/// elsewhere in this daemon, unknown is never resolved in favour of lending.
#[cfg(windows)]
pub fn sample() -> Result<ProcGpuBatch, String> {
    let readings = pdh::sample_instances(GPU_ENGINE_COUNTER, Duration::from_secs(1))?;
    batch_from_readings(readings.iter().map(|(name, value)| (name.as_str(), *value)))
        .ok_or_else(|| format!("{GPU_ENGINE_COUNTER} has no readable GPU engine instance"))
}

/// The counter's English path. PDH resolves it on any display language.
#[cfg(windows)]
const GPU_ENGINE_COUNTER: &str = "\\GPU Engine(*)\\Utilization Percentage";

/// No attributed GPU signal outside Windows yet.
///
/// Linux would read this from DRM client stats (`/proc/<pid>/fdinfo`, the same
/// source `nvtop` uses), which is a different enough shape to be its own
/// implementation rather than a port of this one.
#[cfg(not(windows))]
pub fn sample() -> Result<ProcGpuBatch, String> {
    Err("GPU attribution is not implemented on this platform".into())
}

#[cfg(windows)]
mod pdh {
    use std::sync::atomic::{AtomicBool, Ordering};
    use std::time::Duration;

    use windows_sys::Win32::System::Performance::{
        PdhAddEnglishCounterW, PdhCloseQuery, PdhCollectQueryData, PdhGetFormattedCounterArrayW,
        PdhOpenQueryW, PDH_CSTATUS_NEW_DATA, PDH_CSTATUS_VALID_DATA, PDH_FMT_COUNTERVALUE_ITEM_W,
        PDH_FMT_DOUBLE, PDH_MORE_DATA, PDH_NO_DATA,
    };

    /// `PDH_FMT_NOCAP100` (pdh.h): report values above 100 as they are, for
    /// the caller's own cap to act on. Not exported by `windows-sys`.
    const PDH_FMT_NOCAP100: u32 = 0x0000_8000;
    /// A wedged performance provider can block PDH indefinitely. The
    /// collection therefore runs on its own thread with this deadline, as
    /// every probe subprocess has one.
    const DEADLINE: Duration = Duration::from_secs(7);

    /// Set while a collection thread is running. A new sample is refused
    /// rather than stacking a second thread behind a wedged one.
    static IN_FLIGHT: AtomicBool = AtomicBool::new(false);

    struct Query(isize);

    impl Drop for Query {
        fn drop(&mut self) {
            // SAFETY: the handle came from PdhOpenQueryW and is closed once.
            unsafe { PdhCloseQuery(self.0) };
        }
    }

    fn check(status: u32, what: &str) -> Result<(), String> {
        if status == 0 {
            Ok(())
        } else {
            Err(format!("{what}: PDH status 0x{status:08X}"))
        }
    }

    /// Every instance of `english_path`, a wildcard counter, with its value
    /// over `interval`, or `None` for an instance PDH could not read.
    pub fn sample_instances(
        english_path: &str,
        interval: Duration,
    ) -> Result<Vec<(String, Option<f64>)>, String> {
        if IN_FLIGHT.swap(true, Ordering::AcqRel) {
            return Err("the previous performance-counter sample has not returned".into());
        }
        let path = english_path.to_owned();
        let (sender, receiver) = std::sync::mpsc::channel();
        let spawned = std::thread::Builder::new()
            .name("vacancyd-pdh".into())
            .spawn(move || {
                let result = collect(&path, interval);
                IN_FLIGHT.store(false, Ordering::Release);
                let _ = sender.send(result);
            });
        if let Err(error) = spawned {
            IN_FLIGHT.store(false, Ordering::Release);
            return Err(format!("starting the performance-counter thread: {error}"));
        }
        receiver.recv_timeout(interval + DEADLINE).map_err(|_| {
            format!(
                "performance-counter sample exceeded {} ms",
                (interval + DEADLINE).as_millis()
            )
        })?
    }

    fn collect(
        english_path: &str,
        interval: Duration,
    ) -> Result<Vec<(String, Option<f64>)>, String> {
        let mut handle = 0isize;
        // SAFETY: a null data source selects live data; `handle` is a valid
        // out-parameter.
        check(
            unsafe { PdhOpenQueryW(std::ptr::null(), 0, &mut handle) },
            "opening a PDH query",
        )?;
        let query = Query(handle);
        let wide: Vec<u16> = english_path
            .encode_utf16()
            .chain(std::iter::once(0))
            .collect();
        let mut counter = 0isize;
        // SAFETY: `wide` is NUL-terminated and outlives the call.
        check(
            unsafe { PdhAddEnglishCounterW(query.0, wide.as_ptr(), 0, &mut counter) },
            &format!("adding {english_path}"),
        )?;
        // A utilisation counter is a rate: it needs two collections, and
        // its value is the average between them.
        // SAFETY: `query` is open.
        match unsafe { PdhCollectQueryData(query.0) } {
            PDH_NO_DATA => {}
            status => check(status, "collecting the first sample")?,
        }
        std::thread::sleep(interval);
        // SAFETY: `query` is open.
        match unsafe { PdhCollectQueryData(query.0) } {
            // A wildcard with no instances at the moment: nothing to read.
            PDH_NO_DATA => return Ok(Vec::new()),
            status => check(status, "collecting the second sample")?,
        }

        let format = PDH_FMT_DOUBLE | PDH_FMT_NOCAP100;
        let mut bytes = 0u32;
        let mut count = 0u32;
        // Instances can appear between the sizing call and the read; retry a
        // few times rather than trusting the first size.
        for _ in 0..4 {
            // SAFETY: a null buffer with a zero size asks for the size.
            let status = unsafe {
                PdhGetFormattedCounterArrayW(
                    counter,
                    format,
                    &mut bytes,
                    &mut count,
                    std::ptr::null_mut(),
                )
            };
            if status == 0 {
                // No instances at all.
                return Ok(Vec::new());
            }
            if status != PDH_MORE_DATA {
                return Err(format!("sizing {english_path}: PDH status 0x{status:08X}"));
            }
            // u64 storage gives the item array its 8-byte alignment.
            let mut buffer = vec![0u64; (bytes as usize).div_ceil(8)];
            let items = buffer.as_mut_ptr().cast::<PDH_FMT_COUNTERVALUE_ITEM_W>();
            // SAFETY: `buffer` holds at least `bytes` bytes, suitably aligned.
            let status = unsafe {
                PdhGetFormattedCounterArrayW(counter, format, &mut bytes, &mut count, items)
            };
            if status == PDH_MORE_DATA {
                continue;
            }
            check(status, &format!("reading {english_path}"))?;
            // SAFETY: on success PDH wrote `count` items at the start of the
            // buffer, whose name pointers point into the same buffer.
            let rows = unsafe { std::slice::from_raw_parts(items, count as usize) };
            return Ok(rows
                .iter()
                .map(|row| {
                    // SAFETY: szName is NUL-terminated inside `buffer`.
                    let name = unsafe { wide_to_string(row.szName) };
                    let value = matches!(
                        row.FmtValue.CStatus,
                        PDH_CSTATUS_VALID_DATA | PDH_CSTATUS_NEW_DATA
                    )
                    // SAFETY: PDH_FMT_DOUBLE selects the double member.
                    .then(|| unsafe { row.FmtValue.Anonymous.doubleValue });
                    (name, value)
                })
                .collect());
        }
        Err(format!("reading {english_path}: instance set kept growing"))
    }

    /// # Safety
    /// `ptr` must be null or point to a NUL-terminated UTF-16 string.
    unsafe fn wide_to_string(ptr: *const u16) -> String {
        if ptr.is_null() {
            return String::new();
        }
        let mut len = 0;
        while *ptr.add(len) != 0 {
            len += 1;
        }
        String::from_utf16_lossy(std::slice::from_raw_parts(ptr, len))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // Trimmed from a real run on the reference machine: two adapters, dwm on
    // both, a browser decoding video, and an instance PDH could not read.
    const INSTANCES: [&str; 5] = [
        "pid_21428_luid_0x00000000_0x00013FA9_phys_0_eng_0_engtype_3D",
        "pid_21428_luid_0x00000000_0x00012947_phys_0_eng_0_engtype_3D",
        "pid_41192_luid_0x00000000_0x00013FA9_phys_0_eng_0_engtype_3D",
        "pid_41192_luid_0x00000000_0x00013FA9_phys_0_eng_3_engtype_VideoDecode",
        "pid_9000_luid_0x00000000_0x00013FA9_phys_0_eng_1_engtype_Compute",
    ];
    const VALUES: [Option<f64>; 5] = [Some(3.7), Some(9.6), Some(1.2), Some(3.6), None];

    fn batch(values: [Option<f64>; 5]) -> Option<ProcGpuBatch> {
        batch_from_readings(INSTANCES.into_iter().zip(values))
    }

    fn no_names(_: u32) -> Option<String> {
        None
    }

    fn identity_of(pid: u32) -> Option<ProcessIdentity> {
        Some(ProcessIdentity {
            pid,
            started_at_s: 100,
        })
    }

    #[test]
    fn sums_engines_per_process_and_adapter() {
        let batch = batch(VALUES).unwrap();
        let s = &batch.processes;
        let find = |pid, luid: &str| {
            s.iter()
                .find(|r| r.pid == pid && r.luid == luid)
                .map(|r| r.util_pct)
        };
        // Browser: 3D + VideoDecode on the same card are added together.
        assert_eq!(find(41192, "0x00000000_0x00013fa9"), Some(4.8));
        // The compositor is on both adapters and stays separate per adapter.
        assert_eq!(find(21428, "0x00000000_0x00013fa9"), Some(3.7));
        assert_eq!(find(21428, "0x00000000_0x00012947"), Some(9.6));
        // The unreadable " " instance is skipped, not counted as zero.
        assert_eq!(find(9000, "0x00000000_0x00013fa9"), None);
    }

    #[test]
    fn luid_case_is_normalised() {
        let batch = batch(VALUES).unwrap();
        assert!(batch
            .processes
            .iter()
            .all(|r| r.luid == r.luid.to_ascii_lowercase()));
        assert!(batch
            .observed_luids
            .iter()
            .all(|luid| luid == &luid.to_ascii_lowercase()));
    }

    #[test]
    fn no_readable_adapter_yields_nothing() {
        assert!(batch_from_readings(std::iter::empty()).is_none());
        assert!(batch([None; 5]).is_none(), "every instance unreadable");
        // Instances that are not GPU Engine names are not adapters.
        assert!(
            batch_from_readings([("_Total", Some(4.0)), ("pid_x_luid_y", Some(1.0))]).is_none()
        );
    }

    #[test]
    fn a_readable_zero_is_distinct_from_an_absent_adapter() {
        let batch = batch([Some(0.0), Some(0.0), Some(0.0), Some(0.0), None]).unwrap();

        assert!(batch.processes.is_empty());
        assert!(batch.observed_luids.contains("0x00000000_0x00013fa9"));
        assert!(batch.observed_luids.contains("0x00000000_0x00012947"));
    }

    #[test]
    fn an_adapter_with_only_unreadable_values_is_absent_from_the_batch() {
        let batch = batch([Some(0.0), None, Some(0.0), Some(0.0), None]).unwrap();

        assert!(batch.observed_luids.contains("0x00000000_0x00013fa9"));
        assert!(!batch.observed_luids.contains("0x00000000_0x00012947"));
    }

    #[test]
    fn nonsensical_values_are_skipped_and_sums_are_capped() {
        let batch = batch([
            Some(f64::NAN),
            Some(-1.0),
            Some(80.0),
            Some(70.0),
            Some(f64::INFINITY),
        ])
        .unwrap();
        assert_eq!(batch.processes.len(), 1);
        assert_eq!(batch.processes[0].pid, 41192);
        assert_eq!(batch.processes[0].util_pct, 100.0);
        assert!(!batch.observed_luids.contains("0x00000000_0x00012947"));
    }

    #[test]
    fn lender_processes_do_not_count_as_resource_pressure() {
        let batch = batch(VALUES).unwrap();
        let s = &batch.processes;
        let card = "0x00000000_0x00013FA9"; // deliberately uppercase
        let none = HashSet::new();
        assert_eq!(
            non_lender_pct(s, card, &none, &[], &identity_of, &no_names),
            8.5
        );

        // The browser is the lender for the sake of this test: subtracting it
        // must leave only the compositor.
        let lenders = HashSet::from([ProcessIdentity {
            pid: 41192,
            started_at_s: 100,
        }]);
        assert_eq!(
            non_lender_pct(s, card, &lenders, &[], &identity_of, &no_names),
            3.7
        );
    }

    #[test]
    fn a_reused_pid_does_not_inherit_lender_identity() {
        let batch = batch(VALUES).unwrap();
        let lenders = HashSet::from([ProcessIdentity {
            pid: 41192,
            started_at_s: 99,
        }]);

        assert_eq!(
            non_lender_pct(
                &batch.processes,
                "0x00000000_0x00013fa9",
                &lenders,
                &[],
                &identity_of,
                &no_names,
            ),
            8.5,
            "the current process has the same PID but a different start time"
        );
    }

    #[test]
    fn ignored_process_names_are_dropped() {
        let batch = batch(VALUES).unwrap();
        let s = &batch.processes;
        let names = |pid| match pid {
            21428 => Some("dwm".to_string()),
            41192 => Some("vivaldi".to_string()),
            _ => None,
        };
        let ignore = vec!["VIVALDI".to_string()]; // case-insensitive
        assert_eq!(
            non_lender_pct(
                s,
                "0x00000000_0x00013fa9",
                &HashSet::new(),
                &ignore,
                &identity_of,
                &names,
            ),
            3.7
        );
    }

    #[test]
    fn other_adapters_are_not_mixed_in() {
        let batch = batch(VALUES).unwrap();
        let s = &batch.processes;
        assert_eq!(
            non_lender_pct(
                s,
                "0x00000000_0x00012947",
                &HashSet::new(),
                &[],
                &identity_of,
                &no_names,
            ),
            9.6
        );
    }

    /// Exercises the real counter set, which the fixtures above cannot: a live
    /// desktop reports around a thousand instances, and the reader has to cope
    /// with the whole set rather than a trimmed five. Run it on a
    /// non-English Windows as well: the English counter path must resolve
    /// there too.
    ///
    /// Ignored by default — it needs Windows, a GPU, and a logged-in session,
    /// none of which hold in CI. Run with `cargo test -- --ignored`.
    #[test]
    #[ignore = "requires a real Windows session with a GPU"]
    fn live_sample_sees_the_compositor() {
        let _turn = PDH_TURN.lock().unwrap_or_else(|e| e.into_inner());
        let batch = sample().expect("counter set should be readable in a live session");
        assert!(!batch.processes.is_empty());
        assert!(
            batch
                .processes
                .iter()
                .all(|r| r.util_pct > 0.0 && r.util_pct <= 100.0),
            "utilisation must be a sane percentage: {batch:?}"
        );
        assert!(
            batch.processes.iter().all(|r| r.luid.starts_with("0x")),
            "every row should carry a parsed adapter LUID: {batch:?}"
        );
    }

    /// One PDH sample runs at a time by design (a second is refused while
    /// the first is in flight), so the live tests take turns.
    static PDH_TURN: std::sync::Mutex<()> = std::sync::Mutex::new(());

    /// The counter set is registered on every WDDM Windows, including a VM
    /// with only the basic display adapter, so its English path must resolve
    /// on every Windows test host, whatever the display language. A host
    /// with no process using the GPU may have no instances; that is an empty
    /// read here and an error from `sample`, never an idle batch.
    #[cfg(windows)]
    #[test]
    fn the_english_counter_path_resolves() {
        let _turn = PDH_TURN.lock().unwrap_or_else(|e| e.into_inner());
        let readings =
            pdh::sample_instances(GPU_ENGINE_COUNTER, std::time::Duration::from_millis(200))
                .expect("GPU Engine counters should resolve by English path");
        eprintln!("GPU Engine instances read: {}", readings.len());
        match batch_from_readings(readings.iter().map(|(n, v)| (n.as_str(), *v))) {
            Some(batch) => assert!(!batch.observed_luids.is_empty()),
            None => assert!(sample().is_err()),
        }
    }

    #[cfg(windows)]
    #[test]
    fn an_unknown_counter_path_is_an_error_not_an_empty_reading() {
        let _turn = PDH_TURN.lock().unwrap_or_else(|e| e.into_inner());
        let error = pdh::sample_instances(
            "\\No Such Counter Set(*)\\Nothing",
            std::time::Duration::from_millis(10),
        )
        .unwrap_err();
        assert!(error.contains("PDH status"), "{error}");
    }
}
