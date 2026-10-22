//! Optional numeric observations for dashboards; never used by admission.

use std::collections::HashMap;
use std::sync::Mutex;

use prometheus::{GaugeVec, IntGaugeVec, Opts, Registry};

use crate::plugin::probe::GpuTelemetry;

struct SensorSpec {
    name: &'static str,
    help: &'static str,
    value: fn(&GpuTelemetry) -> Option<f64>,
    maximum: f64,
    zero_valid: bool,
}

const SENSORS: [SensorSpec; 7] = [
    SensorSpec {
        name: "core_temperature_celsius",
        help: "GPU core temperature in degrees Celsius",
        value: |g| g.core_temperature_celsius,
        maximum: 150.0,
        zero_valid: true,
    },
    SensorSpec {
        name: "memory_temperature_celsius",
        help: "Qualified GPU memory temperature in degrees Celsius",
        value: |g| g.memory_temperature_celsius,
        maximum: 150.0,
        zero_valid: false,
    },
    SensorSpec {
        name: "power_watts",
        help: "GPU power draw in watts",
        value: |g| g.power_watts,
        maximum: 2000.0,
        zero_valid: true,
    },
    SensorSpec {
        name: "fan_speed_percent",
        help: "GPU fan speed in percent",
        value: |g| g.fan_speed_percent,
        maximum: 100.0,
        zero_valid: true,
    },
    SensorSpec {
        name: "sm_clock_megahertz",
        help: "GPU SM clock in megahertz",
        value: |g| g.sm_clock_megahertz,
        maximum: 10000.0,
        zero_valid: true,
    },
    SensorSpec {
        name: "memory_clock_megahertz",
        help: "GPU memory clock in megahertz",
        value: |g| g.memory_clock_megahertz,
        maximum: 40000.0,
        zero_valid: true,
    },
    SensorSpec {
        name: "memory_utilization_percent",
        help: "GPU memory activity in percent, independent of allocated bytes",
        value: |g| g.memory_utilization_percent,
        maximum: 100.0,
        zero_valid: true,
    },
];

pub struct GpuMetrics {
    sensors: Vec<GaugeVec>,
    info: IntGaugeVec,
    identities: Mutex<HashMap<String, [String; 3]>>,
}

impl GpuMetrics {
    pub fn new(registry: &Registry) -> Self {
        let sensors = SENSORS
            .iter()
            .map(|spec| {
                let gauge = GaugeVec::new(
                    Opts::new(format!("vacancyd_gpu_{}", spec.name), spec.help),
                    &["resource"],
                )
                .expect("GPU metric definition");
                registry
                    .register(Box::new(gauge.clone()))
                    .expect("GPU metric registration");
                gauge
            })
            .collect();
        let info = IntGaugeVec::new(
            Opts::new(
                "vacancyd_gpu_info",
                "Selected physical GPU identity; scrape and capacity freshness are separate",
            ),
            &["resource", "uuid", "pci_bus_id", "name"],
        )
        .expect("GPU identity metric definition");
        registry
            .register(Box::new(info.clone()))
            .expect("GPU identity metric registration");
        Self {
            sensors,
            info,
            identities: Mutex::new(HashMap::new()),
        }
    }

