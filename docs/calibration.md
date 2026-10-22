# GPU pressure calibration

The default `owner.gpu_pressure.mode` is `"calibrated"`. vacancyd learns each lent GPU's non-lender utilisation and uses it to set a threshold within the configured floor and cap. After calibration, sustained valid readings at or above that threshold engage pressure after `sustain_s`. Owner work below it may remain undetected.

## What is learned

Every successful reading of a GPU's non-lender utilisation goes into a histogram of half-percent bins, including readings taken while pressure is engaged. Locked and unlocked sessions keep separate histograms, because a locked desktop composites far less than an active one; completing one does not complete the other.

Older readings fade. Before each reading is added, the weights already in the histogram decay with a half-life of `learn_half_life_h` (72 hours by default), measured in **observed** time: the interval since the previous reading of the same attendance profile. A gap between good readings longer than the tracker's gap limit (three times `poll_ms`, at least 20 seconds, so one missed reading is tolerated), a failed or invalid read, time spent in the other profile and time the service was stopped all count as zero. A machine that was switched off for a week has not forgotten a week of its habits.

The **target** threshold is the `learn_quantile` (0.75 by default) of that histogram plus `margin_pct` (20), clamped between `floor_pct` (25) and `cap_pct` (40). Quantiles report the upper edge of their bin, so a learned value is never below a reading it stands for.

The quantile is high on purpose. A desktop that idles at 5–14% most of the day and composites at 25–39% every afternoon has two normal states, and the threshold has to clear both. A low quantile would learn only the quiet one and refuse lending through every busy afternoon.

## Why the cap is the guard

Because engaged readings are learned too, a long render or a weekend of gaming would, left alone, raise the threshold until the work no longer registered. The cap prevents that. `cap_pct` is the highest threshold calibration may learn. Valid, calibrated samples at or above it engage pressure after `sustain_s`; owner work below it may remain undetected. Before this version `cap_pct` was the highest baseline accepted as calibration; the number is unchanged and its job is the same, but it now bounds the threshold.

The cap runs at no more than 90%: at 100, a render holding a card near full load could raise the threshold past itself within a day. The floor still wins over the cap, as before: the threshold is never below `floor_pct`. A configuration from an earlier version with `cap_pct` below `floor_pct`, or above 90, still loads; the service logs a warning at startup and in `check-config` and runs with the cap at the floor, or at 90. A new save from the owner console requires `floor_pct <= cap_pct <= 90` in calibrated mode.

The cap also bounds a busy first calibration. Sustained readings at or above the resulting threshold engage pressure after `sustain_s`; owner work below it may remain undetected. When the learned quantile itself is above the cap, `/status` carries the hint `learned load is above the calibration cap; target held at cap`.

This does not prove that work below the cap is harmless. A busy desktop and a light owner workload can look identical to this estimator. A calibrated threshold is not a recommendation to disable owner-presence protection, and vacancyd never changes that policy itself.

## Rise slowly, fall at once

The threshold in force follows the target asymmetrically:

- When the target is lower, the threshold drops to it on that reading. Protecting the owner sooner is always safe.
- When the target is higher, the threshold rises toward it by at most `raise_pct_per_h` percentage points (1 by default) per observed hour.

So a few hours of heavy work move the threshold a few points at most, even below the cap, and a desktop that has become busier stops causing nuisance refusals over hours rather than at once. The owner console shows this state as, for example, `Threshold 34%, rising toward 40% (learned from 52 h)`.

## First calibration

A profile with no saved learning needs `warmup_samples` (60) successful readings before lending. Elapsed time is that count multiplied by the actual sampling cadence, plus any probe delays; it is not a fixed five-minute timer. When the count is reached the threshold is set straight to the target, without the rise limit. Unknown attendance, a failed or invalid reading, or a switch of attendance profile pauses the count without discarding what was collected, and none of them ages the histogram. Missing or stale measurements still refuse admission. A restart does start an unfinished first calibration again, because only completed calibration is saved.

