# Session-agent startup

Windows packages include `vacancyd-session.exe`, a windowless entry point built from the same client and report-format source as `vacancyd agent`. It reports session facts; it does not decide lending, load configuration, or hold borrower credentials. Keep both executables from the same package together.

Both entry points acquire the same session-local singleton. A second instance exits without reporting; another interactive session can have its own agent. Session 0 is rejected. Failure to acquire the lock does not permit another reporter.

## MSI installation

The service reads the complete Windows session inventory independently of reporters. After sign-out, Windows may retain an unowned `WTSDown` entry alongside the unowned `WTSConnected` console. The service accepts that view only after two identical successful reads. Each entry must have a valid `WTSINFOEX` reply matching its enumerated ID and state, with an empty user name. A Down entry with a user, a failed or conflicting query, or another unresolved state still refuses lending. Each active, unlocked console or RDP session still requires its own fresh reporter; an unowned Down entry cannot hide it. The advanced `owner.sign_in_screen_is_presence` policy still applies to a connected sign-in screen.

See the Windows [session states](https://learn.microsoft.com/en-us/windows/win32/api/wtsapi32/ne-wtsapi32-wts_connectstate_class) and [extended session information](https://learn.microsoft.com/en-us/windows/win32/api/wtsapi32/ns-wtsapi32-wtsinfoex_level1_w). A Down state alone does not establish that a session is unowned.

The per-machine MSI installs an all-users Startup shortcut targeting the installed background executable directly. No scheduled task, shell, password, or elevated agent is involved. Windows resolves the MSI's [StartupFolder](https://learn.microsoft.com/en-us/windows/win32/msi/startupfolder) to the all-users location for a per-machine install. The MSI owns shortcut creation, rollback and removal. Its `DISABLEADVTSHORTCUTS=1` property creates regular shortcuts from the installed feature/file targets, avoiding an elevated repair request when an ordinary user cannot inspect the protected configuration key path.

This starts a reporter at subsequent desktop logons, not immediately in sessions that are already open. To start it now, choose **vacancyd > Start session agent** from the Start menu as the ordinary logged-in user. Do not run it as administrator. Installing or starting the agent never enables idle lending automatically.

Startup can be delayed or disabled by Windows startup settings or local policy. The MSI entry is machine-wide: Windows may request administrator credentials when changing it in **Settings > Apps > Startup**, even from a standard-user desktop. This changes logon startup; an already-running reporter keeps running until it exits or the user signs out. Non-Explorer shells need explicit session startup integration. A shortcut is not a supervisor: it does not restart a crashed agent. Connection failures are retried inside the agent; an exited agent can be started again from the Start menu. Missing or expired reports retain conservative presence behavior, not assumed idle.

The ZIP does not install startup entries. For a portable deployment, run the windowless executable yourself or create a shortcut in your own Startup folder (`shell:startup`). Keep the executables in a stable location, and remove that manually owned shortcut before moving or deleting the files.

## Upgrade and uninstall

MSI major upgrades remove the old product inside the installation transaction, allowing Windows Installer to restore it if the upgrade rolls back. The agent has a hidden top-level message window for Windows shutdown notifications. It acknowledges a shutdown query without stopping; only confirmed shutdown or close cancels reporting and releases its pipe and session lock. No process-name kill or cross-session launcher is used.

The background entry point registers for update restarts using [RegisterApplicationRestart](https://learn.microsoft.com/en-us/windows/win32/api/winbase/nf-winbase-registerapplicationrestart). This is best-effort, not a promise of uninterrupted reporting: Windows requires at least 60 seconds of runtime before restarting an application, and Restart Manager [does not restart another user's applications](https://learn.microsoft.com/en-us/windows/win32/rstmgr/guidelines-for-applications). An MSI running in a different session can replace both files while leaving old reporters alive. The service rejects their old versions, and their session lock can prevent a new Start-menu launch from taking over. Major upgrades therefore request a reboot explicitly. Use `/norestart` for unattended MSI operations and handle exit code `3010` as **reboot required**, not completed acceptance. Finish the reboot, sign in, and verify agent version and fresh reports before using the upgraded installation. A clean install does not request this extra reboot.

Uninstall also requests a reboot so reporters in other sessions cannot survive removal. `/norestart` defers that reboot and returns `3010`. Verify removal after restarting. Uninstall removes the MSI-owned shortcuts and executables. It preserves configuration, credentials, calibration and logs under ProgramData. It does not remove a user's manually created task or shortcut. Stop foreground `vacancyd agent` instances from their own terminals before replacing their executable.

When a running reporter delays file deletion until reboot, an empty `C:\Program Files\vacancyd` directory can remain afterward. Verify that it is empty, including hidden entries, before removing it manually if desired. The retained ProgramData directory is separate and contains the operator's data.

### Migrating prepublic scheduled tasks

The experimental `agent-startup` task-management command has been removed. Before upgrading a deployment that used it, inspect and remove its task with the **old** binary's `agent-startup remove --apply`, from an elevated prompt after reviewing the task. For hand-written or changed tasks, review their exact actions and remove them manually. The MSI does not guess ownership from a name or delete foreign startup entries. Leaving an old task can start an obsolete reporter first and prevent the new one from acquiring the session lock.

## Verification matrix

Use `/status` to check the agent's session, version and report age. The GUI agent does not create a persistent log; use `vacancyd agent` in a non-elevated terminal for connection diagnostics after exiting the background instance.

Unit tests send messages only to hidden windows created by those tests. They check canceled/confirmed shutdown, worker completion/panic, and cancellation. Package tests inspect MSI source ownership; build/decompilation checks inspect the resulting artifact. **Neither proves an installation works.**

Record applicable checks on a disposable Windows VM and retain MSI logs:

1. Fresh install and next logon: no console, one unprivileged agent in the right session; current-session Start-menu launch and duplicate launch behave correctly.
2. Two users, fast user switching, RDP reconnect and logout: separate reporters, no session-0 reporter, and no surviving process after logout.
3. Service outage and recovery, plus disabled Startup and a crashed agent: stale/missing reports remain conservative, and manual restart restores reporting.
4. Upgrade with running agents both younger and older than 60 seconds, including another logged-in user: binaries replace or a restart is requested explicitly; all resumed reporters match the package version.
5. Force a controlled upgrade failure after old-product removal: rollback restores the previous files, service and shortcuts, preserving configuration byte-for-byte.
6. Uninstall with agents running: no owned shortcut or binary remains after any requested restart; configuration and unrelated tasks/shortcuts remain untouched.
7. Repeat with non-admin users and an administrator supplying installation credentials. Check startup and menu shortcuts never elevate the reporter, and run MSI ICE validation. Verify a disabled startup preference survives upgrade.

The v1.0.0 Windows Home evidence covers clean installation, ordinary-user reporting and console launch, a running-process upgrade, the required reboot and login recovery, and protected configuration preservation. It does not establish RDP hosting or two concurrently active interactive sessions. Complete case 2 and the cross-user parts of cases 4 and 6 on Windows Server before making those broader support claims.
