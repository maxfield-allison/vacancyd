# Owner console settings contract

Owner: vacancyd. Consumers: the installed Windows owner console and the local vacancyd service. This contract adds durable lending policy to the existing runtime Pause, Resume, and resource switches. Runtime controls continue to affect the current process only. A settings save changes the protected configuration. Model and default capacity requirements apply to subsequent admissions after the durable save; other settings take effect at the next service start.

## Consumer need

The workstation owner needs settings grouped into task tabs, with common controls first and advanced controls disclosed within each task. Search includes both levels, reveals the matching control, and preserves unsaved edits when moving between tabs. Navigation and search are local preferences, not configuration changes. The renderer must not receive raw TOML, credentials, credential paths, raw configuration, or installation filesystem paths. The typed whitelist includes tuning, hardware binding, and network topology; editing those values grants no arbitrary network, shell, or filesystem access to the renderer.

Common settings are:

- whether an unlocked interactive session counts as owner presence;
- the idle time after which an unlocked session may lend, where zero disables idle lending;
- whether media playback counts as owner presence;
- the owner return hold time;
- each configured resource's default enabled state.

Advanced settings are:

- process names that always count as owner activity;
- media-holder exceptions;
- whether the Windows sign-in screen keeps refusing (`sign_in_screen_is_presence`, off by default, so a PC waiting at sign-in lends like any signed-out machine).

GPU pressure has its own tab for enabled state, calibrated or fixed mode, a fixed threshold, sustain time, and process exceptions. Its advanced settings hold the cap, margin and floor, the learning quantile, half-life and rise limit, the warmup sample count and the polling interval (see [calibration](calibration.md)). A configuration naming the retired `adaptive` mode is shown as calibrated, and saving writes `calibrated`. Workloads has per-upstream reclaim controls for unload support, cancel/drain/finish behavior, and the bounded drain time. For an upstream exempt from the owner veto (`owner_scope = "resource"`), those controls apply only when GPU pressure reclaims it, and the console says so on its row.

Each workload has a separate advanced permissions disclosure:

- `owner_scope = "host"` protects owner activity. Choosing `"resource"` allows lending while the owner is present. The warning explains that only GPU pressure on watched hardware can reclaim this work; memory and CPU capacity checks do not stop running work. An upstream without a pressure-watched dependency warns but is not rejected.
- `management = "deny"` refuses borrower management and private-data routes. Choosing `"allow"` permits access to upstream management and owner data. The warning says to allow this only for borrowers trusted to manage the upstream.

Saving these permissions requires the same administrator authorization as other settings. They take effect after a service restart. Current exemption notices still come from `/status`, never from unsaved controls or a saved policy the running service has not adopted.

The advanced disclosure is a renderer preference. It is not persisted and does not change which fields the service validates.

## Service boundary

The service owns persistence. Reading and saving use two channels:

