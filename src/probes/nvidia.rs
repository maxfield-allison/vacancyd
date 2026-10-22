//! NVIDIA GPU probe, via `nvidia-smi`.
//!
//! A subprocess rather than linked NVML, deliberately: `nvidia-smi` ships
//! with every driver on every platform vacancyd targets (Windows, Linux,
//! Jetson), needs no bindings or dlopen fallback, has no driver-version
//! coupling. Read-only queries allocate no GPU memory; their subprocess and
//! driver-query overhead still needs measuring on the target machine.

use anyhow::{bail, Context, Result};
use std::time::{Duration, Instant};

use crate::config::ResourceConfig;
use crate::plugin::probe::{Capacity, GpuTelemetry, Probe};
use crate::probes::gpu_id::PciAddress;

const QUERY: &str = "index,uuid,name,pci.bus_id,memory.total,memory.free,utilization.gpu";
const SENSOR_QUERY: &str = "index,uuid,name,pci.bus_id,memory.total,memory.free,utilization.gpu,temperature.gpu,temperature.memory,power.draw,fan.speed,clocks.sm,clocks.mem,utilization.memory";
const QUERY_TIMEOUT: Duration = Duration::from_secs(5);

pub struct NvidiaGpu {
    /// GPU UUID ("GPU-…"), an index ("0"), or None for the first card.
    selector: Option<String>,
    adapter: Option<PciAddress>,
    telemetry: Option<GpuTelemetry>,
    sensor_fields_supported: bool,
}

pub fn build(cfg: &ResourceConfig) -> Result<Box<dyn Probe>> {
    Ok(Box::new(NvidiaGpu {
        selector: cfg.selector.clone(),
        adapter: None,
        telemetry: None,
        sensor_fields_supported: true,
    }))
}

impl NvidiaGpu {
    fn sample_with(
        &mut self,
        mut read: impl FnMut(&str, Duration) -> Result<Option<Vec<GpuRow>>>,
    ) -> Result<Capacity> {
        self.telemetry = None;
        let started = Instant::now();
        let query = if self.sensor_fields_supported {
            SENSOR_QUERY
        } else {
            QUERY
        };
        let gpus = match read(query, QUERY_TIMEOUT)? {
            Some(gpus) => gpus,
            None => {
                // Legacy drivers may reject an optional column. Cache that fact,
                // preserving capacity and the original total subprocess budget.
                self.sensor_fields_supported = false;
                let remaining = QUERY_TIMEOUT.saturating_sub(started.elapsed());
                anyhow::ensure!(!remaining.is_zero(), "GPU query timeout budget exhausted");
                read(QUERY, remaining)?.context("driver rejected capacity query")?
            }
        };
        let gpu = select(&gpus, self.selector.as_deref())
            .ok_or_else(|| anyhow::anyhow!("no GPU matched selector {:?}", self.selector))?;
        self.adapter = Some(gpu.adapter);
        self.telemetry = Some(gpu.telemetry.clone());
        Ok(gpu.cap.clone())
    }
}

impl Probe for NvidiaGpu {
    fn kind(&self) -> &'static str {
        "nvidia-gpu"
    }

    fn sample(&mut self) -> Result<Capacity> {
        self.sample_with(read_inventory)
    }

    fn adapter(&self) -> Option<PciAddress> {
        self.adapter
    }

    fn gpu_telemetry(&self) -> Option<&GpuTelemetry> {
        self.telemetry.as_ref()
    }
}

#[derive(Debug)]
pub(crate) struct GpuRow {
    pub index: u32,
    pub uuid: String,
    pub adapter: PciAddress,
    pub cap: Capacity,
    pub telemetry: GpuTelemetry,
}

pub(crate) fn inventory() -> Result<Vec<GpuRow>> {
    read_inventory(QUERY, QUERY_TIMEOUT)?.context("driver rejected capacity query")
}

