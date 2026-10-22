# Optional GPU telemetry metrics

`GET /metrics` under the existing `read` scope may include the following GPU
observations. This additive contract serves hardware dashboards. It does not
change capacity, pressure, admission, owner policy or authorization.

The NVIDIA capacity probe owns sampling. Sensor fields extend its existing
read-only `nvidia-smi` query; HTTP scrapes only encode cached observations. A
driver that rejects additional query fields falls back to the original capacity
query within the existing five-second budget, then keeps that legacy query for
the probe's lifetime. No additional polling process, GPU allocation, driver
installation or temperature control is introduced.

| Metric | Labels | Meaning and valid range |
| --- | --- | --- |
| `vacancyd_gpu_info` | `resource`, `uuid`, `pci_bus_id`, `name` | Value 1 for the selected physical adapter. PCI is canonical `0000:01:00.0`; UUID and product name are strings. Driver index is not an identity. |
| `vacancyd_gpu_core_temperature_celsius` | `resource` | Core temperature, 0–150 degrees Celsius. |
| `vacancyd_gpu_memory_temperature_celsius` | `resource` | Qualified memory temperature, greater than 0 through 150 degrees Celsius. Zero is unsupported, not a reading. |
| `vacancyd_gpu_power_watts` | `resource` | Current power, 0–2,000 watts. |
| `vacancyd_gpu_fan_speed_percent` | `resource` | Reported fan percentage, 0–100. Zero is valid. |
| `vacancyd_gpu_sm_clock_megahertz` | `resource` | SM clock, 0–10,000 MHz. |
| `vacancyd_gpu_memory_clock_megahertz` | `resource` | Memory clock, 0–40,000 MHz. |
| `vacancyd_gpu_memory_utilization_percent` | `resource` | Memory activity, 0–100 percent; independent from allocated bytes. |

Unavailable, missing, negative, nonfinite, out-of-range or `N/A` values produce
no series. Never encode unsupported sensors as zero. A newly missing value or
failed capacity observation removes its previous sensor series. Consumers must
also enforce the existing capacity observation age and Prometheus scrape age;
scrape time is not a hardware timestamp. No sensor availability can authorize
lending or invalidate otherwise usable capacity.

Identity is selected by the same configured selector as capacity, and old
identity labels are removed on replacement or failure. Only one identity is
exported per resource. Non-GPU probes produce no GPU series. There are no model,
process, account, request, prompt or device-control labels.

Existing consumers may ignore these metrics. New consumers must tolerate their
absence on older daemons and unsupported devices. No hotspot metric is promised:
the driver query does not supply it. Host credentials, endpoint and port remain
unchanged. Provider tests verify actual Prometheus encoding and missing/error
transitions; dashboard fixtures use these exact names and units.
