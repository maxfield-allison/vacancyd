//! Startup failures an operator can act on, each with its own exit code.
//!
//! On Windows the daemon is built to run as the installed LocalSystem
//! service. Its configuration, credentials, admission records and service log
//! live in directories private to Administrators and LocalSystem, and it
//! assigns Administrators as the owner of what it creates there. An
//! unelevated process, including an administrator's everyday filtered token,
//! can do none of that. Windows then reports whichever call failed first:
//! "Access is denied" (5), "This security ID may not be assigned as the owner
//! of this object" (1307) or "A required privilege is not held" (1314), in the
//! display language of the machine, under a message such as "open admission
//! recovery records". None of that tells the operator what to do.
//!
//! `vacancyd run` therefore checks the process token first and refuses an
//! unelevated start before touching anything ([`require_elevated`]). Every
//! other step that can meet the same wall, and every command that reads the
//! protected files, maps those errors through [`explain`], so a failure that
//! gets past the first check (or occurs in another command) still ends in one
//! message naming the fix. The error keeps the original OS error as its
//! cause, and `main` exits with the kind's code:
//!
//! | Code | Kind |
//! | ---- | ---- |
//! | 50 | [`Kind::NeedsElevation`]: run elevated, or use the installed service |
//! | 51 | [`Kind::InUse`]: another vacancyd, usually the service, holds the state |
//! | 52 | [`Kind::NotUnderServiceManager`]: `vacancyd service` run from a prompt |
//!
//! These do not overlap the `admin-request` codes (20 to 41).

use std::fmt;

/// What kind of startup failure this is. Each has its own exit code.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Kind {
    /// The step needs an elevated administrator or LocalSystem, and this
    /// process is not elevated.
    NeedsElevation,
    /// Another process holds the file exclusively: another vacancyd,
    /// usually the installed service, owns the same admission records.
    InUse,
    /// `vacancyd service` was started from a prompt instead of by the
    /// Windows service manager.
    NotUnderServiceManager,
}

impl Kind {
    pub fn exit_code(self) -> i32 {
        match self {
            Self::NeedsElevation => 50,
            Self::InUse => 51,
            Self::NotUnderServiceManager => 52,
        }
    }
}

/// A startup step that failed for a reason the operator can fix, named in
/// one sentence with the fix. The OS error stays in the chain beneath it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StartupError {
    pub kind: Kind,
    /// The step, as a phrase that completes "… needs": "opening the
    /// admission recovery records in C:\…".
    pub step: String,
}

impl fmt::Display for StartupError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let step = &self.step;
        match self.kind {
            Kind::NeedsElevation => write!(
                f,
                "{step} needs an elevated administrator or the installed vacancyd service, and this process is not elevated. Start the service instead (Start-Service vacancyd), or run this command from an elevated prompt (Run as administrator)"
            ),
            Kind::InUse => write!(
                f,
                "{step} failed because another process is using it, most likely the installed vacancyd service. Stop that instance first (Stop-Service vacancyd), or give this run its own configuration file and listen ports, with server.admission_state_dir empty so that its recovery records are kept beside that file"
            ),
            Kind::NotUnderServiceManager => write!(
                f,
                "`vacancyd service` is started by the Windows service manager, not from a prompt. Start the installed service with Start-Service vacancyd, or use `vacancyd run` to run in this terminal"
            ),
        }
    }
}

impl std::error::Error for StartupError {}

/// Windows system error codes this module recognises. They are named here
/// rather than imported so the mapping is testable on every platform.
const ERROR_ACCESS_DENIED: i32 = 5;
const ERROR_SHARING_VIOLATION: i32 = 32;
const ERROR_FAILED_SERVICE_CONTROLLER_CONNECT: i32 = 1063;
const ERROR_INVALID_OWNER: i32 = 1307;
const ERROR_PRIVILEGE_NOT_HELD: i32 = 1314;

/// The kind a Windows error code means for a startup step, given whether
/// this process is elevated (`None` when that could not be read).
///
/// A permission error means "needs elevation" only when the token is known
/// to be unelevated. An elevated administrator refused access has a real
/// permissions problem, and the original error says more about it than
/// advice to elevate would.
pub fn classify(code: i32, elevated: Option<bool>) -> Option<Kind> {
    match code {
        ERROR_ACCESS_DENIED | ERROR_INVALID_OWNER | ERROR_PRIVILEGE_NOT_HELD
            if elevated == Some(false) =>
        {
            Some(Kind::NeedsElevation)
        }
        ERROR_SHARING_VIOLATION => Some(Kind::InUse),
        ERROR_FAILED_SERVICE_CONTROLLER_CONNECT => Some(Kind::NotUnderServiceManager),
        _ => None,
    }
}