fn read_inventory(query: &str, timeout: Duration) -> Result<Option<Vec<GpuRow>>> {
    let program = crate::child_process::nvidia_smi().context("locating nvidia-smi")?;
    let mut command = std::process::Command::new(program);
    command.args([
        &format!("--query-gpu={query}"),
        "--format=csv,noheader,nounits",
    ]);
    let out = crate::child_process::output(&mut command, timeout)
        .context("running nvidia-smi (is the NVIDIA driver installed?)")?;
    if !out.status.success() {
        if unsupported_field(&out.stdout, &out.stderr) {
            return Ok(None);
        }
        bail!("nvidia-smi exited with {}", out.status);
    }
    parse(&String::from_utf8_lossy(&out.stdout)).map(Some)
}

fn unsupported_field(stdout: &[u8], stderr: &[u8]) -> bool {
    [stdout, stderr].iter().any(|stream| {
        String::from_utf8_lossy(stream)
            .to_ascii_lowercase()
            .contains("not a valid field")
    })
}

fn sensor(fields: &[&str], index: usize, maximum: f64, exclude_zero: bool) -> Option<f64> {
    fields.get(index)?.parse::<f64>().ok().filter(|value| {
        value.is_finite() && *value >= 0.0 && *value <= maximum && (!exclude_zero || *value > 0.0)
    })
}

fn parse(stdout: &str) -> Result<Vec<GpuRow>> {
    const MIB: u64 = 1024 * 1024;
    let mut gpus = Vec::new();
    for line in stdout.lines().filter(|l| !l.trim().is_empty()) {
        let f: Vec<&str> = line.split(',').map(str::trim).collect();
        if f.len() < 7 {
            bail!("unexpected nvidia-smi line: {line:?}");
        }
        let total_bytes = f[4]
            .parse::<u64>()
            .context("memory.total")?
            .checked_mul(MIB)
            .context("memory.total exceeds byte range")?;
        let free_bytes = f[5]
            .parse::<u64>()
            .context("memory.free")?
            .checked_mul(MIB)
            .context("memory.free exceeds byte range")?;
        anyhow::ensure!(
            free_bytes <= total_bytes,
            "memory.free exceeds memory.total"
        );
        // Device-wide utilization is independent of memory capacity and
        // attributed owner pressure. Preserve usable memory when this field
        // is unavailable or invalid, without manufacturing an idle reading.
        let utilization_pct = f[6].parse::<u32>().ok().filter(|value| *value <= 100);
        gpus.push(GpuRow {
            index: f[0].parse().context("gpu index")?,
            uuid: f[1].to_string(),
            adapter: f[3].parse().context("pci.bus_id")?,
            cap: Capacity {
                total_bytes,
                free_bytes,
                utilization_pct,
                label: f[2].to_string(),
            },
            telemetry: GpuTelemetry {
                uuid: f[1].to_string(),
                adapter: f[3].parse().context("pci.bus_id")?,
                name: f[2].to_string(),
                core_temperature_celsius: sensor(&f, 7, 150.0, false),
                memory_temperature_celsius: sensor(&f, 8, 150.0, true),
                power_watts: sensor(&f, 9, 2000.0, false),
                fan_speed_percent: sensor(&f, 10, 100.0, false),
                sm_clock_megahertz: sensor(&f, 11, 10000.0, false),
                memory_clock_megahertz: sensor(&f, 12, 40000.0, false),
                memory_utilization_percent: sensor(&f, 13, 100.0, false),
            },
        });
    }
    Ok(gpus)
}

