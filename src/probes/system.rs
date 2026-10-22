//! System memory and CPU probes, via `sysinfo` (cross-platform).
//!
//! Unlike GPUs — whose gating is per-model and lives in the gate — these two
//! carry their own lendability rule, expressed as `reserve` in config:
//! memory reserves bytes for the owner, CPU caps the load above which the
//! machine is considered busy.

use std::sync::{Mutex, OnceLock};
use std::time::Instant;

use anyhow::Result;
use sysinfo::{CpuRefreshKind, MemoryRefreshKind, RefreshKind, System};

use crate::config::ResourceConfig;
use crate::plugin::probe::{Capacity, Probe};

/// One shared `System`: refreshing it is not free, and both probes plus the
/// presence sampler want it.
fn shared() -> &'static Mutex<System> {
    static SYS: OnceLock<Mutex<System>> = OnceLock::new();
    SYS.get_or_init(|| {
        Mutex::new(System::new_with_specifics(
            RefreshKind::nothing()
                .with_memory(MemoryRefreshKind::everything())
                .with_cpu(CpuRefreshKind::everything()),
        ))
    })
}

/// When the shared `System` last refreshed CPU usage. sysinfo reports usage
/// as the difference between two refreshes, and a pair closer together than
/// `MINIMUM_CPU_UPDATE_INTERVAL` reads as roughly 0 %, which the CPU reserve
/// would accept as lendable.
fn cpu_refreshed_at() -> &'static Mutex<Option<Instant>> {
    static AT: OnceLock<Mutex<Option<Instant>>> = OnceLock::new();
    AT.get_or_init(|| Mutex::new(None))
}

pub struct SystemMemory;
pub struct SystemCpu;

pub fn build_memory(_cfg: &ResourceConfig) -> Result<Box<dyn Probe>> {
    Ok(Box::new(SystemMemory))
}

pub fn build_cpu(_cfg: &ResourceConfig) -> Result<Box<dyn Probe>> {
    Ok(Box::new(SystemCpu))
}

impl Probe for SystemMemory {
    fn kind(&self) -> &'static str {
        "system-memory"
    }

    fn sample(&mut self) -> Result<Capacity> {
        let mut sys = shared().lock().unwrap();
        sys.refresh_memory();
        memory_capacity(sys.total_memory(), sys.available_memory())
    }

    /// Lendable while at least `reserve` bytes remain free for the owner.
    fn lendable(&self, c: &Capacity, reserve: Option<u64>) -> bool {
        c.free_bytes >= reserve.unwrap_or(0)
    }
}

impl Probe for SystemCpu {
    fn kind(&self) -> &'static str {
        "system-cpu"
    }

    fn sample(&mut self) -> Result<Capacity> {
        // Only this probe refreshes CPU usage, and the capacity loop is its
        // only caller, so holding this lock orders every refresh pair.
        let mut refreshed_at = cpu_refreshed_at().lock().unwrap();
        let last = *refreshed_at.get_or_insert_with(|| {
            shared().lock().unwrap().refresh_cpu_usage();
            Instant::now()
        });
        if let Some(wait) = sysinfo::MINIMUM_CPU_UPDATE_INTERVAL.checked_sub(last.elapsed()) {
            // Sleep without the shared `System`, which memory and presence
            // sampling also use.
            std::thread::sleep(wait);
        }
        let mut sys = shared().lock().unwrap();
        sys.refresh_cpu_usage();
        *refreshed_at = Some(Instant::now());
        cpu_capacity(sys.global_cpu_usage(), sys.cpus().len())
    }

    /// Lendable while load is at or below `reserve` percent.
    fn lendable(&self, c: &Capacity, reserve: Option<u64>) -> bool {
        c.utilization_pct
            .is_some_and(|percent| u64::from(percent) <= reserve.unwrap_or(100))
    }
}

/// A memory sample, or an error when the reading cannot be real. sysinfo
/// reports 0 for memory it could not read, and a zero total would otherwise
/// pass as a machine with no free memory rather than an unreadable one.
fn memory_capacity(total: u64, free: u64) -> Result<Capacity> {
    anyhow::ensure!(total > 0, "system memory total is unreadable (reported 0)");
    anyhow::ensure!(
        free <= total,
        "available memory ({free} bytes) exceeds total ({total} bytes)"
    );
    Ok(Capacity {
        free_bytes: free,
        total_bytes: total,
        utilization_pct: u32::try_from(u128::from(total - free) * 100 / u128::from(total)).ok(),
        label: "system memory".into(),
    })
}

