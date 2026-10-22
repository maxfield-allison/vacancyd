# Running vacancyd as a Windows service

vacancyd implements the Windows Service Control Manager (SCM) lifecycle directly. No service wrapper is required. The MSI installs this service automatically; the manual procedure below is for development builds.

**Elevation required.** Every step below creates or changes a service and must run from an elevated prompt.

## Procedure

Build a release binary and create separate program and mutable-data directories:

```powershell
cargo build --release --features windows-session
$program = "$env:ProgramFiles\vacancyd"
$data = "$env:ProgramData\vacancyd"
New-Item -ItemType Directory -Force $program, $data
Copy-Item target\release\vacancyd.exe "$program\vacancyd.exe"
Copy-Item target\release\vacancyd-session.exe "$program\vacancyd-session.exe"
Copy-Item vacancyd.toml "$data\vacancyd.toml"
```

Review the copied configuration, then restrict it and its directory to Administrators and LocalSystem:

```powershell
& "$env:ProgramFiles\vacancyd\vacancyd.exe" protect-config `
  --config "$env:ProgramData\vacancyd\vacancyd.toml"
```

Validate the config as the file the service will read:

```powershell
& "$env:ProgramFiles\vacancyd\vacancyd.exe" check-config `
  --config "$env:ProgramData\vacancyd\vacancyd.toml"
```

Expected: `C:\ProgramData\vacancyd\vacancyd.toml OK (...)`

Install the native service. The quoted executable and data paths are part of the stored SCM command line, so paths containing spaces remain safe:

```powershell
$exe = "$env:ProgramFiles\vacancyd\vacancyd.exe"
$config = "$env:ProgramData\vacancyd\vacancyd.toml"
$log = "$env:ProgramData\vacancyd\vacancyd.log"
$command = ('"{0}" service --config "{1}" --log-file "{2}"' -f $exe, $config, $log)
New-Service -Name vacancyd -BinaryPathName $command `
  -DisplayName "vacancyd" `
  -Description "Gate borrowed AI requests on owner policy and available resources" `
  -StartupType Automatic