fn select<'a>(gpus: &'a [GpuRow], selector: Option<&str>) -> Option<&'a GpuRow> {
    match selector {
        None => gpus.first(),
        Some(s) if s.starts_with("GPU-") => gpus.iter().find(|g| g.uuid == s),
        Some(s) => s
            .parse::<u32>()
            .ok()
            .and_then(|i| gpus.iter().find(|g| g.index == i)),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const TWO_CARDS: &str =
        "0, GPU-aaaaaaaa-bbbb-cccc-dddd-eeeeeeeeeeee, NVIDIA Example GPU 8GB, 00000000:01:00.0, 8192, 6150, 11\n\
         1, GPU-11111111-2222-3333-4444-555555555555, NVIDIA Example GPU 16GB, 00000000:0E:00.0, 16303, 13179, 5\n";

    #[test]
    fn parses_and_selects() {
        let g = parse(TWO_CARDS).unwrap();
        assert_eq!(g.len(), 2);
        let by_uuid = select(&g, Some("GPU-11111111-2222-3333-4444-555555555555")).unwrap();
        assert_eq!(by_uuid.cap.free_bytes, 13179 * 1024 * 1024);
        assert_eq!(by_uuid.cap.label, "NVIDIA Example GPU 16GB");
        assert_eq!(by_uuid.adapter.to_string(), "0000:0e:00.0");
        assert_eq!(
            select(&g, Some("0")).unwrap().cap.label,
            "NVIDIA Example GPU 8GB"
        );
        assert!(select(&g, Some("GPU-nope")).is_none());
    }

    #[test]
    fn unavailable_utilization_preserves_memory_without_claiming_idle() {
        for value in ["[N/A]", "N/A", "", "invalid", "-1", "101", "4294967296"] {
            let g = parse(&format!(
                "0, GPU-x, Some Card, 00000000:01:00.0, 8192, 4096, {value}\n"
            ))
            .unwrap();
            assert_eq!(g[0].cap.utilization_pct, None, "{value}");
            assert_eq!(g[0].cap.free_bytes, 4096 * 1024 * 1024);
            assert_eq!(g[0].cap.total_bytes, 8192 * 1024 * 1024);
        }
        for value in [0, 1, 100] {
            let g = parse(&format!(
                "0, GPU-x, Some Card, 00000000:01:00.0, 8192, 4096, {value}\n"
            ))
            .unwrap();
            assert_eq!(g[0].cap.utilization_pct, Some(value));
        }
    }

    #[test]
    fn rejects_impossible_or_overflowing_memory() {
        for (total, free) in [
            ("8192", "8193"),
            ("18446744073709551615", "0"),
            ("8192", "18446744073709551615"),
            ("[N/A]", "0"),
            ("8192", "[N/A]"),
        ] {
            assert!(parse(&format!(
                "0, GPU-x, Some Card, 00000000:01:00.0, {total}, {free}, 0\n"
            ))
            .is_err());
        }
    }

    #[test]
    fn optional_sensors_select_the_same_physical_adapter_as_capacity() {
        let rows = parse("0, GPU-first, First GPU, 00000000:03:00.0, 8192, 7689, 0, 43, N/A, [N/A], 0, 210, 7001, 7\n1, GPU-second, Second GPU, 00000000:0F:00.0, 16303, 8186, 99, 59, 86, 298.59, 30, 2857, 14801, 22\n").unwrap();
        let first = select(&rows, Some("GPU-first")).unwrap();
        assert_eq!(first.telemetry.fan_speed_percent, Some(0.0));
        assert_eq!(first.telemetry.power_watts, None);
        assert_eq!(first.telemetry.memory_temperature_celsius, None);
        let second = select(&rows, Some("GPU-second")).unwrap();
        assert_eq!(second.cap.utilization_pct, Some(99));
        assert_eq!(second.telemetry.core_temperature_celsius, Some(59.0));
        assert_eq!(second.telemetry.memory_temperature_celsius, Some(86.0));
        assert_eq!(second.telemetry.power_watts, Some(298.59));
        assert_eq!(second.telemetry.adapter.to_string(), "0000:0f:00.0");
    }

    #[test]
    fn invalid_optional_columns_do_not_invalidate_capacity() {
        for value in ["", "N/A", "[N/A]", "NaN", "inf", "-1", "99999999999999"] {
            let rows = parse(&format!("0, GPU-x, GPU, 0000:01:00.0, 8192, 4096, 0, {value}, {value}, {value}, {value}, {value}, {value}, {value}\n")).unwrap();
            let gpu = &rows[0];
            assert_eq!(gpu.cap.free_bytes, 4096 * 1024 * 1024);
            assert!(gpu.telemetry.core_temperature_celsius.is_none());
            assert!(gpu.telemetry.memory_temperature_celsius.is_none());
            assert!(gpu.telemetry.power_watts.is_none());
            assert!(gpu.telemetry.fan_speed_percent.is_none());
            assert!(gpu.telemetry.sm_clock_megahertz.is_none());
            assert!(gpu.telemetry.memory_clock_megahertz.is_none());
            assert!(gpu.telemetry.memory_utilization_percent.is_none());
        }
    }

    fn probe() -> NvidiaGpu {
        NvidiaGpu {
            selector: Some("GPU-11111111-2222-3333-4444-555555555555".into()),
            adapter: None,
            telemetry: None,
            sensor_fields_supported: true,
        }
    }

    #[test]
    fn legacy_fallback_is_cached_and_shares_the_original_deadline() {
        let mut gpu = probe();
        let mut calls = Vec::new();
        gpu.sample_with(|query, timeout| {
            calls.push((query.to_owned(), timeout));
            if query == SENSOR_QUERY {
                Ok(None)
            } else {
                parse(TWO_CARDS).map(Some)
            }
        })
        .unwrap();
        assert_eq!(calls.len(), 2);
        assert_eq!(calls[0].0, SENSOR_QUERY);
        assert_eq!(calls[1].0, QUERY);
        assert!(calls[1].1 <= QUERY_TIMEOUT);
        assert_eq!(
            gpu.telemetry.as_ref().unwrap().core_temperature_celsius,
            None
        );
        gpu.sample_with(|query, _| {
            assert_eq!(query, QUERY);
            parse(TWO_CARDS).map(Some)
        })
        .unwrap();
    }

    #[test]
    fn unsupported_field_is_recognized_on_either_cli_output_channel() {
        let error = b"Field \"temperature.memory\" is not a valid field to query.";
        assert!(unsupported_field(error, b""));
        assert!(unsupported_field(b"", error));
        assert!(!unsupported_field(b"", b"Failed to initialize NVML"));
    }

    #[test]
    fn successful_sampling_uses_one_query_and_failure_clears_cached_sensors() {
        let mut gpu = probe();
        let mut calls = 0;
        gpu.sample_with(|query, _| {
            calls += 1;
            assert_eq!(query, SENSOR_QUERY);
            parse(TWO_CARDS).map(Some)
        })
        .unwrap();
        assert_eq!(calls, 1);
        assert!(gpu.gpu_telemetry().is_some());
        assert!(gpu.sample_with(|_, _| bail!("driver timed out")).is_err());
        assert!(gpu.gpu_telemetry().is_none());
        assert!(gpu.sensor_fields_supported);
    }

    #[cfg(windows)]
    #[test]
    #[ignore = "requires NVIDIA hardware and a real Windows session"]
    fn live_pci_join_reaches_the_pressure_counter_luid() {
        let mut command = std::process::Command::new(crate::child_process::nvidia_smi().unwrap());
        command.args([
            &format!("--query-gpu={QUERY}"),
            "--format=csv,noheader,nounits",
        ]);
        let output = crate::child_process::output(&mut command, Duration::from_secs(5))
            .expect("nvidia-smi should run");
        assert!(output.status.success());
        let gpus = parse(&String::from_utf8_lossy(&output.stdout)).unwrap();
        let adapters = crate::probes::gpu_id::windows_adapters().unwrap().adapters;
        let counters = crate::probes::gpu_procs::sample().expect("GPU counters should be readable");

        for gpu in gpus {
            let matches: Vec<_> = adapters
                .iter()
                .filter(|adapter| adapter.pci == gpu.adapter)
                .collect();
            assert_eq!(
                matches.len(),
                1,
                "{} must map to exactly one Windows adapter: {matches:?}",
                gpu.adapter
            );
            assert!(
                counters.observed_luids.contains(&matches[0].luid),
                "{} mapped to {}, which was absent from the counter batch",
                gpu.adapter,
                matches[0].luid
            );
        }
    }
}