/// A CPU sample, or an error when there is no real reading. With no CPUs
/// enumerated sysinfo's average is 0 or NaN, and `NaN as u32` is 0: either
/// would read as an idle machine.
fn cpu_capacity(usage: f32, cpus: usize) -> Result<Capacity> {
    anyhow::ensure!(cpus > 0, "no logical CPUs were enumerated");
    anyhow::ensure!(
        usage.is_finite() && usage >= 0.0,
        "CPU usage reading {usage} is not a percentage"
    );
    Ok(Capacity {
        free_bytes: 0,
        total_bytes: 0,
        // Rounding can carry a busy average just past 100.
        utilization_pct: Some((usage.round() as u32).min(100)),
        label: format!("{cpus} logical cpus"),
    })
}

/// Resolve a PID to a process name, for the presence sampler.
///
/// Only the Windows backend resolves foreground PIDs today; the Linux and
/// macOS backends will call this the same way once they land.
#[cfg_attr(not(windows), allow(dead_code))]
pub fn process_name(pid: u32) -> Option<String> {
    let mut sys = shared().lock().unwrap();
    let p = sysinfo::Pid::from_u32(pid);
    sys.refresh_processes(sysinfo::ProcessesToUpdate::Some(&[p]), true);
    sys.process(p).map(|p| {
        let n = p.name().to_string_lossy();
        n.strip_suffix(".exe").unwrap_or(&n).to_string()
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn memory_reserve_is_respected() {
        let c = Capacity {
            free_bytes: 8 * 1024 * 1024 * 1024,
            ..Default::default()
        };
        assert!(SystemMemory.lendable(&c, Some(4 * 1024 * 1024 * 1024)));
        assert!(!SystemMemory.lendable(&c, Some(16 * 1024 * 1024 * 1024)));
        assert!(SystemMemory.lendable(&c, None));
    }

    #[test]
    fn cpu_ceiling_is_respected() {
        let busy = Capacity {
            utilization_pct: Some(85),
            ..Default::default()
        };
        assert!(!SystemCpu.lendable(&busy, Some(60)));
        assert!(SystemCpu.lendable(&busy, Some(90)));
        assert!(!SystemCpu.lendable(&Capacity::default(), None));
    }

    #[test]
    fn cpu_usage_is_never_computed_from_refreshes_closer_than_sysinfo_allows() {
        // Two back-to-back samples, the first of which may be the process's
        // first CPU refresh: each must span the minimum update interval, or
        // the reading is near 0 % regardless of real load.
        let started = Instant::now();
        SystemCpu.sample().unwrap();
        SystemCpu.sample().unwrap();
        assert!(started.elapsed() >= sysinfo::MINIMUM_CPU_UPDATE_INTERVAL);
    }

    #[test]
    fn unreadable_memory_is_an_error_not_a_full_machine() {
        assert!(memory_capacity(0, 0).is_err());
        assert!(memory_capacity(8, 9).is_err());
        let c = memory_capacity(16 * 1024, 4 * 1024).unwrap();
        assert_eq!(c.utilization_pct, Some(75));
        assert_eq!(
            memory_capacity(u64::MAX, 0).unwrap().utilization_pct,
            Some(100)
        );
        assert_eq!(memory_capacity(8, 8).unwrap().utilization_pct, Some(0));
    }

    #[test]
    fn a_cpu_reading_without_cpus_or_a_number_is_an_error_not_idle() {
        assert!(cpu_capacity(0.0, 0).is_err());
        assert!(cpu_capacity(f32::NAN, 8).is_err());
        assert!(cpu_capacity(f32::INFINITY, 8).is_err());
        assert!(cpu_capacity(-1.0, 8).is_err());
        assert_eq!(cpu_capacity(0.0, 8).unwrap().utilization_pct, Some(0));
        assert_eq!(cpu_capacity(42.4, 8).unwrap().utilization_pct, Some(42));
        assert_eq!(cpu_capacity(100.4, 8).unwrap().utilization_pct, Some(100));
    }

    #[test]
    fn memory_probe_reads_the_real_machine() {
        let c = SystemMemory.sample().unwrap();
        assert!(c.total_bytes > 0);
        assert!(c.free_bytes <= c.total_bytes);
    }
}
