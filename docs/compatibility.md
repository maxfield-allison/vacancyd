# Compatibility for v1

This is the v1.0.0 compatibility contract. It defines the promises that later 1.x releases must preserve.

## Configuration

The documented TOML settings in `vacancyd.example.toml` are the configuration interface. A later 1.x release must continue to accept a valid v1.0.0 configuration with the same documented meaning. Removing a setting, changing its type or unit, or changing a default in a way that breaks existing behavior requires a major release and migration instructions. New optional settings may be added with defaults that preserve existing behavior.

This promise runs forward. An older binary can reject settings introduced by a newer release: the parser deliberately rejects unknown fields rather than silently ignoring an operator's intended policy. Run `vacancyd check-config` with the target binary before an upgrade or downgrade. The configuration has no separate schema-version field; the application version and documented settings identify its compatibility boundary.

MSI upgrade, rollback and uninstall preserve the existing configuration and credentials. Guided setup refuses to overwrite a customized configuration. Preserving bytes does not make a newer configuration compatible with an older binary. Retain a protected copy of both the configuration and any referenced credential files when preparing a downgrade. See [setup](setup.md) and the [Windows lifecycle instructions](agent-startup.md).

## Status clients

`GET /status` has an independent `schema_version`, currently `1`. The `daemon.version` field identifies the running package; clients must not use it as a substitute for checking the schema. Within schema 1, fields may be added. Removing a field, changing its type or meaning, or introducing an enum value that existing clients cannot decode requires a new schema version.

Clients tolerate additive fields and reject unsupported schemas. An unavailable, malformed or incompatible response is unknown, not evidence of vacancy. Stale or unknown observations cannot be displayed as current measurements. Resource lendability, upstream admission, owner veto and service readiness remain separate facts. The shared decoder and synthetic fixtures implement these boundaries; see the [console contract](console.md).

Human-readable CLI output, logs and explanatory error text are diagnostic interfaces. Integrations should use the typed status fields rather than parse those strings. Status also includes held work and fresh model observations. The `resource_busy` refusal means another job or a scheduled yield owns the resource. Recovery records survive a daemon restart and must be retained during rollback; see [admission](admission.md). Status also includes the optional `presence.basis`, a one-line diagnostic explanation of the presence reading, and `presence.sessions`, the sessions a session-enumerating backend (systemd-logind) read with whether each counted. Both are diagnostic and may be absent. While `presence.media` is `unknown`, the optional `presence.media_error` says why media detection could not be read; it is diagnostic text and absent otherwise. Status also includes the optional `config_revision`, the revision of the config the running process actually loaded at startup. It never changes until an actual restart, even after a settings save replaces the file on disk. Compare it with the settings snapshot's revision and `application.restart_required` to identify unapplied restart policy. The separate `application.capacity_revision` identifies published model/default budgets; it never claims that restart-only settings are active. See [the settings contract](console-settings.md). Status also includes the optional `logging` object: the log level in force, what set it, and while a debug window is open its end as `debug_until`. The optional `admin_channel.state` is `listening`, `unavailable` while the local admin channel (the Windows admin pipe or the Linux admin socket) could not be started, or `not_supported` on a platform without one; the service retries an unavailable channel and logs why. Each upstream's optional `unseen_gpu_work.resources` lists the tracked GPUs whose pressure may be counting that upstream's borrowed work as owner load, because the work ran while no process serving its target used a GPU. The field appears when vacancyd detects this and disappears once a process serving the target is seen using a GPU; see [the architecture](architecture.md). The diagnostics export from `GET /admin/diagnostics` has its own `schema_version`, currently `1`, under the same additive rule. See [logging and diagnostics](diagnostics.md).

