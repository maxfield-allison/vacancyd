# Logging and diagnostics

When vacancyd refuses work you expected it to lend, or lends when you expected it to refuse, three tools show why. The log level decides how much the service writes as it works. A temporary debug window raises that level for a while without a restart. The diagnostics export saves what the service saw and decided, including the last few hours of GPU pressure readings, whether or not debug logging was on at the time.

All three are part of the normal build. There is no separate debug build.

## The log level

```toml
[log]
level = "info"
```

`level` is `error`, `warn`, `info` (the default), `debug` or `trace`. `debug` and `trace` apply to vacancyd's own events; the HTTP and runtime libraries it uses stay at `info`, so the extra lines are vacancyd's reasoning rather than connection internals. Changing `level` takes effect at the next service start.

`RUST_LOG`, when it is set to a valid filter, replaces `[log] level` entirely. It accepts the full [`tracing` filter syntax](https://docs.rs/tracing-subscriber/latest/tracing_subscriber/filter/struct.EnvFilter.html), for example `RUST_LOG=info,vacancyd::proxy=trace`. An invalid value is ignored with a warning and `[log] level` applies. The packaged systemd unit does not set `RUST_LOG`, so the configuration decides. A previously copied unit may still set `Environment=RUST_LOG=info`; delete that line to let `[log] level` apply (see [upgrading](setup.md#linux-install-from-the-release-archive)).

`/status` reports what is in force under `logging`:

```json
"logging": {
  "level": "info",
  "source": "config",
  "configured_level": "info",
  "environment_override": false
}
```

`source` is `config`, `environment` (`RUST_LOG`, when `level` reads `custom`) or `debug_window`. `vacancyd status` prints a `logging:` line whenever the level is not the default `info` from the configuration.

Where the lines go: a foreground `vacancyd run` and the systemd journal receive them on standard output; the Windows service writes the bounded `vacancyd.log` described in [the Windows service guide](windows-service.md#service-log-retention).

## A temporary debug window

A debug window logs vacancyd's own events at `debug` (or keeps `trace` if the configuration already asks for it) and then turns itself off. It needs no restart, and a restart ends it: the window is held in memory only, so a service that starts again always uses the configured level. It ends at its time by the system clock as well as the service's own timer, whichever comes first, so a machine that sleeps past the end does not keep logging debug after it wakes.

| Where | Open | End early |
| ----- | ---- | --------- |
| Owner console (Windows) | Installation, **Debug logging for 30 minutes** | **End debug logging** |
| HTTP | `POST /admin/debug/enable`, optional body `{"minutes": n}` | `POST /admin/debug/disable` |
| `vacancyd admin-request` | `{"protocol_version": 1, "action": {"kind": "start_debug", "minutes": 30}}` | `{"protocol_version": 1, "action": {"kind": "end_debug"}}` |

A window lasts 30 minutes unless the request names another length from 1 to 240 minutes. Opening a window while one is open moves its end. A window is also kept short enough that its pressure records fill at most half of the Windows service log's 8 MiB: about 384 bytes per record, one record per tracked GPU per sample. One GPU at the default 5-second cadence allows the full 240 minutes; four GPUs sampled every second allow 45. A longer request is shortened to fit: the HTTP answer carries `requested_minutes`, `granted_minutes` and `limited`, the service log says so, and the console shows the time actually granted. Turning debug logging on or off needs an administrator, as switching a resource does: the console raises the Windows approval prompt, `admin-request` needs an elevated prompt on Windows or root or the admin group on Linux, and HTTP needs an `admin`-scoped token (see [authentication](authentication.md)). HTTP requests also need the `X-Vacancyd-Admin: 1` intent header.

While a window is open, `/status` reports `"source": "debug_window"`, its end as `debug_until` (RFC 3339, UTC) and `debug_remaining_s`.

With `RUST_LOG` in force, the window adds `vacancyd=debug` to that filter only when the filter logs vacancyd less finely than debug, so it never lowers a `RUST_LOG=vacancyd=trace`. A directive for a narrower target, such as `vacancyd::pressure=warn`, keeps its own level during the window. In either case `/status` reports `"source": "environment"` and `"level": "custom"` beside `debug_until`, because `RUST_LOG` and not the window decides what is written. The filter applies unchanged when the window ends.

Commands other than `vacancyd run` (the service itself) write their log lines to standard error, so `vacancyd diagnostics export` without `--out` writes only the JSON document to standard output.

At debug level the pressure sampler writes one line per tracked GPU per sample, with the target `vacancyd::pressure`:

```text
DEBUG vacancyd::pressure: pressure sample resource="gpu0" non_lender_pct=3.25 profile="attended" threshold_pct=25.0 target_pct=25.0 baseline_pct=3.5 calibrated=true calibration_samples=60 calibration_required=60 engaged=false above_for_s=0 mode="calibrated"
```

`threshold_pct` is the threshold in force, `baseline_pct` the learned quantile and `target_pct` the threshold that quantile asks for. The last two are left out until the profile's histogram holds `warmup_samples` readings.

That is roughly one line every five seconds per GPU at the default `owner.gpu_pressure.poll_ms`. A four-hour window on a two-GPU machine adds about 6,000 lines, well within the Windows service log's 8 MiB.

The service emits `info` events naming restored calibration or explaining why saved calibration was not restored: `index selector not cached`, `no saved entry`, `GPU identity mismatch`, `fixed mode; saved calibration kept`, `ignore list changed`, `no completed calibration saved` or `saved calibration invalid`. The active filter determines whether they reach the log. A restored line names any profile carried over from an older cache's baseline in `migrated_legacy_baseline`. See [calibration](calibration.md#persistence).

## Why work was reclaimed

Each reclaim emits an `info` event when it starts; the active filter determines whether it reaches the log:

```text
INFO vacancyd::yield_control: reclaiming borrowed work reclaim=7 trigger="resource-refused" refused="gpu0 engaged at 96.0% (threshold 25.0%)" upstreams=["ollama"] entries=[41]
```

`trigger` is `owner-veto` (with the veto's own cause as `veto_cause`) or `resource-refused`. For a refused resource the reason is one of:

- `engaged at P% (threshold T%)`: pressure from work vacancyd did not admit stayed above the threshold for the sustain time;
- `not calibrated (n/required samples)`: the attendance profile in force has not completed its first calibration, so the resource refuses until it does; the estimator's explanation follows the count when it has one;
- `invalid reading (detail)`: the sample was not a percentage of the device, so nothing could be judged from it;
- `unknown (Q reading invalidated: why; for Ns)`: the reading that had admitted work can no longer be trusted, and has stayed that way for `N` seconds, at least the pressure tracker's gap limit; a shorter gap reclaims nothing. `Q` is what it was then, `ready` or `stale`, and `why` is the failure that started the gap, such as a failed attribution sample, a GPU missing from the counter batch, unavailable adapter identity, or unresolved presence. The export carries the duration as `unknown_for_ms`.

Each confirmed cancellation still logs `cancelled borrowed work on owner return`, now with the same `reclaim` id, `trigger` and `refused` fields, so a search for the old message still finds it. The diagnostics export keeps the last 64 reclaims under `reclaims`, with the same causes, the upstreams and ledger entries each reached, each upstream's `on_owner_return` mode, whether its models were unloaded, and which entries were released and how. Two fields appear only when they apply:

- `abandoned_after_ms`: the reclaim stood down for that upstream because its trigger cleared while it waited, this many milliseconds after it began. Nothing was cancelled or unloaded. The log says `reclaim abandoned; its trigger cleared` with the same `reclaim` id.
- `unload_skipped_for_foreign_work`: `true` when the unload was skipped because the upstream was running or queueing work vacancyd did not admit. The log says `model unload skipped; the upstream holds work vacancyd did not admit`.

When a pressure reading recovers before its gap limit, nothing is reclaimed and the log says `GPU pressure reading recovered before running work was reclaimed` with how long it was unknown.

## The diagnostics export

The export is one JSON document holding:

- the daemon's version, operating system and architecture, and uptime;
- the complete `/status` document;
- the running settings, shaped like the [settings API](console-settings.md), with the revision this process loaded and the revision on disk now (they differ while a saved change waits for a restart);
- the log level, as in `/status`;
- the saved pressure calibration as read from its file at export time, or why it could not be read (`absent`, `unreadable`, `too_large`, `invalid`). A file in an older format is shown in the current layout, with `migrated_from` naming its schema;
- each tracked GPU's recent pressure samples, and the five busiest processes counted as non-lender work in its latest sample, by name and percentage;
- under each tracked GPU's `learned`, a summary of each attendance profile's histogram as of its latest sample: the quantiles 0.1, 0.25, 0.5, 0.75, 0.9, 0.95 and 0.99 (bin upper edges), the total decayed weight, the sample count and observed seconds, the learned quantile, the target and the threshold in force;
- the most recent reclaims and their causes (see above).

Credentials never appear in it. Settings carry only how many tokens and upstream credentials are configured, and upstream targets are shown with any user information and query string replaced by `***`, as `/status` shows them. The document does contain what `/status` already shows to a local reader: owner presence, foreground and owner applications, and media playback. It also names the busiest processes using each GPU outside vacancyd, which appear nowhere else: not in `/status`, metrics, or logs below debug. Read it before you share it.

| Where | How |
| ----- | --- |
| Owner console (Windows) | Installation, **Export diagnostics**, then choose a file |
| Command line | `vacancyd diagnostics export --out vacancyd-diagnostics.json` |
| HTTP | `GET /admin/diagnostics` with `X-Vacancyd-Admin: 1` |

Reading the export is like reading settings: any local account may do it over loopback, and a remote client needs an `admin`-scoped token. The command reads `127.0.0.1:9811` unless `--addr` names another API address, ignores `HTTP_PROXY`-style settings and does not follow redirects. Without `--out` it writes to standard output.

### Recent pressure samples

Every tracked GPU keeps its recent pressure samples in memory, whatever the log level, so you can export what led up to a refusal after it happened. Each sample records its time (`at_unix_ms`) and `quality`. A `ready` sample adds the non-lender percentage, the attendance profile, the threshold and baseline in force, and whether the profile was calibrated and pressure engaged. An `unknown` sample, written each time a reading is invalidated, adds the `reason` (at most 200 characters), such as a GPU missing from the counter batch or an unresolved presence reading, so the history has no silent gaps.

The history covers six hours at the pressure poll cadence: 4,320 samples at the default `poll_ms` of 5,000, and at most 21,600 samples at the one-second floor. A sample takes at most 56 bytes, so a GPU holds about 240 KiB at the default cadence and at most about 1.2 MiB. A repeated reason is stored once and shared. The export is indented for reading and takes about 250 bytes per sample, about 1 MiB per GPU at the default cadence. The history starts empty after a restart.

## Metrics

The pressure gauges carry the same decision for dashboards:

| Metric | Labels | Meaning |
| ------ | ------ | ------- |
| `vacancyd_resource_pressure_pct` | `resource` | non-lender utilisation of the last sample, in percent |
| `vacancyd_resource_pressure_threshold_pct` | `resource`, `profile` | engage threshold in force for the attendance profile |
| `vacancyd_resource_pressure_baseline_pct` | `resource`, `profile` | learned quantile; absent until the profile's histogram holds `warmup_samples` readings, and in fixed mode |
| `vacancyd_resource_pressure_target_pct` | `resource`, `profile` | threshold the learned quantile asks for; the threshold in force rises toward it at a bounded rate and falls to it at once; absent like the baseline |
| `vacancyd_resource_pressure_engaged` | `resource`, `profile` | 1 while pressure refuses the resource, 0 otherwise |

The first four are floating-point gauges. While a reading is unknown, `vacancyd_resource_pressure_pct` and `vacancyd_resource_pressure_engaged` have no series for that resource rather than keeping their last value; `vacancyd_resource_readiness{input="pressure"}` says why the reading is not ready. Every sample sets the per-profile gauges (threshold, baseline, target, `vacancyd_resource_calibrated` and `vacancyd_resource_calibration_samples`) for both attendance profiles: the observed profile from the sample, the other from what the estimator holds for it. A value therefore never lingers from the last time a profile was observed. `vacancyd_resource_pressure_engaged` describes the current sample, so it has a series only for the profile of the last sample.
