# Authentication

vacancyd uses shared bearer tokens with scopes. Configure separate tokens for borrowers, Prometheus scrapes and operators.

## What is actually at risk

Tokens protect access to these resources:

| Asset | Reached through | Consequence |
| ----- | --------------- | ----------- |
| **The hardware** | the gated proxy ports | a free GPU for anyone who can reach the port |
| **The switches** | `POST /admin/*` | lending silently stops; nothing breaks loudly, the cluster just loses capacity |
| **The owner's presence** | `GET /status`, `GET /metrics` | a log of when a specific human is at their desk, whether the screen is locked, and whether they are watching something |

Status reports when the owner is at their desk, so an unauthenticated LAN listener exposes that person's presence even to clients that never borrow the GPU. The `read` scope protects those observations.

## Scopes

| Scope | Covers |
| ----- | ------ |
| `borrow` | the gated proxy listeners: inference and each upstream's listed discovery, health and result reads, plus model and state management, the owner's data and unlisted reads where an upstream sets `management = "allow"` |
| `read` | `/status`, `/metrics` |
| `admin` | `/admin/*`, the lending switches |

`GET /healthz` and `GET /readyz` are never authenticated. They report only a liveness/readiness bit, and service supervisors need them without credentials. Detailed state and the reasons an input is not ready remain behind `read`.

## Defaults

The default policy trusts direct loopback clients and requires credentials for remote listeners.

**Loopback is trusted for reading, and on Windows and Linux for pausing.** A direct local client needs no token to read `/status`, `/metrics` or `GET /admin/settings`, so the tray app and `vacancyd status` work with no setup. What a loopback client may *change* depends on the platform:

- **Windows and Linux.** A TCP connection does not show which account opened it. The service writes a configuration file that only administrators may edit (LocalSystem and Administrators on Windows, root on Linux), and a standard user cannot stop it. So a loopback client without a token may pause lending and nothing else. Resuming, switching a resource, releasing held work and saving settings need an account the service can verify, over its admin pipe on Windows or its admin socket on Linux, or an `admin` token. See [Local administration on Windows](#local-administration-on-windows) and [Local administration on Linux](#local-administration-on-linux).
- **macOS.** Loopback trust still grants every admin action in this release. Any local process that can connect to the API port can change lending and, through `POST /admin/settings`, rewrite the saved configuration as the service's user. If the machine has untrusted local users, configure tokens and set `trust_loopback = false`; validation rejects disabling that trust without a credential to use instead.

A token presented on loopback is honoured when it carries the scope, so an `admin` token works from the machine itself as it does from the network.

Loopback trust is meant for the owner's own tools, which connect straight to the port and address it as `127.0.0.1`, `[::1]` or `localhost`. A request from a loopback address is judged like a remote one, needing a token, when:

- it carries a proxy forwarding header (`Forwarded`, `X-Forwarded-For`, `X-Forwarded-Host`, `X-Forwarded-Proto`, `X-Real-IP` or `Via`);
- its `Host` header is anything other than a loopback address or `localhost` with the port the request arrived on; or
- a browser marks it as sent by another site: `Sec-Fetch-Site: cross-site`, or an `Origin` header that is not local. A local origin is `http` or `https` on a loopback address or `localhost`, on any port, or a Tauri desktop app's own origin (`tauri://localhost`, `http://tauri.localhost`). `Origin: null` is not local.

The `Host` rule stops DNS rebinding, where a web page makes its own host name resolve to 127.0.0.1 so that the browser lets its script read the daemon's responses. The browser still sends the page's host name, so the request is refused. The last rule stops a web page on another site from submitting a plain form to `http://127.0.0.1:<proxy port>/`, which a browser sends with a loopback `Host` and without asking first. Ollama checks `Origin` itself, but llama.cpp and ComfyUI may not, so vacancyd refuses such a request for every upstream. Native clients, including `vacancyd status` and the owner console's service client, send neither header. Without tokens configured such a request is refused outright (`403`), because nothing could authorise it.

**A reverse proxy, port forward or tunnel on the same machine needs `trust_loopback = false`.** Everything it forwards arrives from loopback. A proxy that adds forwarding headers is caught by the rule above, but a raw TCP forward (`netsh interface portproxy`, `ssh -L`, many stream proxies) adds nothing and can pass on any `Host` its client chooses, so vacancyd cannot tell its clients from a local process. Turn loopback trust off and give each client a token before putting anything in front of vacancyd, including a proxy that only adds TLS. A local client that addresses vacancyd by another name, such as `host.docker.internal`, also needs a token.

**Remote listeners require credentials.** With no tokens configured, `check-config` rejects any API or proxy listener outside `127.0.0.0/8` or `::1`. The shipped example therefore listens only on loopback. Configure a token before changing a listener to a LAN address.

`allow_unauthenticated_remote = true` is an explicit escape hatch for a deployment behind another access-control boundary. It exposes the hardware and owner-presence data to every client that can reach the ports. Even in that mode, anonymous remote `admin` requests remain forbidden as defense in depth.

**Configure one token and the posture flips to closed.** Every remote request then needs a token carrying the right scope. "If you configured auth, it is enforced" is the least surprising rule available.

## Local administration on Windows

On Windows the service decides local admin requests from the caller's Windows account, which it reads from the kernel rather than from anything the caller sends:

| Action | Effect | Who may |
| ------ | ------ | ------- |
| Read status, settings and the diagnostics export | shows owner presence and policy | any local user |
| Pause lending | only reduces lending | any signed-in user |
| Resume lending | can increase lending | the account that paused, or an administrator |
| Switch a resource on or off, release held work | can increase lending | an administrator |
| Save settings | rewrites the Administrators-only configuration | an administrator |
| Open or end a debug logging window | changes what the service writes to its log | an administrator |

"An administrator" means an elevated member of the local Administrators group, LocalSystem, or a request carrying an `admin`-scoped token. Under User Account Control an administrator's everyday, unelevated programs do not count; the action needs an elevated one.

The account comes from the service's admin named pipe, `\\.\pipe\vacancyd-admin`. Any signed-in (interactive) user can connect to it. The client writes one request; the service reads the Windows token of the account that wrote it, answers, and closes the connection. The client opens the pipe at identification level, so the service can read the token but can never act as that account. Before sending anything, a client checks that the pipe belongs to the running vacancyd service, the same check the session reporter makes.

**Who may resume.** The service remembers which account paused. That account may resume its own pause without elevation; anyone else needs an administrator. Pausing lending that is already paused does not change who paused it, so a second account cannot take over a pause by pausing again. A pause made over TCP loopback or with a token names no account, so only an administrator may resume it. The record lives in memory: a service restart restores the configured defaults, with lending on.

**The owner console** sends Pause and Resume over the pipe as the signed-in account. Resuming another account's pause, switching a resource and saving settings raise the Windows approval prompt, which runs `vacancyd.exe admin-request` elevated with the staged change. The staged file is readable only by the signed-in account, Administrators and LocalSystem, and is deleted afterwards. Choosing **No** changes nothing, and the console says so. The stale-revision and validation answers are the same as the HTTP settings API's.

**Scripts** can use an `admin` token over HTTP, or send the same requests the console sends with `vacancyd admin-request --request <file>` (also available on Linux; see [Local administration on Linux](#local-administration-on-linux)). Pausing and resuming your own pause work from a standard prompt; everything else needs an elevated prompt. The file holds exactly one JSON request, one of:

```text
{"protocol_version": 1, "action": {"kind": "pause"}}
{"protocol_version": 1, "action": {"kind": "resume"}}
{"protocol_version": 1, "action": {"kind": "set_resource", "resource": "gpu0", "enabled": false}}
{"protocol_version": 1, "action": {"kind": "release_work", "id": 7, "upstream_stopped": true}}
{"protocol_version": 1, "action": {"kind": "save_settings", "settings": <settings document>}}
{"protocol_version": 1, "action": {"kind": "start_debug", "minutes": 30}}
{"protocol_version": 1, "action": {"kind": "end_debug"}}
```

`start_debug` opens a [debug logging window](diagnostics.md#a-temporary-debug-window) of 1 to 240 minutes; `end_debug` ends it early.

`save_settings` takes the complete document described in [the settings contract](console-settings.md), with the `revision` from `GET /admin/settings`. The command exits with:

| Code | Meaning |
| ---- | ------- |
| 0 | the service confirmed the change |
| 20 | an administrator is required |
| 21 | the service did not understand the request |
| 22 | the values were rejected |
| 23 | the settings changed since `revision` was read |
| 24 | no such resource |
| 25 | the service refused to release that work |
| 26, 27, 28 | the saved settings could not be reloaded, a credential in them could not be resolved, or they could not be written; see the service log |
| 40 | no confirmed answer: the service could not be reached or its answer did not match; check status before retrying |
| 41 | the request file was missing, too large or not a valid request |

**Loopback trust is narrow.** A loopback `POST` succeeds without a token only for `POST /admin/lending/disable`. Resume, resource switches, work release, `POST /admin/settings` and `POST /admin/debug/enable|disable` over loopback return `403` with error type `administrator_required`. `GET /admin/settings` and `GET /admin/diagnostics` stay readable over loopback with the intent header. Give such a script an `admin` token, or use `vacancyd admin-request` from an elevated prompt.

## Local administration on Linux

On Linux the service applies the same rules, and learns the caller's account from the kernel through a Unix domain socket:

| Action | Effect | Who may |
| ------ | ------ | ------- |
| Read status, settings and the diagnostics export | shows owner presence and policy | any local user |
| Pause lending | only reduces lending | any local account |
| Resume lending | can increase lending | the account that paused, or an administrator |
| Switch a resource on or off, release held work | can increase lending | an administrator |
| Save settings | rewrites the root-owned configuration | an administrator |
| Open or end a debug logging window | changes what the service writes to its log | an administrator |

"An administrator" means root, a member of the admin group, or a request carrying an `admin`-scoped token. The admin group is `vacancyd-admin` unless `server.admin_group` names another; an empty value makes root the only local administrator. vacancyd does not create the group:

```console
$ sudo groupadd --system vacancyd-admin
$ sudo usermod -aG vacancyd-admin alice
```

**How membership is checked.** When a client connects, the kernel records its uid, gid and supplementary groups (`SO_PEERCRED`, `SO_PEERGROUPS`), and nothing the client sends can change them. A non-root caller is an administrator when the connecting process holds the admin group *and* the group database lists the account as a member at the time of the request. The first condition works like an elevated Windows token: membership added with `usermod -aG` applies to processes started after the account next signs in (or under `newgrp vacancyd-admin`), and a process that dropped its groups is not an administrator. The second makes removing an account from the group take effect at once. The group is looked up by name on every request that needs it, so creating, renaming or deleting it needs no restart. If the user or group database cannot be read within five seconds, the caller is treated as a standard account.

**Who counts as signed in.** On Linux any local account that can connect to the socket may pause, whether or not it has a desktop or logind session. Pausing only reduces lending, and TCP loopback already allows it, so requiring a session would add no protection. Who may resume works as on Windows: the service remembers the uid that paused, that account may resume its own pause, and anyone else needs an administrator. Pausing lending that is already paused does not change who paused it. A pause made over TCP loopback or with a token names no account, so only an administrator may resume it. The record lives in memory: a service restart restores the configured defaults, with lending on.

**The socket.** The default path is `/run/vacancyd/admin.sock`; `server.admin_socket` sets another absolute path. The packaged systemd unit creates `/run/vacancyd` (`RuntimeDirectory=vacancyd`, mode 0755), removes it when the service stops, and lets the service write `/etc/vacancyd` so that saving settings works under its sandbox. The socket file is mode 0666 so that any local account can connect; connecting grants nothing by itself. The service refuses to use a directory that is a symbolic link, is owned by another account, or is writable by group or others, and creates only the last path component when it is missing. It replaces an existing socket only when nothing is listening on it, never a symbolic link or other file, and removes the socket on shutdown only if it is still the one it created. When the socket cannot be created, the service logs why and retries with backoff; reading and pausing over loopback keep working meanwhile. `/status` reports `admin_channel.state` as `unavailable` for as long as it is down, and `vacancyd status` prints a line saying so. The packaged unit allows writes under `/etc/vacancyd`, `/var/lib/vacancyd` and `/run/vacancyd`, plus its private `/tmp` and `/var/tmp` under `PrivateTmp=true`. A socket outside all those writable paths also needs its directory in `ReadWritePaths=`; `vacancyd check-config` warns when the installed unit, or the packaged one before it is installed, would block it.

**Scripts** send the same requests as on Windows with `vacancyd admin-request --request <file>`, run as the account that should make the change, for example with `sudo` for settings. Add `--socket <path>` when the service uses a non-default `server.admin_socket`. The request file format and exit codes are the ones listed under [Local administration on Windows](#local-administration-on-windows). On success the command prints the service's answer as JSON, including the new settings `revision` after a save; otherwise it prints the reason to standard error. Before sending, the command checks who serves the socket, and exits with code 40 without sending anything if the server is not trusted.

```console
$ echo '{"protocol_version": 1, "action": {"kind": "pause"}}' > pause.json
$ vacancyd admin-request --request pause.json
{"outcome":"lending","lending_enabled":false}
```

`--request -` reads the request from standard input instead. When the service runs as an unprivileged account, as in the container image, the command also accepts a server that owns both the socket and its directory, provided no other account can write to that directory. In a container, run it through `docker exec`; see [the container guide](container.md#headless-the-default).

## Generating a token

```console
$ vacancyd gen-token
kPqZ2t7nR4xM8vB1sL0wYcE6hJ3uA5dG9fT-iO2rNqU
```

256 bits from the OS random source, URL-safe so it survives shells, YAML, `.env` files and HTTP headers without quoting. It is printed once and never stored. Put it straight where it belongs.

There is no rate limiting of failed attempts and no lockout. An unguessable token is doing that work instead, which is why this command exists and why short tokens draw a warning at startup.

## Configuring

```toml
[auth]
# Direct requests from 127.0.0.1 / ::1 need no token (limits under Defaults).
trust_loopback = true
# Unsafe unless another network layer enforces access.
allow_unauthenticated_remote = false

[[auth.token]]
name   = "cluster"          # appears in logs; never the secret
scopes = ["borrow"]
value_file = "C:/ProgramData/vacancyd/secrets/cluster.token"

[[auth.token]]
name   = "prometheus"
scopes = ["read"]
value_env = "VACANCYD_TOKEN_PROMETHEUS"

[[auth.token]]
name   = "operator"
scopes = ["read", "admin"]
value_file = "C:/ProgramData/vacancyd/secrets/operator.token"
```

Exactly one of `value`, `value_env` or `value_file` per token. `value_file` mirrors the common `_FILE` convention, so Docker secrets and Kubernetes secret mounts work with nothing extra. A relative `value_file` path is resolved from the directory containing the vacancyd config, as is the GPU-pressure state file.

**Inline `value` is discouraged** and exists only for quick local testing: it puts a secret in a file that gets copied, backed up, and occasionally committed. `Debug` output for the config redacts it either way. Neither the config structs nor the authenticator will print a secret into a log.

A missing environment variable or unreadable secret file is a **startup failure**, not a warning. The alternative is a daemon that looks configured, rejects the one client that mattered, and takes an afternoon to diagnose.

## Wiring up the clients

### A borrower (LiteLLM)

```yaml
model_list:
  - model_name: ws-gemma3-12b
    litellm_params:
      model: ollama/gemma3:12b
      api_base: http://192.0.2.10:11435   # the gate, not :11434
      api_key: os.environ/VACANCYD_TOKEN_CLUSTER
```

The token is sent as `Authorization: Bearer …`. vacancyd **strips that header before forwarding upstream**, so its credential never reaches Ollama or its logs. If you are fronting a service that authenticates callers itself, set `forward_authorization = true` on that upstream.

A `borrow` token uses the hardware; it does not manage the upstream or read the owner's use of it. Pulling, creating, copying, pushing or deleting Ollama models, uploading to ComfyUI, changing its settings, queue or history, reading the owner's ComfyUI history, workflows or logs, reading llama.cpp's `/slots` (the prompts it is processing), `/metrics` or `/props`, Ollama's `/api/ps` or Chatterbox's `/status`, and any `GET` or `HEAD` the adapter does not list as a borrower read are refused with `403` (`management_denied`) unless that upstream sets `management = "allow"`. The listed reads are model discovery, health and a ComfyUI client's own results; the [route tables](architecture.md#proxy-srcproxyrs) name them. Model management from Open WebUI or LiteLLM through the gate needs that opt-in; pointing those tools at the real service from the owner's machine does not. Treat `allow` as handing the borrower that upstream's administration, and use it only on an instance dedicated to borrowed work.

### An upstream that authenticates its callers

Some real services want a credential of their own: llama.cpp started with `--api-key`, or a service behind HTTP Basic authentication. Give that upstream a `credential`, and vacancyd presents it on every request it makes there: forwarded borrower requests, WebSocket upgrades, and its own model observation, completion tracking, cancellation and unload calls.

```toml
[[upstream]]
id     = "llamacpp"
listen = "127.0.0.1:8001"
target = "http://127.0.0.1:8000"
kind   = "openai"
resource = "gpu0"

[upstream.credential]
scheme     = "bearer"       # Authorization: Bearer <secret>
value_file = "C:/ProgramData/vacancyd/secrets/llamacpp.key"
```

For HTTP Basic, set `scheme = "basic"` and `username`; the secret is then the password. The secret comes from `value_file` or `value_env`, exactly one, with the same rules as a token: a relative file is resolved from the config's directory, trailing whitespace is trimmed, and a missing or empty secret is a startup failure that `vacancyd check-config` reports first. There is no inline form. A bearer secret must be printable ASCII without spaces.

Borrowers never learn it. Their own `Authorization` header is not forwarded to that upstream, and `forward_authorization = true` together with a `credential` is refused, since both would set the same header. The secret is held as a sensitive header value: it does not appear in logs, `/status`, error responses or `Debug` output.

**Credentials in the target URL are refused.** A target such as `http://user:password@127.0.0.1:8000` fails validation with a pointer to `credential`, as does a query string, because a URL is shown in logs, `/status`, the console and `vacancyd status`. Every one of those shows the URL with any credentials and query replaced by `***` in any case.

### Prometheus

```yaml
scrape_configs:
  - job_name: vacancyd
    static_configs:
      - targets: ["192.0.2.10:9811"]
    authorization:
      type: Bearer
      credentials_file: /etc/prometheus/secrets/vacancyd.token
```

### An operator

```console
$ curl -H "Authorization: Bearer $(cat operator.token)" \
       -H "X-Vacancyd-Admin: 1" \
       -X POST http://192.0.2.10:9811/admin/lending/disable
```

From the machine itself no token is needed to pause when loopback is trusted, but the explicit admin-intent header is still required:

```console
$ curl -H "X-Vacancyd-Admin: 1" \
       -X POST http://127.0.0.1:9811/admin/lending/disable
```

On Windows and Linux that is the only change loopback trust allows; see [Local administration on Windows](#local-administration-on-windows) and [Local administration on Linux](#local-administration-on-linux). On macOS it allows every admin route.

The header is not a secret or a replacement for an `admin` token. It makes the request non-simple in a browser, so a cross-origin page cannot send it without a successful CORS preflight. vacancyd grants no cross-origin access and serves no preflight route. This prevents a blind form or fetch from changing local lending state. Which native local client may do what is decided separately, as described under *Defaults*. Administrative requests carrying an `Origin` header are also refused. Reads of `/status`, `/metrics` and `GET /admin/settings` from a DNS-rebinding page carry no `Origin`; the `Host` rule under *Defaults* refuses those. A desktop webview must ask its native host to perform administration; it must not call these routes directly from renderer JavaScript.

## Responses

| Situation | Status | Notes |
| --------- | ------ | ----- |
| No credential presented | `401` | with `WWW-Authenticate: Bearer` |
| Credential not recognised | `401` | with `WWW-Authenticate: Bearer` |
| Recognised, wrong scope | `403` | retrying will not help |
| Missing/wrong admin-intent header | `403` | add `X-Vacancyd-Admin: 1` in a native client |
| Browser `Origin` on an admin request | `403` | browser-originated mutation is unsupported |
| Proxied, non-loopback `Host` or cross-site request from loopback, no tokens configured | `403` | configure a token; see *Defaults* |
| Windows or Linux loopback admin request other than pause, no `admin` token | `403` | error type `administrator_required`; see [Local administration on Windows](#local-administration-on-windows) and [on Linux](#local-administration-on-linux) |

A request that passes authentication can still be refused by the proxy's route policy, before anything is forwarded:

| Situation | Status | `error.type` |
| --------- | ------ | ------------ |
| Management route, or a `GET`/`HEAD` that is not a listed borrower read, on an upstream without `management = "allow"` | `403` | `management_denied` |
| Any other method on a route vacancyd does not recognize, including `OPTIONS` | `501` | `unrecognized_route` |
| A ComfyUI `/prompt` body that is not a JSON object | `400` | `invalid_submission` |
| A `/v1/responses` body that asks for background mode, or is not a JSON object | `400` | `unsupported_request` |
| A request body over `max_request_body_mib` | `413` | `request_body_too_large` |
| A request body that would take the proxy past `max_buffered_body_mib` | `503`, with `Retry-After` | `request_body_budget` |
| A request body that stops arriving for `body_idle_timeout_s`, or is not complete within `body_read_timeout_s` of its headers | `408` | `request_body_timeout` |

They are counted in `vacancyd_requests_total{upstream,outcome}` as `refused_management`, `refused_route`, `refused_request`, `refused_body_too_large`, `refused_body_budget` and `refused_body_timeout`.

The distinction is deliberate: `401` means *retry with a credential*, `403` means *stop asking*. A denial names the scope required and nothing else. It never reveals which credentials exist.

Every decision increments `vacancyd_auth_total{scope,outcome}`, where `outcome` is `allowed`, `missing`, `invalid`, `forbidden`, `not_direct`, `missing_intent`, `browser_origin` or `administrator_required`. An allowed borrowing request is counted after the borrow scope succeeds and before adapter classification. A rising `invalid` or browser-related count from an unexpected peer is worth an alert.

## Deliberate limits

Worth stating plainly rather than leaving to be discovered:

- **Tokens are compared in constant time**, but stored in memory as given. There is no hashing at rest; protect the config and secret files with file permissions.
- **No TLS.** Tokens cross the network in the clear, so this is appropriate for a trusted LAN or a tunnel, not the open internet. Front it with a reverse proxy that terminates TLS if you need more, and set `trust_loopback = false` when that proxy runs on the same machine.
- **No rate limiting** of failed attempts; see the note under *Generating a token*. `[server.limits]` caps connections per listener, how long a client may take over a request body or leave a response unread, and how much body data a proxy holds, which is not the same thing. A client with enough connections can still occupy a listener for up to those timeouts, so keep `borrow` tokens with clients you trust not to.
- **Shared tokens, not users.** There is no per-user identity, no rotation workflow and no revocation beyond editing the config and restarting.

These are the right weight for a daemon whose blast radius is one workstation's spare cycles. They would not be for anything larger, and none of them should be quietly outgrown.