- `GET /admin/settings` on the fixed loopback API returns the current whitelisted settings. It requires `X-Vacancyd-Admin: 1`; any local user may read it.
- Saving writes the Administrators-only configuration, so it needs an administrator: an elevated member of the local Administrators group, LocalSystem, or an `admin`-scoped token. On Windows the console saves over the service's admin pipe, where the service reads the caller's account from the kernel (see [local administration on Windows](authentication.md#local-administration-on-windows)). A standard console's save raises the Windows approval prompt, which runs `vacancyd.exe admin-request` elevated with the staged document. Declining the prompt saves nothing and the console says so. `POST /admin/settings` accepts the same document from an `admin` token; on Windows and Linux a loopback request without one is refused with `403` `administrator_required`. On Linux, root or a member of the `vacancyd-admin` group saves with `vacancyd admin-request` over the admin socket (see [local administration on Linux](authentication.md#local-administration-on-linux)). On macOS loopback trust still permits it in this release.

The service accepts only the documented typed settings, including listeners and upstream target URLs. Credential values, installation paths and arbitrary TOML remain outside this boundary. The pipe request carries the same document and is parsed by the same strict reader, so validation, stale-revision and persistence answers do not depend on the channel. The console reads settings with the same bounded, no-proxy, no-redirect client as its status polling.

The Tauri boundary exposes only `read_settings` and `save_settings`. Their capability is granted to the packaged `main` window. No filesystem, shell, service-control or arbitrary HTTP capability is added to the renderer; the approval prompt is started by the native host.

## Tuning fields

The shared Rust wire types in `src/settings.rs` are the canonical field definitions. The TypeScript consumer is checked against encoded Rust fixtures, including strict request shape. All request objects reject unknown fields. Existing fields remain required; the expansion adds:

- `owner.presence` (`auto` or `headless`) and `owner.media_poll_ms`. Headless disables desktop and session evidence; named activity processes remain visible and conflicts fail validation.
- Pressure cap, margin, floor, learning quantile, half-life, rise limit, warmup samples and polling interval. Fixed mode uses its threshold and polling interval; disabled calibration fields keep their saved values, while the console retains their drafts across mode changes.
- `poll_interval_ms`, `server_listen`, and `server_limits`, with the listener limits and units described in the configuration reference. An unset buffered-body limit inherits the larger of 256 MiB and the individual request-body limit.
- Resource `reserve` (decimal string or null), `selector` and PCI `adapter` (string or null). RAM reserves are bytes; CPU reserve is its utilization ceiling in percent. IDs and kinds stay immutable; hardware binding and reserve edits require restart.
- Upstream completion timeout (zero expires tracking immediately and retains the lease), authorization forwarding, listener, target, primary resource, additional dependencies, default free MiB, and a list of named model requirements. Topology and permissions require restart. Forwarding sends the borrower's bearer token to the target; never enable it for an untrusted target, and it cannot coexist with a stored upstream credential.
- Each model has a unique `name`, `mode` (`scalar` or `per_resource`), decimal-string `min_free_mib` and `resident_min_free_mib`, and `resources` entries with unique `id` and decimal-string `min_free_mib`. Scalar mode requires an empty resource list; per-resource mode requires scalar `min_free_mib = "0"`. Dependencies must exist. Resident requirements cannot exceed any applicable load requirement. The daemon's whole-config validator remains decisive.

Wide capacity values are canonical unsigned decimal strings from 0 through 9223372036854775807; leading zeros, signs, whitespace, fractions and exponents are rejected. Other integer wire values must be safe JavaScript integers within the daemon's configuration bounds. No integer is rounded before conversion. Lists are bounded and duplicate models/resources fail validation.

Responses add `application`: `capacity_revision` names the revision used by live admission, or null when startup provenance is unavailable; `restart_required` compares all saved restart-only fields with the startup settings. A partial live update never marks the whole configuration revision active. Every capacity decision uses one effective snapshot for the gate, aliases, residency and ledger prior check; status uses that same snapshot. Publication holds the admission mutex only after durable persistence and while the settings-write mutex still serializes saves. Model maps must also validate against running dependencies: when a restart-only dependency edit conflicts with a live budget, save the topology and restart before adding that budget. Existing active and held leases and reclaim owners remain intact.

The console shows saved budgets beside separately observed resident model state. Missing or stale observations cannot prove residency. Read-only `capabilities` report configured token and upstream-credential counts, loopback/remote policy, account-channel capability, group access, and default/custom storage/cache/socket flags. These describe saved configuration; secret lifecycle and state-directory/privilege migration remain separate privileged workflows.

## Wire type summary

The service and console compile from the same Rust types in `src/settings.rs`. JSON uses snake case fields and rejects unknown request fields. The summary below shows the common policy fields; the additional required tuning fields are described above and defined in those shared types. It is not a complete request to paste into the API.

```text
SettingsSnapshot {
  revision: string,
  owner: OwnerSettings,
  resources: ResourceSetting[],
  upstreams: UpstreamSetting[]
}

OwnerSettings {
  unlocked_session_is_presence: boolean,
  idle_lends_after_s: integer,
  media_is_presence: boolean,
  release_after_s: integer,
  processes: string[],
  media_ignore: string[],
  sign_in_screen_is_presence: boolean,
  gpu_pressure: GpuPressureSettings
}

GpuPressureSettings {
  enabled: boolean,
  mode: "calibrated" | "fixed",
  threshold_pct: number,
  sustain_s: integer,
  ignore: string[],
  cap_pct: number,
  margin_pct: number,
  floor_pct: number,
  learn_quantile: number,
  learn_half_life_h: number,
  raise_pct_per_h: number,
  warmup_samples: integer,
  poll_ms: integer
}

ResourceSetting {
  id: string,
  kind: string,
  enabled: boolean
}

UpstreamSetting {
  id: string,
  kind: string,
  yield_on_veto: boolean,
  on_owner_return: "cancel" | "drain" | "finish",
  drain_timeout_s: integer,
  owner_scope: "host" | "resource",
  management: "deny" | "allow"
}

SettingsUpdate {
  revision: string,
  owner: OwnerSettings,
  resources: ResourceSetting[],
  upstreams: UpstreamSetting[]
}
```

`revision` is the lowercase SHA-256 of the exact source configuration bytes. The update must name each currently configured resource and upstream exactly once with the same `id` and `kind`. Missing, duplicate or unknown entries are rejected. This makes a stale screen fail instead of overwriting a concurrent manual edit.

## Validation

- `idle_lends_after_s` is from 0 through 604800.
- `release_after_s`, `sustain_s`, and `drain_timeout_s` are from 0 through 86400.
- Each list has at most 64 entries. Each trimmed entry is nonempty and at most 128 Unicode scalar values. Stored text is the trimmed value.
- Fixed pressure mode requires `0 < threshold_pct <= 100`.
- Calibrated mode requires `threshold_pct = 0`. `adaptive` is not accepted.
- In calibrated mode, `0 < floor_pct <= cap_pct <= 90`; in fixed mode, `0 < floor_pct < 100`. `0 < learn_quantile < 1`; `learn_half_life_h` is greater than 0 and at most 720; `raise_pct_per_h` is greater than 0 and at most 100.
- A save writes `learn_quantile`, `learn_half_life_h` and `raise_pct_per_h` (new keys go before `warmup_samples`, each under a one-line comment), and removes the deprecated `baseline_quantile` and `baseline_window_s` from `[owner.gpu_pressure]`. A comment above a removed key moves to the key that followed it.
- Every boolean, including `sign_in_screen_is_presence`, is required and must be a JSON boolean.
- Both upstream permissions are required strings with exactly the enum values shown above. Missing values, wrong types, case changes, and surrounding whitespace are rejected.
- The staged result must pass the full existing configuration validation and credential resolution before it can replace the source.

Validation errors return HTTP 400. A stale revision returns HTTP 409. A read or persistence failure returns HTTP 500. Over the admin pipe the same outcomes are `refused` answers with reason `validation`, `stale_revision`, `persistence`, `config_invalid` or `credential_unresolved`, and `vacancyd admin-request` exits with codes 22, 23, 28, 26 and 27. When the save went through the approval prompt, the console reads the settings again afterwards to show the saved revision. Error JSON is `{ "error": { "type": string, "message": string } }`. Messages are bounded and contain no source snippets, raw TOML, credentials, or paths. `type` is one of `validation`, `stale_revision`, `persistence`, `config_invalid` (the request passed field validation, but the staged file failed whole-configuration validation, for example because two routes sharing a server selected conflicting owner scopes; nothing was saved) or `credential_unresolved` (the resulting config parses, but an auth token or upstream credential it names could not be resolved). The service logs the specific cause of a `config_invalid` or `credential_unresolved` failure; the response never does.

## Persistence and response meaning

The service reads at most one MiB from a regular configuration file, checks the request revision, patches only the fields named above while preserving all other TOML content, writes a sibling staging file, applies the protected Windows file ACL, loads and validates the staged file, resolves its credentials, rechecks the source revision, and then replaces the source. Any failure before replacement leaves the source bytes unchanged.

The staging file is synced to storage before the replacement. On Linux the directory is synced after the rename; on Windows the rename is a write-through move. Only then is the save acknowledged. If the replacement happened but could not be made durable, the answer is still `persistence`: the file on disk may already hold the new settings, and a settings refresh shows which revision is current.

A successful POST returns the saved `SettingsSnapshot` plus `restart_required`, matching `application.restart_required`. Capacity tuning has been published for subsequent admissions; the flag identifies pending restart-only changes. The console names both outcomes and exposes no restart control. It does not replay a timed-out save automatically.

## Compatibility

The current service and console are packaged together. Unknown request fields fail closed. Unknown response fields may be ignored by the renderer. Removing or changing a field or enum requires a new contract revision and matching consumer update. Credential contents and installation paths remain outside this boundary.