/// The first OS error code in `error`'s chain.
fn os_error_code(error: &anyhow::Error) -> Option<i32> {
    error
        .chain()
        .filter_map(|cause| cause.downcast_ref::<std::io::Error>())
        .find_map(std::io::Error::raw_os_error)
}

/// [`explain`], with the elevation state supplied rather than read, so the
/// unelevated path can be tested from an elevated CI runner.
pub fn explain_with(
    step: impl Into<String>,
    error: anyhow::Error,
    elevated: Option<bool>,
) -> anyhow::Error {
    if error.downcast_ref::<StartupError>().is_some() {
        return error;
    }
    match os_error_code(&error).and_then(|code| classify(code, elevated)) {
        Some(kind) => error.context(StartupError {
            kind,
            step: step.into(),
        }),
        None => error,
    }
}

/// Name the fix when `error`, from `step`, is one of the startup failures
/// above. Other errors pass through unchanged. A no-op outside Windows,
/// where these codes mean something else.
pub fn explain(step: impl Into<String>, error: anyhow::Error) -> anyhow::Error {
    if !cfg!(windows) {
        return error;
    }
    explain_with(step, error, current_process_is_elevated().ok())
}

/// Refuse to start `step` unelevated, before it touches anything. An
/// unreadable token is not a refusal: the step then runs, and [`explain`]
/// still maps whatever it meets.
// Called by `run` on Windows only; the tests exercise it everywhere.
#[cfg_attr(not(windows), allow(dead_code))]
pub fn require_elevated(step: &str) -> anyhow::Result<()> {
    match current_process_is_elevated() {
        Ok(false) => Err(anyhow::Error::new(StartupError {
            kind: Kind::NeedsElevation,
            step: step.into(),
        })),
        Ok(true) | Err(_) => Ok(()),
    }
}

/// The startup failure in `error`'s chain, if it is one.
pub fn startup_error(error: &anyhow::Error) -> Option<&StartupError> {
    error.downcast_ref::<StartupError>()
}

/// Whether this process's token is elevated. LocalSystem is; a standard
/// user, or an administrator's filtered token, is not.
#[cfg(windows)]
pub fn current_process_is_elevated() -> std::io::Result<bool> {
    use windows_sys::Win32::Foundation::{CloseHandle, HANDLE};
    use windows_sys::Win32::Security::{
        GetTokenInformation, TokenElevation, TOKEN_ELEVATION, TOKEN_QUERY,
    };
    use windows_sys::Win32::System::Threading::{GetCurrentProcess, OpenProcessToken};

    let mut token: HANDLE = std::ptr::null_mut();
    // SAFETY: the pseudo-handle from GetCurrentProcess needs no closing, and
    // `token` is a valid out-parameter.
    if unsafe { OpenProcessToken(GetCurrentProcess(), TOKEN_QUERY, &mut token) } == 0 {
        return Err(std::io::Error::last_os_error());
    }
    let mut elevation = TOKEN_ELEVATION { TokenIsElevated: 0 };
    let mut returned = 0u32;
    // SAFETY: the buffer is a TOKEN_ELEVATION of the stated size.
    let ok = unsafe {
        GetTokenInformation(
            token,
            TokenElevation,
            (&mut elevation as *mut TOKEN_ELEVATION).cast(),
            std::mem::size_of::<TOKEN_ELEVATION>() as u32,
            &mut returned,
        )
    };
    let result = if ok == 0 {
        Err(std::io::Error::last_os_error())
    } else {
        Ok(elevation.TokenIsElevated != 0)
    };
    // SAFETY: `token` was opened above and is closed exactly once.
    unsafe { CloseHandle(token) };
    result
}

/// Outside Windows there is no token elevation to read; the Unix startup
/// paths report their own permission errors.
#[cfg(not(windows))]
pub fn current_process_is_elevated() -> std::io::Result<bool> {
    Err(std::io::Error::new(
        std::io::ErrorKind::Unsupported,
        "token elevation is a Windows concept",
    ))
}

#[cfg(test)]
mod tests {
    use super::*;
    use anyhow::Context;

    /// An error shaped like the reported failure: the OS error from the ACL call,
    /// under the context the startup code adds.
    fn from_os(code: i32, context: &str) -> anyhow::Error {
        Err::<(), _>(std::io::Error::from_raw_os_error(code))
            .context(context.to_string())
            .unwrap_err()
    }