Each upstream reports `owner_scope`, `host` or `resource`, and `owner_veto.exempt_upstreams` lists the upstreams whose scope is `resource`, whether or not the veto is active. A status without these fields comes from a daemon where every upstream is host-scoped, and reads that way. While `owner_veto.active` is true, host-scoped upstreams are refused, and an exempt upstream may still report `vacancy`; a client must not present an active veto as "nothing is lending" while `exempt_upstreams` is non-empty. The shared decoder rejects a response whose exempt list disagrees with the upstream scopes, or where an upstream that is not `resource`-scoped reports a vacancy under an active veto. It treats any other scope value as host-scoped. See [resource-scoped upstreams](admission.md#resource-scoped-upstreams).

The console does not own configuration migration or admission decisions. Its bounded runtime switches can pause/resume new lending globally or per resource; the service remains authoritative, existing work follows reclaim policy, and a service restart restores configured defaults. See [console controls](console-controls.md).

## Proxy routes

Each adapter's route table in [architecture](architecture.md#proxy-srcproxyrs) is part of the contract, including its list of borrower reads. A later 1.x release may recognize more upstream routes, turning a `501 unrecognized_route` or an unlisted-read `403 management_denied` refusal into a forwarded request. It does not stop recognizing a route, and does not move a route out of work or pass-through, except to close a security problem, which the changelog states under Security. Refusals keep their status codes and `error.type` values: `503 no_vacancy`, `403 management_denied`, `501 unrecognized_route`, `400 invalid_submission`, `400 unsupported_request`, `408 request_body_timeout`, `413 request_body_too_large` and `503 request_body_budget`.

An upstream's `management` setting defaults to `deny`. Model-store and ComfyUI state routes, reads of the owner's data and any `GET` or `HEAD` an adapter does not list as a borrower read do not pass through to a borrower by default, and any other unrecognized request is refused rather than admitted and held. A configuration that wants borrowers to manage an upstream through the gate, or to reach its unlisted reads, needs `management = "allow"` on that upstream.

## ComfyUI releases

The `comfyui` adapter admits, tracks and unloads work on any ComfyUI release that serves `/prompt` (`POST` and `GET`), `/queue`, `/history/<id>`, `/system_stats` and `/free`. Owner-return cancellation depends on the release, because ComfyUI's interrupt changed:

- 0.26.0 and later stop a running borrowed prompt with the job cancel route, which cannot reach another prompt.
- 0.3.57 to 0.25.x use the prompt-scoped `/interrupt`, which ComfyUI checks outside its queue lock.
- Earlier releases, and releases that do not report a version, are never sent an interrupt, because theirs stops whatever is running. A running borrowed prompt then finishes and keeps its reservation until it does; pending ones are still deleted.

ComfyUI 0.3.45 and later queue a borrowed prompt under the ID vacancyd assigns. Earlier releases use their own ID, which vacancyd learns from the submission reply and records for recovery; if that reply is lost, the reservation is held for an operator. See [admission](admission.md#owner-return) for the details.

ComfyUI 0.6.0 and later answer vacancyd's questions about one prompt through `GET /api/jobs/<id>`, whatever the size of the queue. On earlier releases vacancyd reads `/queue` instead, which makes ComfyUI serialize every queued workflow; with a long queue that slows ComfyUI for its owner, so prefer 0.6.0 or later.

On every release, a prompt whose ID the submission reply named is also released once it is neither queued, running nor in history on two consecutive checks, for example after the owner deletes it or clears the queue. See [admission](admission.md#knowing-when-work-finishes).

## Windows session reporters and stored observations

The session reporter protocol is separate from the status schema. The service currently requires both protocol version 1 and an exact reporter package-version match. Install the daemon, reporter and console from the same package. An older reporter is rejected conservatively even when its status schema would still be compatible. Complete the MSI-required reboot and verify fresh reports after upgrading; replacing files does not replace a process already running in another user's session.

Calibration caches are recoverable observations, not portable configuration. Restoration checks the supported cache schema, GPU UUID and case-insensitive ignore-name set; a new PCI address for the same UUID keeps the calibration. Current learning and threshold settings apply to the restored histogram without discarding it. Supported older schemas migrate as described in [calibration](calibration.md#upgrading-from-a-frozen-baseline); unusable entries start from new readings. A calibration cache is never evidence of current free capacity.