sc.exe failure vacancyd reset= 86400 actions= restart/5000/restart/5000/""/0
sc.exe failureflag vacancyd 1
Start-Service vacancyd
```

The binary reports `StartPending`, `Running`, `StopPending`, and `Stopped` to SCM. Stop and preshutdown controls use the same shutdown path as Ctrl-C. New borrowed work is refused at once. Requests already being forwarded get time to finish: none for upstreams set to `cancel`, up to `drain_timeout_s` for `drain`, and at most 10 seconds in any case. Whatever is still in flight after that is cut off. `Stopped` is reported only after the process has finished its background work; a probe already running gets up to 9 seconds, longer than any probe's own deadline. The stop-pending wait hint (25 seconds) covers both. Unexpected collector or listener exit stops the service with a service-specific error, allowing the recovery policy above to restart it.

A listener configured on a LAN address that the machine does not have yet, for example while DHCP or Wi-Fi is still connecting after boot, does not stop the service. vacancyd keeps retrying the bind, starting at one second and backing off to every 30 seconds, and logs a warning for each attempt. `/readyz` reports not ready until every proxy listener is bound. An address already in use by another program is retried for about a minute and then stops the service as before.

## Automatic recovery

Major MSI upgrades and uninstall request a reboot to retire reporters in other user sessions. Use `/norestart` for unattended operations and treat exit code `3010` as requiring that reboot. Normal service recovery below is independent of this installer lifecycle.

Fresh MSI installation sets the same bounded policy: restart after five seconds on each of the first two failures, then leave the service stopped. A day without failures resets the count. Normal operator stops do not request recovery. Both an unexpected process exit and a reported service error count as failures.

After installation or upgrade, inspect both settings:

```powershell
sc.exe qfailure vacancyd
sc.exe qfailureflag vacancyd
```

Expect two `RESTART` lines with 5000 ms delays and a reset period of 86400 seconds. `qfailure` can omit the final no-action entry; the third-failure test proves that retries stop. `qfailureflag` reports `TRUE` for the flag configured with value `1`. The action array uses WiX's [Utility ServiceConfig](https://docs.firegiant.com/wix/schema/util/serviceconfig/). The reported-error flag uses `sc.exe failureflag` through a checked, elevated [WiX deferred action](https://docs.firegiant.com/wix/tools/wixext/quietexec/), after service creation and before first start. This avoids the native MSI service-config table's WIX1149 warning. It runs only on fresh install. Upgrades and repair preserve both existing recovery settings, including administrator changes. Database inspection alone does not prove recovery.

Major upgrades install shared components before removing the old product, inside the same transaction. The service keeps its registration and recovery policy through both successful upgrade and rollback. Package authors must preserve component GUIDs and key paths under Windows Installer's component rules; changes to component identity require a reviewed migration. Qualification must force failures before and after old-product removal and compare the original service policy, including customized settings, after rollback.

A package with the same version as the installed one, such as a signed build of an unsigned candidate, is also a major upgrade: it replaces the installed product instead of installing beside it, including the owner console, whose file version does not change between rebuilds. Reinstalling the identical package is maintenance, not an upgrade.

All three Windows executables carry the package version in their Windows file metadata. The MSI checks those versions when it is built. Without them, Windows Installer can preserve an old daemon or session agent as modified user data after a failed upgrade rolls back, leaving different versions in the same installation. Configuration under ProgramData follows its separate preservation rules.

If retries are exhausted, inspect the service log and validate the configuration with `check-config` before starting the service again. Fix the reported cause; repeatedly starting an invalid configuration does not repair it. Lending remains unavailable while the service is stopped. Uninstall removes the service and its recovery settings while preserving ProgramData.

## Service log retention

Service mode writes `vacancyd.log` under the retained ProgramData directory. The log must be inside `ProgramData\vacancyd`, and that protected root must already exist. A log path anywhere else stops startup with an error that names it. The service checks the log directory whether or not it exists. It creates a missing one, and any missing parents, protected for Administrators and LocalSystem from creation. It does not repair an existing directory: a junction or other reparse point on the way, or a folder from `ProgramData\vacancyd` down that gives access to another account, stops startup. The active file and three archives (`.1` newest through `.3` oldest) are each capped at 2 MiB, for an 8 MiB total. Writes larger than one file are split at the limit; rotation can split a line or UTF-8 character between files. Foreground `vacancyd run` writes logs to standard output; other commands write logs to standard error.

Restart and upgrade reopen the same log set. When upgrading from an unbounded logger, startup retains only the newest 2 MiB of each owned file. Uninstall preserves these files alongside the configuration for diagnosis and deliberate removal by an administrator. The service owns rotation; the MSI's permanent data component retains the directory. Stop the service before manually replacing log files.

Rotation renames the live file to `.1`. A reader that holds the log without delete sharing, as many tail tools do, blocks that rename on Windows; rotation then copies the live file to `.1` and empties it instead, so output continues. A write or rotation failure stops that write rather than exceeding the bound. The service attempts later writes again and reports one error per failure episode directly to stderr and the Windows Application event log (`vacancyd`, event ID 1). The event's insertion string carries the error; there is no installed message catalogue, so Event Viewer may show its missing-description notice. This uses [Windows event reporting](https://learn.microsoft.com/en-us/windows/win32/api/winbase/nf-winbase-registereventsourcew), without calling the file logger recursively. If the event service is also unavailable, the reporting attempt is best-effort.

The automated tests use small file limits to exercise continuous output, restart, migration from an oversized file, rotation failure and recovery. Installer lifecycle evidence must still verify retention on the exact Windows candidate.

## Session 0: what a service can and cannot see

A service runs in **session 0**, which has no interactive desktop. That costs one signal and keeps the one that matters:

| Signal | From a service | Notes |
| ------ | -------------- | ----- |
| **Session state** (locked / disconnected / logged-out / active) | ✅ | Terminal Services API. This carries the whole owner check. The sign-in screen counts as logged out; see below. |
| Foreground process name | ❌ returns `null` | No desktop to query. |

Session state remains available from session 0. Foreground application and idle time require the user-session agent (included in MSI startup) described below:

### The sign-in screen

A physical Windows PC always has a console session. After a restart that nobody has signed in after, after a sign-out, and on the Switch user screen, that session is `WTSConnected` with no user name. vacancyd treats it as the sign-in screen: nobody is at a desktop, so it lends like a logged-out machine (`session: logged_out`). Because a single read can catch a transition during sign-in, sign-out or user switching, the same session view must be read twice in a row before it lends; until then `/status` reports `session: unknown` with a `basis` saying the view is not yet confirmed. When another user's session is still signed in, that session decides: unlocked refuses, and locked or disconnected lends (reported as `locked` or `disconnected`). A failed session query, a user-less session in any other state, or an inconsistent view reports `session: unknown` and refuses, with a cause naming the Windows session inventory.

Lending does not start the moment the owner leaves. The owner veto holds for `owner.release_after_s` (120 seconds by default) after the last read that counted as presence, so locking, signing out or switching user lends once that hold has passed.

To keep the sign-in screen refusing, set this and restart the service, or use **Keep refusing while Windows shows the sign-in screen** under Advanced settings in the owner console:

```toml
[owner]
sign_in_screen_is_presence = true
```

With it on, any session at the sign-in screen refuses as `session: active` with the cause `sign-in screen shown and owner.sign_in_screen_is_presence keeps it refusing`, treating the sign-in screen as presence rather than vacancy.

```json
{"presence_backend": "windows-wts",
 "presence": {"session": "active", "foreground": null, "owner_app": null},
 "owner_veto": {"active": true, "cause": "session unlocked"}}
