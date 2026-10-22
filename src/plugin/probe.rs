//! The **probe** seam: measuring how much of a resource is free.
//!
//! A probe answers one question — "how much of this thing is available right
//! now?" — for one kind of hardware. Probes must be cheap enough to run
//! every few seconds and **must not allocate on the resource they measure**;
//! a probe that reserved VRAM to find out how much VRAM was free would
//! defeat the daemon's entire purpose.
//!
//! To add a resource kind (AMD via `rocm-smi`, Intel via `xpu-smi`, disk,
//! network): implement [`Probe`], register the constructor in
//! [`ProbeRegistry::with_builtins`], and document the `kind` string.

use std::collections::HashMap;

use anyhow::{bail, Result};
use serde::Serialize;

use crate::config::ResourceConfig;
use crate::probes::gpu_id::PciAddress;

/// One measurement of one resource.
#[derive(Debug, Clone, Default, Serialize)]
pub struct Capacity {
    /// Bytes free. Zero for resources that are not measured in bytes
    /// (a CPU reports utilization instead).
    pub free_bytes: u64,
    /// Bytes total, or zero where not meaningful.
    pub total_bytes: u64,
    /// Utilization percent, 0-100; absent when the device cannot measure it.
    /// Memory capacity can still be valid without this independent reading.
    pub utilization_pct: Option<u32>,
    /// Human-readable identity, e.g. "NVIDIA Example GPU".
    pub label: String,
}

/// Optional dashboard observations, independent from admission capacity.
/// Missing/invalid sensors must never change whether usable capacity is returned.
#[derive(Debug, Clone)]
pub struct GpuTelemetry {
    pub uuid: String,
    pub adapter: PciAddress,
    pub name: String,
    pub core_temperature_celsius: Option<f64>,
    pub memory_temperature_celsius: Option<f64>,
    pub power_watts: Option<f64>,
    pub fan_speed_percent: Option<f64>,
    pub sm_clock_megahertz: Option<f64>,
    pub memory_clock_megahertz: Option<f64>,
    pub memory_utilization_percent: Option<f64>,
}

/// Measures one resource. Implementations are `Send + Sync` because the
/// collector loop owns them across await points.
pub trait Probe: Send + Sync {
    /// The `kind` string used in config, e.g. `"nvidia-gpu"`.
    fn kind(&self) -> &'static str;

    /// Take one measurement.
    fn sample(&mut self) -> Result<Capacity>;

    /// Cached numeric GPU observations from the latest successful sample only.
    /// Never initiate I/O here: metric encoding and callers only borrow the cache.
    fn gpu_telemetry(&self) -> Option<&GpuTelemetry> {
        None
    }

    /// Stable hardware identity learned by the latest successful sample.
    /// Resource kinds without a PCI-addressed adapter leave this absent.
    fn adapter(&self) -> Option<PciAddress> {
        None
    }

    /// Is this resource lendable on its own terms, ignoring per-model
    /// thresholds (which belong to the upstream, not the resource)?
    ///
    /// `reserve` is the config's kind-specific reservation: bytes that must
    /// stay free for a memory resource, or a maximum utilization percent
    /// for a CPU. The default treats the resource as always lendable, which
    /// is right for GPUs — their gating is per-model and lives in the gate.
    fn lendable(&self, _c: &Capacity, _reserve: Option<u64>) -> bool {
        true
    }
}

/// Constructs a probe from its config stanza.
pub type ProbeFactory = fn(&ResourceConfig) -> Result<Box<dyn Probe>>;

/// Maps config `kind` strings to probe constructors.
pub struct ProbeRegistry {
    factories: HashMap<&'static str, ProbeFactory>,
}

impl ProbeRegistry {
    pub fn new() -> Self {
        Self {
            factories: HashMap::new(),
        }
    }

    /// Every probe compiled into this build.
    pub fn with_builtins() -> Self {
        let mut r = Self::new();
        r.register("nvidia-gpu", crate::probes::nvidia::build);
        r.register("system-memory", crate::probes::system::build_memory);
        r.register("system-cpu", crate::probes::system::build_cpu);
        r
    }

    pub fn register(&mut self, kind: &'static str, f: ProbeFactory) {
        self.factories.insert(kind, f);
    }

    pub fn build(&self, cfg: &ResourceConfig) -> Result<Box<dyn Probe>> {
        match self.factories.get(cfg.kind.as_str()) {
            Some(f) => f(cfg),
            None => bail!(
                "unknown resource kind '{}' (known: {})",
                cfg.kind,
                self.kinds().join(", ")
            ),
        }
    }

    pub fn kinds(&self) -> Vec<&'static str> {
        let mut k: Vec<_> = self.factories.keys().copied().collect();
        k.sort_unstable();
        k
    }
}

impl Default for ProbeRegistry {
    fn default() -> Self {
        Self::with_builtins()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn builtins_are_registered() {
        let r = ProbeRegistry::with_builtins();
        assert_eq!(r.kinds(), vec!["nvidia-gpu", "system-cpu", "system-memory"]);
    }

    #[test]
    fn unknown_kind_names_the_alternatives() {
        let r = ProbeRegistry::with_builtins();
        let cfg = ResourceConfig {
            id: "x".into(),
            kind: "quantum-gpu".into(),
            selector: None,
            adapter: None,
            enabled: true,
            reserve: None,
        };
        let Err(e) = r.build(&cfg) else {
            panic!("unknown kind must not build");
        };
        let err = e.to_string();
        assert!(err.contains("unknown resource kind 'quantum-gpu'"), "{err}");
        assert!(err.contains("nvidia-gpu"), "{err}");
    }
}