    #[test]
    fn the_owner_error_unelevated_names_the_fix_and_exit_code() {
        let error = explain_with(
            "opening the admission recovery records",
            from_os(ERROR_INVALID_OWNER, "open admission recovery records"),
            Some(false),
        );
        let startup = startup_error(&error).expect("mapped");
        assert_eq!(startup.kind, Kind::NeedsElevation);
        assert_eq!(startup.kind.exit_code(), 50);
        let text = format!("{error:#}");
        assert!(text.starts_with("opening the admission recovery records needs an elevated administrator or the installed vacancyd service"), "{text}");
        assert!(text.contains("Start-Service vacancyd"), "{text}");
        assert!(text.contains("Run as administrator"), "{text}");
        // The OS error is kept as the cause, for whoever needs the detail.
        assert!(text.contains("open admission recovery records"), "{text}");
        let root = error.root_cause().downcast_ref::<std::io::Error>().unwrap();
        assert_eq!(root.raw_os_error(), Some(ERROR_INVALID_OWNER));
    }

    #[test]
    fn every_permission_code_maps_only_when_unelevated() {
        for code in [
            ERROR_ACCESS_DENIED,
            ERROR_INVALID_OWNER,
            ERROR_PRIVILEGE_NOT_HELD,
        ] {
            assert_eq!(classify(code, Some(false)), Some(Kind::NeedsElevation));
            // An elevated administrator refused access has a real ACL
            // problem; advice to elevate would be wrong.
            assert_eq!(classify(code, Some(true)), None);
            // Unknown elevation claims nothing.
            assert_eq!(classify(code, None), None);
            let error = explain_with("step", from_os(code, "ctx"), Some(true));
            assert!(startup_error(&error).is_none());
        }
    }

    #[test]
    fn a_sharing_violation_is_another_instance_whatever_the_token() {
        for elevated in [Some(true), Some(false), None] {
            assert_eq!(
                classify(ERROR_SHARING_VIOLATION, elevated),
                Some(Kind::InUse)
            );
        }
        let error = explain_with(
            "opening the admission recovery records",
            from_os(ERROR_SHARING_VIOLATION, "open admission recovery records"),
            Some(true),
        );
        let startup = startup_error(&error).unwrap();
        assert_eq!(startup.kind.exit_code(), 51);
        assert!(format!("{error}").contains("Stop-Service vacancyd"));
    }

    #[test]
    fn the_service_command_outside_the_service_manager_says_so() {
        let error = explain_with(
            "starting the service",
            from_os(
                ERROR_FAILED_SERVICE_CONTROLLER_CONNECT,
                "starting Windows service dispatcher",
            ),
            Some(true),
        );
        let startup = startup_error(&error).unwrap();
        assert_eq!(startup.kind.exit_code(), 52);
        assert!(format!("{error}").contains("vacancyd run"));
    }

    #[test]
    fn unrelated_errors_pass_through_unchanged() {
        let missing = explain_with("reading", from_os(2, "reading config file x"), Some(false));
        assert!(startup_error(&missing).is_none());
        assert_eq!(format!("{missing}"), "reading config file x");
        let plain = explain_with("parsing", anyhow::anyhow!("bad toml"), Some(false));
        assert!(startup_error(&plain).is_none());
    }

    #[test]
    fn the_startup_error_is_found_under_later_context() {
        // main reads the exit code from the whole chain: context added by a
        // caller after the mapping must not hide it.
        let error = explain_with(
            "reading the configuration",
            from_os(ERROR_ACCESS_DENIED, "reading config file x"),
            Some(false),
        )
        .context("starting the daemon");
        assert_eq!(startup_error(&error).unwrap().kind, Kind::NeedsElevation);
        // And an already-mapped error is not mapped twice.
        let again = explain_with("outer step", error, Some(false));
        assert!(!format!("{again:#}").contains("outer step"));
    }

    #[test]
    fn exit_codes_are_distinct_and_clear_of_admin_request_codes() {
        let codes = [
            Kind::NeedsElevation.exit_code(),
            Kind::InUse.exit_code(),
            Kind::NotUnderServiceManager.exit_code(),
        ];
        let unique: std::collections::HashSet<_> = codes.iter().collect();
        assert_eq!(unique.len(), codes.len());
        // docs/authentication.md: admin-request uses 0, 20-28, 40, 41.
        assert!(codes.iter().all(|c| !(20..=41).contains(c) && *c > 1));
    }

    #[cfg(windows)]
    #[test]
    fn the_preflight_agrees_with_the_token() {
        let elevated = current_process_is_elevated().unwrap();
        let result = require_elevated("running the vacancyd daemon");
        assert_eq!(result.is_ok(), elevated, "{result:?}");
        if let Err(error) = result {
            assert_eq!(startup_error(&error).unwrap().kind.exit_code(), 50);
        }
    }

    #[cfg(not(windows))]
    #[test]
    fn nothing_is_refused_or_mapped_outside_windows() {
        assert!(require_elevated("running the vacancyd daemon").is_ok());
        let error = explain("step", from_os(ERROR_ACCESS_DENIED, "ctx"));
        assert!(startup_error(&error).is_none());
    }
}
