# Windows service and session reporter

The service decides whether to lend. The reporter supplies the idle time and foreground process for its own Windows session. The packaged owner console displays the service's answer and offers the bounded controls described in [the console guide](console.md).

## Why the reporter runs in the user session

A Windows service runs in session 0, without the user's interactive desktop. It can read session state through the Terminal Services API, but [GetLastInputInfo](https://learn.microsoft.com/en-us/windows/win32/api/winuser/nf-winuser-getlastinputinfo) reports input only for the calling session. Foreground-window detection has the same desktop boundary. The windowless `vacancyd-session.exe` runs as the signed-in user and supplies those facts to `vacancyd.exe`.

The service can still lend while the owner is locked or logged out without an idle-time report. Relaxing unlocked-session presence through `owner.idle_lends_after_s` requires a fresh, compatible reporter observation. Missing or unreadable evidence never counts as an idle owner.

## One package, matching versions

The Windows package contains the daemon, `vacancyd-session.exe` and `vacancyd-console.exe`. The reporter shares its client and report-format source with the Windows-only `vacancyd agent` diagnostic command. The MSI creates the reporter's all-users Startup shortcut for subsequent logons. In an already-open session, use **Start session agent** from the Start menu without elevation.

The service requires an exact reporter package-version match as well as protocol version 1. An upgrade can replace files while an older reporter remains alive in another session, so complete the MSI-required reboot and verify fresh reports afterward. See [startup and upgrades](agent-startup.md).

## Named-pipe boundary

The service creates `\\.\pipe\vacancyd-agent` with an explicit access-control list. LocalSystem and Administrators have full control. Interactive Users receive read and write-data access without the right to create pipe instances. Remote named-pipe clients are refused.

The service obtains the client session from the Windows kernel and rejects a report claiming another session. This prevents one signed-in session from supplying another's idle observation. It does not distinguish the packaged reporter from another trusted process in the same logon session; that local boundary remains an operating assumption.

## Failure and freshness

- Each reporter owns a session-local singleton; another Windows session can have its own reporter. Session 0 cannot run one.
- Reports include sample time and a per-connection sequence. They expire after `max(4 × poll interval, 15 seconds)`, and disconnect invalidates them.
- Remote clients, oversized frames, unknown fields, mismatched protocol or package versions and inconsistent session identities are refused.
- A service-side channel failure logs a warning and retries. A reporter retries connection failures; its Startup shortcut is not a supervisor for a crashed process.
- Reporter and console lifetimes remain separate from the service. Closing the console does not stop the reporter or change lending policy.

These are implemented boundaries. Windows Home acceptance covers one local interactive session; RDP reconnect and simultaneously active sessions require separate Windows Server evidence before broader support is claimed.