```

## Verification

```powershell
Get-Service vacancyd
& "$env:ProgramFiles\vacancyd\vacancyd.exe" status
Invoke-RestMethod http://127.0.0.1:9811/healthz
Invoke-RestMethod http://127.0.0.1:9811/readyz
Get-Content "$env:ProgramData\vacancyd\vacancyd.log" -Tail 50
& "$env:ProgramFiles\vacancyd\vacancyd.exe" diagnostics export --out "$env:USERPROFILE\vacancyd-diagnostics.json"
```

To see more in the service log for a while, open a debug window from the owner console or with `vacancyd admin-request`; see [logging and diagnostics](diagnostics.md).

## Running from a prompt, and startup errors

The installed service runs as LocalSystem. `vacancyd run` in a terminal is for development and diagnosis, and on Windows it needs an **elevated** prompt (Run as administrator): the installed configuration and state directories are private to Administrators and LocalSystem. An administrator's everyday, unelevated prompt cannot access them. Stop the installed service first (`Stop-Service vacancyd`), or give the run its own configuration file in a folder inside `ProgramData\vacancyd`, and its own listen ports, leaving `server.admission_state_dir` empty so that its admission records are kept beside that file; two daemons cannot share one set of admission records.

`run` checks the process token before touching anything and refuses an unelevated start. The same startup failures in other commands (`check-config`, `setup`, `migrate-config`, `protect-config`, `healthcheck`) and in `vacancyd service` are reported in one sentence that names the fix, with the Windows error kept beneath it, and a distinct exit code:

| Code | Meaning | What to do |
| ---- | ------- | ---------- |
| 50 | The step needs an elevated administrator or the installed service, and this process is not elevated. Windows reported access denied (5), an owner it would not assign (1307) or a missing privilege (1314). | Start the service (`Start-Service vacancyd`), or run the command from an elevated prompt. |
| 51 | Another process holds the admission records (sharing violation, 32): usually the installed service. | Stop that instance, or use a different `server.admission_state_dir` inside `ProgramData\vacancyd`, and different ports. |
| 52 | `vacancyd service` was run from a prompt; only the service manager can start it (1063). | `Start-Service vacancyd`, or `vacancyd run` in the terminal. |
| 1 | Any other failure. The message says what failed. | |

For example, from an unelevated prompt:

```text
PS> vacancyd run --config C:\ProgramData\vacancyd\vacancyd.toml
Error: running the vacancyd daemon needs an elevated administrator or the installed vacancyd service, and this process is not elevated. Start the service instead (Start-Service vacancyd), or run this command from an elevated prompt (Run as administrator)
PS> $LASTEXITCODE
50
```

An elevated administrator who is still refused access has a real permissions problem rather than a missing elevation, so that error is shown as Windows reported it, with exit code 1. These codes do not overlap the `admin-request` codes in [authentication](authentication.md).

The session-agent and local admin channels are optional and retried. If their named pipe already belongs to another vacancyd, the service log says so and the daemon keeps running without that channel.

## Firewall

If the Windows Firewall is enabled, the gate ports (e.g. `11435`, `8001`) and the API port (`9811`) need inbound rules scoped to the borrower's addresses. The real service ports (`11434`, `8000`) can then be *closed* to the LAN. Only the gate should be reachable.

## Rollback

```powershell
Stop-Service vacancyd
sc.exe delete vacancyd
```

This removes the service registration but deliberately preserves `%ProgramData%\vacancyd`, including configuration, token files, learned baselines, and logs. Delete that directory only when its data is no longer wanted.

## Who can change lending on this PC

The service runs as LocalSystem, and a standard user can neither stop it nor edit its configuration. It therefore decides local changes from the caller's Windows account rather than trusting every local process:

- Anyone can read status and settings, and anyone signed in can pause lending.
- The account that paused can resume. Resuming another account's pause, switching a resource, releasing held work and saving settings need an elevated administrator (or an `admin` token).
- The owner console raises the Windows approval prompt for settings, resource switches and another account's pause. Choosing **No** changes nothing. Held-work release has no console control; use the [administrator recovery request](admission.md#recovering-a-held-reservation).
- A script that sends `POST /admin/*` to `127.0.0.1:9811` without a token can only pause. Give it an `admin` token, or run `vacancyd admin-request --request <file>` from an elevated prompt.

The service learns the account from its admin pipe, `\\.\pipe\vacancyd-admin`, which it creates next to the session reporter's pipe with the same access list. [Local administration on Windows](authentication.md#local-administration-on-windows) has the full rules, the request format and the exit codes.

## Installing the user-session agent

The service handles session state; the agent supplies idle time and the foreground application. The Windows package includes a windowless background entry point built from the same client source as `vacancyd agent`.

The MSI owns an all-users Startup shortcut for future interactive desktop logons. It installs regular shortcuts that launch the binaries directly. Windows Installer's advertised activation can mistake the protected configuration for a missing file when an ordinary user signs in, so the package sets [`DISABLEADVTSHORTCUTS`](https://learn.microsoft.com/en-us/windows/win32/msi/disableadvtshortcuts). Startup and console launch do not require configuration access or elevated repair. To start the agent in an already-open session, choose **vacancyd > Start session agent** from the Start menu without elevation. Source/ZIP installations do not install startup entries; create a per-user shortcut only if you want logon startup.

See [session-agent startup](agent-startup.md) for removal, upgrade limitations, prepublic scheduled-task migration and required multi-user acceptance tests.

If you choose idle lending, separately enable the feature it feeds in `vacancyd.toml`:

```toml
[owner]
idle_lends_after_s = 900   # lend after 15 minutes untouched
media_is_presence  = true  # ...but never during playback
```

### Why the second line is not optional

`idle_lends_after_s` on its own has one failure mode, and it is the one a user notices immediately: **a film takes no keyboard input for two hours.** To an idle timer that is indistinguishable from an empty chair, so lending would engage in the middle of a movie and the borrowed work would fight the decoder for the GPU.

`media_is_presence` closes it by reading the **display-required power request** that the player itself asserts. This is the mechanism that stops your screen blanking during playback. Because the signal comes from the application, it needs no list of players and covers ones released after this was written.

| Signal | Held by |
| ------ | ------- |
| `DISPLAY` | video players, streaming browser tabs, presentations; **this is the one** |
| `SYSTEM` | audio drivers, downloads. On the reference machine the Focusrite driver holds it permanently, so treating it as presence would veto lending forever |

Reading power requests **requires elevation**. A LocalSystem service has it. vacancyd reads the `powercfg /requests` report by its structure, not its words, so it works on every Windows display language: the display section is always the first, and a holder is always a line tagged with a bracketed kind. If the read fails, or the report is not in that shape, media reads as unknown and **an unlocked session keeps refusing borrowed work**, both for idle lending and with `unlocked_session_is_presence = false`, rather than assuming nothing is playing. The same rule governs an absent agent: a signal it cannot read must never make it lend more. The veto cause is `media detection unavailable`, and `presence.media_error` in `/status` (and `vacancyd status`) says why. Set `media_is_presence = false` to lend without the check.

Verify with playback running:

```powershell
(Invoke-RestMethod http://127.0.0.1:9811/status).presence.media   # @{playing=vlc}
(Invoke-RestMethod http://127.0.0.1:9811/status).owner_veto.cause # media playback (vlc)
```

With detection failing:

```powershell
(Invoke-RestMethod http://127.0.0.1:9811/status).presence.media_error
# reading power requests requires elevation, and this process is not elevated
```

In metrics, `vacancyd_media_playing` is `1` during playback, `0` when checked and quiet, and `-1` when the signal is switched off or unreadable, so a borrower can tell "quiet" from "not being measured".

### Verifying

```powershell
(Invoke-RestMethod http://127.0.0.1:9811/status).presence   # idle_seconds populated
(Invoke-RestMethod http://127.0.0.1:9811/status).agent      # session_id, version, protocol_version, age_s
```

`idle_seconds` should be a number rather than `null`, and `age_s` should stay small. In metrics, `vacancyd_agent_connected` reads `1` and `vacancyd_owner_idle_seconds` tracks inactivity; `-1` means unknown, which is how "no agent" stays distinguishable from "just touched the keyboard".

Only a fresh, compatible report whose claimed session matches the kernel's identity for that pipe connection is used. A disconnect invalidates it immediately. Windows Home qualification covers a local interactive session; RDP reconnect and multiple simultaneously active sessions require separate Windows Server evidence before they are included in the support claim.

### It is safe to skip

Nothing breaks without the agent. `idle_seconds` stays `null`, unknown idle is never treated as idle, and the daemon behaves exactly as a service-only install: lending when locked, disconnected, logged out or at the sign-in screen, not while unlocked. The agent only ever adds the *walked away without locking* case.

MSI uninstall removes its own startup shortcut along with the agent and service. Manually created tasks and shortcuts require manual removal; configuration is retained.