    pub fn record(&self, resource: &str, reading: Option<&GpuTelemetry>) {
        let mut identities = self.identities.lock().unwrap();
        let next = reading.map(|g| [g.uuid.clone(), g.adapter.to_string(), g.name.clone()]);
        if identities.get(resource) != next.as_ref() {
            if let Some(old) = identities.remove(resource) {
                // Absent labels are expected when a resource has no GPU observation.
                let _ = self
                    .info
                    .remove_label_values(&[resource, &old[0], &old[1], &old[2]]);
            }
            if let Some(identity) = next {
                self.info
                    .with_label_values(&[resource, &identity[0], &identity[1], &identity[2]])
                    .set(1);
                identities.insert(resource.to_owned(), identity);
            }
        }
        for (gauge, spec) in self.sensors.iter().zip(&SENSORS) {
            let value = reading.and_then(spec.value).filter(|v| {
                v.is_finite() && *v >= 0.0 && *v <= spec.maximum && (spec.zero_valid || *v > 0.0)
            });
            if let Some(value) = value {
                gauge.with_label_values(&[resource]).set(value);
            } else {
                // Removal, rather than zero or a retained gauge, communicates missing data.
                let _ = gauge.remove_label_values(&[resource]);
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use crate::metrics::Metrics;
    use crate::plugin::probe::GpuTelemetry;

    fn reading() -> GpuTelemetry {
        GpuTelemetry {
            uuid: "GPU-11111111-2222-3333-4444-555555555555".into(),
            adapter: "0000:0e:00.0".parse().unwrap(),
            name: "NVIDIA Example GPU".into(),
            core_temperature_celsius: Some(42.0),
            memory_temperature_celsius: None,
            power_watts: Some(172.75),
            fan_speed_percent: Some(0.0),
            sm_clock_megahertz: Some(2872.0),
            memory_clock_megahertz: Some(14801.0),
            memory_utilization_percent: Some(5.0),
        }
    }

    #[test]
    fn encoded_metrics_preserve_units_zero_and_missing_sensors() {
        let metrics = Metrics::new();
        metrics.gpu.record("gpu0", Some(&reading()));
        let text = metrics.render();
        for line in [
            "vacancyd_gpu_core_temperature_celsius{resource=\"gpu0\"} 42",
            "vacancyd_gpu_power_watts{resource=\"gpu0\"} 172.75",
            "vacancyd_gpu_fan_speed_percent{resource=\"gpu0\"} 0",
            "vacancyd_gpu_sm_clock_megahertz{resource=\"gpu0\"} 2872",
            "vacancyd_gpu_memory_clock_megahertz{resource=\"gpu0\"} 14801",
            "vacancyd_gpu_memory_utilization_percent{resource=\"gpu0\"} 5",
        ] {
            assert!(text.contains(line), "{line}\n{text}");
        }
        assert!(!text.contains("vacancyd_gpu_memory_temperature_celsius"));
        assert!(text.contains("pci_bus_id=\"0000:0e:00.0\""));
        assert!(!text.contains("index="));
    }

    #[test]
    fn missing_invalid_and_failed_readings_remove_previous_gauges() {
        let metrics = Metrics::new();
        let mut gpu = reading();
        metrics.gpu.record("gpu0", Some(&gpu));
        gpu.core_temperature_celsius = Some(f64::NAN);
        gpu.memory_temperature_celsius = Some(0.0);
        gpu.power_watts = None;
        gpu.fan_speed_percent = Some(101.0);
        metrics.gpu.record("gpu0", Some(&gpu));
        let text = metrics.render();
        for name in [
            "core_temperature_celsius",
            "memory_temperature_celsius",
            "power_watts",
            "fan_speed_percent",
        ] {
            assert!(!text.contains(&format!("vacancyd_gpu_{name}")));
        }
        metrics.gpu.record("gpu0", None);
        assert!(!metrics.render().contains("vacancyd_gpu_"));
    }

    #[test]
    fn identity_replacement_does_not_leave_another_adapter_series() {
        let metrics = Metrics::new();
        let mut gpu = reading();
        metrics.gpu.record("gpu0", Some(&gpu));
        gpu.uuid = "GPU-aaaaaaaa-bbbb-cccc-dddd-eeeeeeeeeeee".into();
        gpu.name = "NVIDIA Other GPU".into();
        metrics.gpu.record("gpu0", Some(&gpu));
        let text = metrics.render();
        assert!(!text.contains("GPU-11111111"));
        assert_eq!(
            text.lines()
                .filter(|line| line.starts_with("vacancyd_gpu_info{"))
                .count(),
            1
        );
        metrics.gpu.record("cpu", None);
        assert!(!metrics.render().contains("resource=\"cpu\""));
    }
}