Until then, `vacancyd status` and `/status` report progress, GPU-dependent upstreams return `503` with a calibration explanation, and `/readyz` stays unready. `/status` reports, per resource, the threshold in force (`threshold_pct`), the learned quantile (`baseline_pct`, absent until the histogram holds `warmup_samples` readings), the `target_pct`, and how much the histogram has seen (`learned_samples`, `learned_s`). The metrics are in [diagnostics](diagnostics.md#metrics). In fixed mode, `calibrated` means the configured threshold is usable, not that learning occurred.

## Fixed mode

`mode = "fixed"` requires `threshold_pct` greater than zero. It skips learning, but not attribution, freshness checks or pressure hysteresis. It is for operators who have measured their hardware. A nonzero `threshold_pct` without fixed mode is a configuration error. Fixed mode neither uses nor overwrites the saved calibration, so switching back to `calibrated` restores it.

`mode = "adaptive"` is retired. A configuration that names it still loads, runs as `calibrated`, and logs a warning at startup and in `check-config`. The learned threshold does what adaptive mode was for: it follows the machine without letting a running job raise it past the cap.

## Persistence

The histograms and thresholds in force are saved every five minutes to the pressure cache, `vacancyd-pressure.json` beside the TOML configuration unless `state_file` names another location. A saved entry is restored when the configured GPU UUID and the `ignore` list (compared without case, order or repeats) match what it was learned with. The PCI address is not part of the match: the UUID names the card, and a change elsewhere in the machine can renumber its bus. Entries saved by earlier versions used the UUID and PCI address together. They restore for the same UUID at any address, and later saves use the UUID alone. Everything else is applied to the histogram afterwards, so changing it keeps the learning: `learn_quantile`, `learn_half_life_h`, `raise_pct_per_h`, `margin_pct`, `floor_pct`, `cap_pct`, `warmup_samples`, the timings and the upstream list. A restored threshold is bounded by the current floor and cap.

The borrower ports are not part of the match, although they decide which processes are subtracted. An idle borrower adds almost nothing to a card, and the port list changes whenever an upstream is added for another GPU.

Only completed calibration is saved; an unfinished first calibration starts again after a restart. Index selectors such as `"0"` are usable but do not restore or save calibration, because device order can change. The cache file is compact JSON of a few KiB per GPU.

A `state_file` outside the default location must sit in a directory only the service can change: on Linux one owned by the service account or root that neither its group nor other accounts can write, on Windows one inside `ProgramData\vacancyd` whose permissions, and those of every folder between it and `ProgramData\vacancyd`, grant only Administrators and LocalSystem. Another account able to write there could plant a calibration that decides when lending is refused. The service neither reads nor writes a cache in any other directory: it logs `pressure cache not used` at `warn` with the reason and keeps calibration for that run only, and `check-config` warns about the same directory. The service checks the path before creating a missing directory with private permissions. On Windows, it refuses a cache directory outside `ProgramData\vacancyd`, whether or not it exists, and a path through a junction or other reparse point. Each save writes a newly created, private staging file and renames it over the cache, so a link planted in the directory is never followed.

When the service starts tracking a GPU it logs, at `info`, either `restored validated GPU calibration` or `saved GPU calibration not restored` with the reason: `index selector not cached`, `no saved entry`, `GPU identity mismatch`, `fixed mode; saved calibration kept`, `ignore list changed`, `no completed calibration saved` or `saved calibration invalid`. A restored line names any profile carried over from an older cache in `migrated_legacy_baseline`, and any profile whose saved data was unusable in `invalid_profile`. A cache file that is unreadable, larger than 1 MiB or invalid is logged at `warn` with that state, and every GPU calibrates from new samples.

## Upgrading from a frozen baseline

Earlier versions froze a low-quantile baseline from the first quiet readings and saved it in a schema 2 or schema 3 cache. The cache is now schema 4. An older file is read, and each saved baseline becomes a starting threshold of baseline plus `margin_pct`, clamped between the floor and the cap, using the configuration in force when it is first restored. That profile is calibrated at once, so lending continues across the upgrade without new samples. No readings are invented: its histogram starts empty. The histogram may not lower that starting threshold until it has learned from 24 hours of observed time, because a few minutes of readings are exactly the noisy anchor the frozen baseline was; without the wait, a card whose saved 13% gave 33% would drop to the 25% floor in its first quiet minutes and take about fifteen hours to climb back. Rises toward a higher target are rate-limited as always. The cache records that a profile is still maturing, so a restart within the 24 hours keeps the protection. After that the normal rule applies. The next save writes schema 4. An earlier vacancyd ignores a schema 4 file and calibrates again, so a downgrade costs one calibration.

`baseline_quantile` and `baseline_window_s` are deprecated. They are still accepted so an older configuration loads, they are ignored, and the service warns about each one present at startup and in `check-config`. Every earlier console save wrote `baseline_quantile = 0.25`; that value never reaches the learned threshold. Remove both keys, or save settings from the owner console, which removes them.

## Recalibrating

To start from nothing, stop vacancyd, move the configured pressure cache aside, and start it again. Lending of each GPU waits for its first calibration. Keep the old file if you need to reverse the reset.

The owner console shows calibration progress and the rising state, and its Settings page can change the mode, fixed threshold, learning settings, sustain time and exceptions for the next service start (see the [settings contract](console-settings.md)). It cannot request recalibration; use the stopped-service procedure above. The [diagnostics export](diagnostics.md#the-diagnostics-export) includes the saved file as read at export time, a summary of each live histogram, and the last six hours of pressure samples, and a [debug window](diagnostics.md#a-temporary-debug-window) logs each sample's decision.

Linux and macOS still lack attributed GPU backends; choosing a mode does not supply those missing measurements.
