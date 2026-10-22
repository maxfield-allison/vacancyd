//! Who may change lending on this machine, and the local channel that proves
//! who is asking.
//!
//! # The rule
//!
//! A local caller is judged by what the operating system can prove about it,
//! never by anything the caller says about itself.
//!
//! | Action | Effect | Who may |
//! | ------ | ------ | ------- |
//! | read status, settings or diagnostics | shows owner presence and policy | any local user |
//! | pause lending | only reduces lending | any signed-in user |
//! | resume lending | can increase lending | the account that paused, or an administrator |
//! | resource on/off, release held work | can increase lending | an administrator |
//! | save settings | rewrites the protected configuration | an administrator |
//! | open or end a debug logging window | changes what the service writes to its log | an administrator |
//!
//! "Administrator" means a caller the service can see is an administrator on
//! its platform, or a request carrying an `admin`-scoped bearer token:
//!
//! - **Windows**: an elevated member of `BUILTIN\Administrators`, or
//!   LocalSystem, identified from the caller's token over the admin named
//!   pipe.
//! - **Linux**: root, or a member of the configured admin group (by default
//!   `vacancyd-admin`), identified from the peer credentials of the admin
//!   Unix socket. "Signed-in user" there means any local account that can
//!   connect to the socket: pausing only reduces lending, and TCP loopback
//!   already allows it, so requiring a logind session would add no
//!   protection.
//!
//! # Why a TCP loopback connection is not enough
//!
//! A TCP connection from 127.0.0.1 says nothing about which account opened
//! it. The service writes a configuration file that only administrators may
//! edit (LocalSystem and Administrators on Windows, root on Linux), so
//! treating every local process as the owner would let a standard user or a
//! second signed-in account rewrite the owner's protection through the
//! service. TCP loopback therefore keeps only reading and pausing on Windows
//! and Linux. Everything else arrives over the platform's admin channel,
//! where the service reads the caller's identity from the kernel.
//!
//! Other platforms (macOS) keep full loopback trust this release.

use std::io;
use std::path::Path;

use serde::{Deserialize, Serialize};
use tokio::io::{AsyncBufRead, AsyncBufReadExt, AsyncReadExt};

use crate::settings::{SettingsSnapshot, SettingsUpdate};
use crate::status::Logging;

#[cfg(windows)]
pub mod client;
#[cfg(target_os = "linux")]
pub mod socket_client;

/// Wire contract version for the admin pipe and the admin socket.
pub const PROTOCOL_VERSION: u32 = 1;
/// Largest request accepted, sized for a complete settings document with
/// every list at its validation limit.
pub const MAX_REQUEST_BYTES: usize = 256 * 1024;
/// Largest response accepted; a saved settings snapshot is the largest.
pub const MAX_RESPONSE_BYTES: usize = 256 * 1024;
/// The service's admin pipe.
#[cfg(windows)]
pub const PIPE_NAME: &str = r"\\.\pipe\vacancyd-admin";
/// The service's admin socket unless `server.admin_socket` names another.
/// Under the packaged systemd unit its directory is the service's
/// `RuntimeDirectory`.
pub const DEFAULT_SOCKET_PATH: &str = "/run/vacancyd/admin.sock";
/// The group whose members are administrators on Linux unless
/// `server.admin_group` names another.
pub const DEFAULT_ADMIN_GROUP: &str = "vacancyd-admin";

/// One local administrative action, independent of how it arrived.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Action {
    ReadSettings,
    /// The diagnostics export: status, settings and recent pressure
    /// readings, all of which status and settings reads already show.
    ReadDiagnostics,
    Pause,
    Resume,
    SetResource,
    ReleaseWork,
    SaveSettings,
    /// Open or end a temporary debug logging window.
    SetDebug,
    /// An admin path this version does not recognise. Treated as the most
    /// privileged action so a route added later fails closed.
    Other,
}

impl Action {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::ReadSettings => "read_settings",
            Self::ReadDiagnostics => "read_diagnostics",
            Self::Pause => "pause",
            Self::Resume => "resume",
            Self::SetResource => "set_resource",
            Self::ReleaseWork => "release_work",
            Self::SaveSettings => "save_settings",
            Self::SetDebug => "set_debug",
            Self::Other => "other",
        }
    }
}

/// Who is asking, as far as the transport can prove it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Caller {
    /// A request carrying a bearer token with the `admin` scope.
    AdminToken,
    /// A direct TCP loopback client. The account behind it is unknown.
    Loopback,
    /// A local account identified by the operating system: on Windows, the
    /// admin pipe client's token; on Linux, the admin socket's peer
    /// credentials.
    Account {
        /// A stable identifier for the account: its security identifier on
        /// Windows (`S-1-5-21-...`), `uid:<n>` on Linux.
        account: String,
        /// On Windows an elevated member of `BUILTIN\Administrators`, or
        /// LocalSystem; on Linux root, or a member of the admin group.
        administrator: bool,
    },
}

/// Who paused lending, which decides who may resume it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PausedBy {
    /// A local account identified by the operating system.
    Account(String),
    /// A caller without a proven account: TCP loopback or a token. Only an
    /// administrator may resume this pause.
    Unattributed,
}

impl PausedBy {
    /// The record a pause by `caller` leaves.
    pub fn from_caller(caller: &Caller) -> Self {
        match caller {
            Caller::Account { account, .. } => Self::Account(account.clone()),
            Caller::AdminToken | Caller::Loopback => Self::Unattributed,
        }
    }
}

/// How much a TCP loopback caller is trusted on this platform.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Policy {
    /// Grant every admin action to a direct loopback client.
    pub loopback_is_administrator: bool,
}

impl Policy {
    /// Windows identifies local callers through the admin pipe and Linux
    /// through the admin socket, so loopback keeps read and pause only
    /// there. Other platforms keep full loopback trust until they have a
    /// channel that identifies the caller.
    pub const fn for_this_platform() -> Self {
        Self {
            loopback_is_administrator: !(cfg!(windows) || cfg!(target_os = "linux")),
        }
    }
}

/// Decide whether `caller` may perform `action`.
///
/// `paused` is who paused lending, or `None` while lending is on; it matters
/// only for [`Action::Resume`].
pub fn permits(
    policy: Policy,
    action: Action,
    caller: &Caller,
    paused: Option<&PausedBy>,
) -> Result<(), RefusalReason> {
    let administrator = match caller {
        Caller::AdminToken => true,
        Caller::Loopback => policy.loopback_is_administrator,
        Caller::Account { administrator, .. } => *administrator,
    };
    if administrator {
        return Ok(());
    }
    match (action, caller) {
        (Action::ReadSettings | Action::ReadDiagnostics | Action::Pause, _) => Ok(()),
        // Resuming lending that is already on changes nothing.
        (Action::Resume, Caller::Account { .. }) if paused.is_none() => Ok(()),
        (Action::Resume, Caller::Account { account, .. })
            if paused == Some(&PausedBy::Account(account.clone())) =>
        {
            Ok(())
        }
        _ => Err(RefusalReason::AdministratorRequired),
    }
}

/// Why the service refused a local admin request. Fixed and low-cardinality,
/// so a message can be shown to the owner and a code can cross the
/// elevation boundary as a process exit status.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RefusalReason {
    AdministratorRequired,
    InvalidRequest,
    Validation,
    StaleRevision,
    NotFound,
    Conflict,
    ConfigInvalid,
    CredentialUnresolved,
    Persistence,
}

impl RefusalReason {
    const ALL: [Self; 9] = [
        Self::AdministratorRequired,
        Self::InvalidRequest,
        Self::Validation,
        Self::StaleRevision,
        Self::NotFound,
        Self::Conflict,
        Self::ConfigInvalid,
        Self::CredentialUnresolved,
        Self::Persistence,
    ];

    pub fn as_str(self) -> &'static str {
        match self {
            Self::AdministratorRequired => "administrator_required",
            Self::InvalidRequest => "invalid_request",
            Self::Validation => "validation",
            Self::StaleRevision => "stale_revision",
            Self::NotFound => "not_found",
            Self::Conflict => "conflict",
            Self::ConfigInvalid => "config_invalid",
            Self::CredentialUnresolved => "credential_unresolved",
            Self::Persistence => "persistence",
        }
    }

    /// Plain words for the owner console.
    pub fn message(self) -> &'static str {
        match self {
            Self::AdministratorRequired => "This change needs administrator approval.",
            Self::InvalidRequest => "The service did not understand this request. Check that the service and console versions match.",
            Self::Validation => "The service rejected these values.",
            Self::StaleRevision => "Settings changed since this screen was opened. Refresh and try again.",
            Self::NotFound => "The service no longer recognizes this resource or work entry.",
            Self::Conflict => "The service refused to release this work.",
            Self::ConfigInvalid => "The proposed settings conflict with the configuration. Nothing was saved. Check the service log.",
            Self::CredentialUnresolved => "A saved settings value could not resolve its credential. Check the service log.",
            Self::Persistence => "The service could not save settings. Refresh before trying again.",
        }
    }
}

/// A request on the admin pipe. `S` is the settings payload: the typed
/// update for clients, raw JSON for the service, which validates it with the
/// same strict parser as the HTTP settings API.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Request<S = SettingsUpdate> {
    pub protocol_version: u32,
    pub action: RequestedAction<S>,
}

impl<S> Request<S> {
    pub fn new(action: RequestedAction<S>) -> Self {
        Self {
            protocol_version: PROTOCOL_VERSION,
            action,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
pub enum RequestedAction<S = SettingsUpdate> {
    // Empty struct variants rather than unit variants, so an unexpected
    // field is refused like everywhere else in the protocol.
    Pause {},
    Resume {},
    SetResource {
        resource: String,
        enabled: bool,
    },
    ReleaseWork {
        id: u64,
        upstream_stopped: bool,
    },
    SaveSettings {
        settings: S,
    },
    /// Log vacancyd's own events at debug for `minutes` (1 to 240), or move
    /// the end of a window already open.
    StartDebug {
        minutes: u64,
    },
    /// End an open debug window early.
    EndDebug {},
}

impl<S> RequestedAction<S> {
    pub fn action(&self) -> Action {
        match self {
            Self::Pause {} => Action::Pause,
            Self::Resume {} => Action::Resume,
            Self::SetResource { .. } => Action::SetResource,
            Self::ReleaseWork { .. } => Action::ReleaseWork,
            Self::SaveSettings { .. } => Action::SaveSettings,
            Self::StartDebug { .. } | Self::EndDebug {} => Action::SetDebug,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "outcome", rename_all = "snake_case", deny_unknown_fields)]
pub enum Response {
    Lending {
        lending_enabled: bool,
    },
    Resource {
        resource: String,
        enabled: bool,
    },
    Released,
    Saved {
        settings: Box<SettingsSnapshot>,
        restart_required: bool,
    },
    Logging {
        logging: Logging,
    },
    Refused {
        reason: RefusalReason,
    },
}

impl Response {
    /// Whether this response confirms exactly the change `action` asked for.
    pub fn acknowledges<S>(&self, action: &RequestedAction<S>) -> bool {
        match (action, self) {
            (RequestedAction::Pause {}, Self::Lending { lending_enabled }) => !lending_enabled,
            (RequestedAction::Resume {}, Self::Lending { lending_enabled }) => *lending_enabled,
            (
                RequestedAction::SetResource { resource, enabled },
                Self::Resource {
                    resource: acknowledged,
                    enabled: now,
                },
            ) => resource == acknowledged && enabled == now,
            (RequestedAction::ReleaseWork { .. }, Self::Released) => true,
            (
                RequestedAction::SaveSettings { .. },
                Self::Saved {
                    settings,
                    restart_required,
                },
            ) => *restart_required == settings.application.restart_required,
            (RequestedAction::StartDebug { .. }, Self::Logging { logging }) => {
                logging.debug_until.is_some()
            }
            (RequestedAction::EndDebug {}, Self::Logging { logging }) => {
                logging.debug_until.is_none()
            }
            _ => false,
        }
    }
}

/// How the elevated `vacancyd admin-request` helper reports its result. The
/// console cannot read an elevated process's output, only its exit status,
/// so each outcome has a fixed code.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum HelperExit {
    /// The service confirmed the change.
    Applied,
    /// The service refused it.
    Refused(RefusalReason),
    /// The service could not be reached, or its answer could not be read or
    /// did not confirm the change. The outcome is unknown.
    Unconfirmed,
    /// The request file was missing, too large or not a valid request.
    BadRequestFile,
}

impl HelperExit {
    const REFUSED_BASE: i32 = 20;
    const UNCONFIRMED: i32 = 40;
    const BAD_REQUEST_FILE: i32 = 41;

    pub fn code(self) -> i32 {
        match self {
            Self::Applied => 0,
            Self::Refused(reason) => {
                Self::REFUSED_BASE
                    + RefusalReason::ALL
                        .iter()
                        .position(|candidate| *candidate == reason)
                        .expect("every reason is listed") as i32
            }
            Self::Unconfirmed => Self::UNCONFIRMED,
            Self::BadRequestFile => Self::BAD_REQUEST_FILE,
        }
    }

    /// `None` for a status this helper never returns, such as a crash.
    pub fn from_code(code: i32) -> Option<Self> {
        match code {
            0 => Some(Self::Applied),
            Self::UNCONFIRMED => Some(Self::Unconfirmed),
            Self::BAD_REQUEST_FILE => Some(Self::BadRequestFile),
            code => usize::try_from(code - Self::REFUSED_BASE)
                .ok()
                .and_then(|index| RefusalReason::ALL.get(index))
                .map(|reason| Self::Refused(*reason)),
        }
    }

    /// The helper's result for one pipe response.
    pub fn from_response<S>(action: &RequestedAction<S>, response: &Response) -> Self {
        match response {
            Response::Refused { reason } => Self::Refused(*reason),
            response if response.acknowledges(action) => Self::Applied,
            _ => Self::Unconfirmed,
        }
    }
}

/// Read one newline-terminated frame of at most `max` bytes. `Ok(None)`
/// means the peer closed without sending anything.
pub async fn read_frame<R: AsyncBufRead + Unpin>(
    reader: &mut R,
    max: usize,
) -> io::Result<Option<Vec<u8>>> {
    let mut frame = Vec::new();
    let limit = u64::try_from(max).unwrap_or(u64::MAX).saturating_add(1);
    reader.take(limit).read_until(b'\n', &mut frame).await?;
    if frame.is_empty() {
        return Ok(None);
    }
    if frame.pop() != Some(b'\n') {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "admin frame is incomplete or exceeds its size limit",
        ));
    }
    if frame.last() == Some(&b'\r') {
        frame.pop();
    }
    Ok(Some(frame))
}

/// Read a request staged for the elevated helper, refusing anything larger
/// than a request can be.
pub fn read_request_file(path: &Path) -> io::Result<Request> {
    let file = std::fs::File::open(path)?;
    if !file.metadata()?.is_file() {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "request is not a regular file",
        ));
    }
    read_request(file)
}

/// Read one request of at most [`MAX_REQUEST_BYTES`] from `source` and check
/// its protocol version.
pub fn read_request(source: impl io::Read) -> io::Result<Request> {
    use std::io::Read;
    let mut bytes = Vec::new();
    source
        .take(MAX_REQUEST_BYTES as u64 + 1)
        .read_to_end(&mut bytes)?;
    if bytes.len() > MAX_REQUEST_BYTES {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "request exceeds its size limit",
        ));
    }
    let request: Request = serde_json::from_slice(&bytes)
        .map_err(|error| io::Error::new(io::ErrorKind::InvalidData, error))?;
    if request.protocol_version != PROTOCOL_VERSION {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "request uses another protocol version",
        ));
    }
    Ok(request)
}

#[cfg(test)]
mod tests {
    use super::*;

    const RESTRICTED: Policy = Policy {
        loopback_is_administrator: false,
    };
    const LEGACY: Policy = Policy {
        loopback_is_administrator: true,
    };

    fn user(account: &str) -> Caller {
        Caller::Account {
            account: account.into(),
            administrator: false,
        }
    }

    fn admin(account: &str) -> Caller {
        Caller::Account {
            account: account.into(),
            administrator: true,
        }
    }

    const EVERY_ACTION: [Action; 9] = [
        Action::ReadSettings,
        Action::ReadDiagnostics,
        Action::Pause,
        Action::Resume,
        Action::SetResource,
        Action::ReleaseWork,
        Action::SaveSettings,
        Action::SetDebug,
        Action::Other,
    ];

    #[test]
    fn a_standard_user_may_read_and_pause_but_not_change_policy() {
        let alice = user("S-1-5-21-1-1001");
        for action in [Action::ReadSettings, Action::ReadDiagnostics, Action::Pause] {
            assert_eq!(
                permits(RESTRICTED, action, &alice, None),
                Ok(()),
                "{action:?}"
            );
        }
        for action in [
            Action::SetResource,
            Action::ReleaseWork,
            Action::SaveSettings,
            Action::SetDebug,
            Action::Other,
        ] {
            assert_eq!(
                permits(RESTRICTED, action, &alice, None),
                Err(RefusalReason::AdministratorRequired),
                "{action:?}"
            );
        }
    }

    #[test]
    fn only_the_account_that_paused_may_resume_without_an_administrator() {
        let alice = user("S-1-5-21-1-1001");
        let bob = user("S-1-5-21-1-1002");
        let paused_by_alice = PausedBy::from_caller(&alice);
        assert_eq!(
            permits(RESTRICTED, Action::Resume, &alice, Some(&paused_by_alice)),
            Ok(())
        );
        assert_eq!(
            permits(RESTRICTED, Action::Resume, &bob, Some(&paused_by_alice)),
            Err(RefusalReason::AdministratorRequired)
        );
        // An administrator may resume anyone's pause.
        assert_eq!(
            permits(
                RESTRICTED,
                Action::Resume,
                &admin("S-1-5-21-1-500"),
                Some(&paused_by_alice)
            ),
            Ok(())
        );
        assert_eq!(
            permits(
                RESTRICTED,
                Action::Resume,
                &Caller::AdminToken,
                Some(&paused_by_alice)
            ),
            Ok(())
        );
        // Resuming lending that is already on changes nothing.
        assert_eq!(permits(RESTRICTED, Action::Resume, &bob, None), Ok(()));
    }

    #[test]
    fn a_pause_without_a_proven_account_needs_an_administrator_to_resume() {
        let unattributed = PausedBy::from_caller(&Caller::Loopback);
        assert_eq!(unattributed, PausedBy::Unattributed);
        assert_eq!(
            PausedBy::from_caller(&Caller::AdminToken),
            PausedBy::Unattributed
        );
        assert_eq!(
            permits(
                RESTRICTED,
                Action::Resume,
                &user("S-1-5-21-1-1001"),
                Some(&unattributed)
            ),
            Err(RefusalReason::AdministratorRequired)
        );
        assert_eq!(
            permits(
                RESTRICTED,
                Action::Resume,
                &admin("S-1-5-18"),
                Some(&unattributed)
            ),
            Ok(())
        );
    }

    #[test]
    fn restricted_loopback_keeps_read_and_pause_only() {
        for action in EVERY_ACTION {
            let expected = match action {
                Action::ReadSettings | Action::ReadDiagnostics | Action::Pause => Ok(()),
                _ => Err(RefusalReason::AdministratorRequired),
            };
            assert_eq!(
                permits(RESTRICTED, action, &Caller::Loopback, None),
                expected,
                "{action:?}"
            );
            assert_eq!(
                permits(
                    RESTRICTED,
                    action,
                    &Caller::Loopback,
                    Some(&PausedBy::Unattributed)
                ),
                expected,
                "{action:?} while paused"
            );
        }
    }

    #[test]
    fn administrators_tokens_and_legacy_loopback_may_do_everything() {
        for action in EVERY_ACTION {
            for (policy, caller) in [
                (RESTRICTED, admin("S-1-5-18")),
                (RESTRICTED, Caller::AdminToken),
                (LEGACY, Caller::Loopback),
            ] {
                assert_eq!(
                    permits(
                        policy,
                        action,
                        &caller,
                        Some(&PausedBy::Account("S-1-5-21-1-1001".into()))
                    ),
                    Ok(()),
                    "{action:?} {caller:?}"
                );
            }
        }
    }

    #[test]
    fn the_platform_policy_restricts_loopback_where_callers_can_be_identified() {
        let restricted = cfg!(windows) || cfg!(target_os = "linux");
        assert_eq!(
            Policy::for_this_platform().loopback_is_administrator,
            !restricted
        );
        if restricted {
            for action in EVERY_ACTION {
                let expected = match action {
                    Action::ReadSettings | Action::ReadDiagnostics | Action::Pause => Ok(()),
                    _ => Err(RefusalReason::AdministratorRequired),
                };
                assert_eq!(
                    permits(Policy::for_this_platform(), action, &Caller::Loopback, None),
                    expected,
                    "{action:?}"
                );
            }
        }
    }

    #[test]
    fn unix_accounts_follow_the_same_rule() {
        // Linux identifies callers as `uid:<n>`; the rule does not care how
        // the account is spelled, only that the OS proved it.
        let alice = user("uid:1000");
        let bob = user("uid:1001");
        let root = admin("uid:0");
        let paused = PausedBy::from_caller(&alice);
        assert_eq!(paused, PausedBy::Account("uid:1000".into()));
        assert_eq!(permits(RESTRICTED, Action::Pause, &bob, None), Ok(()));
        assert_eq!(
            permits(RESTRICTED, Action::Resume, &alice, Some(&paused)),
            Ok(())
        );
        assert_eq!(
            permits(RESTRICTED, Action::Resume, &bob, Some(&paused)),
            Err(RefusalReason::AdministratorRequired)
        );
        assert_eq!(
            permits(RESTRICTED, Action::Resume, &root, Some(&paused)),
            Ok(())
        );
        for action in [
            Action::SetResource,
            Action::ReleaseWork,
            Action::SaveSettings,
            Action::Other,
        ] {
            assert_eq!(
                permits(RESTRICTED, action, &alice, Some(&paused)),
                Err(RefusalReason::AdministratorRequired),
                "{action:?}"
            );
            assert_eq!(
                permits(RESTRICTED, action, &root, None),
                Ok(()),
                "{action:?}"
            );
        }
        // A uid that merely looks like another account's SID is a different
        // account.
        assert_eq!(
            permits(
                RESTRICTED,
                Action::Resume,
                &user("S-1-5-21-1-1001"),
                Some(&paused)
            ),
            Err(RefusalReason::AdministratorRequired)
        );
    }

    fn settings() -> SettingsUpdate {
        serde_json::from_str::<SettingsSnapshot>(include_str!(
            "../../tests/fixtures/settings/default.json"
        ))
        .unwrap()
        .into()
    }

    #[test]
    fn requests_are_strict_and_round_trip() {
        for action in [
            RequestedAction::Pause {},
            RequestedAction::Resume {},
            RequestedAction::SetResource {
                resource: "gpu0".into(),
                enabled: false,
            },
            RequestedAction::ReleaseWork {
                id: 7,
                upstream_stopped: true,
            },
            RequestedAction::SaveSettings {
                settings: settings(),
            },
            RequestedAction::StartDebug { minutes: 30 },
            RequestedAction::EndDebug {},
        ] {
            let request = Request::new(action);
            let bytes = serde_json::to_vec(&request).unwrap();
            assert_eq!(serde_json::from_slice::<Request>(&bytes).unwrap(), request);
            // The service reads the settings payload as raw JSON.
            let raw: Request<serde_json::Value> = serde_json::from_slice(&bytes).unwrap();
            assert_eq!(raw.action.action(), request.action.action());
        }
        for invalid in [
            r#"{"protocol_version":1,"action":{"kind":"pause"},"extra":1}"#,
            r#"{"protocol_version":1,"action":{"kind":"pause","sid":"S-1-5-18"}}"#,
            r#"{"protocol_version":1,"action":{"kind":"format_disk"}}"#,
            r#"{"protocol_version":1,"action":{"kind":"start_debug"}}"#,
            r#"{"protocol_version":1,"action":{"kind":"start_debug","minutes":30,"level":"trace"}}"#,
            r#"{"protocol_version":1,"action":{"kind":"end_debug","minutes":30}}"#,
            r#"{"protocol_version":1}"#,
        ] {
            assert!(
                serde_json::from_str::<Request<serde_json::Value>>(invalid).is_err(),
                "{invalid}"
            );
        }
    }

    #[test]
    fn only_a_matching_response_acknowledges_a_change() {
        let pause = RequestedAction::<SettingsUpdate>::Pause {};
        let resume = RequestedAction::<SettingsUpdate>::Resume {};
        let off = Response::Lending {
            lending_enabled: false,
        };
        assert!(off.acknowledges(&pause));
        assert!(!off.acknowledges(&resume));
        let resource = RequestedAction::<SettingsUpdate>::SetResource {
            resource: "gpu0".into(),
            enabled: true,
        };
        assert!(Response::Resource {
            resource: "gpu0".into(),
            enabled: true
        }
        .acknowledges(&resource));
        assert!(!Response::Resource {
            resource: "gpu1".into(),
            enabled: true
        }
        .acknowledges(&resource));
        assert!(!Response::Released.acknowledges(&resource));
        assert!(!Response::Refused {
            reason: RefusalReason::AdministratorRequired
        }
        .acknowledges(&pause));

        let logging = |debug_until: Option<&str>| Response::Logging {
            logging: Logging {
                level: if debug_until.is_some() {
                    "debug"
                } else {
                    "info"
                }
                .into(),
                source: if debug_until.is_some() {
                    "debug_window"
                } else {
                    "config"
                }
                .into(),
                configured_level: "info".into(),
                environment_override: false,
                debug_until: debug_until.map(str::to_owned),
                debug_remaining_s: debug_until.map(|_| 1800),
            },
        };
        let start = RequestedAction::<SettingsUpdate>::StartDebug { minutes: 30 };
        let end = RequestedAction::<SettingsUpdate>::EndDebug {};
        let open = logging(Some("2026-10-07T05:00:00Z"));
        let closed = logging(None);
        assert!(open.acknowledges(&start));
        assert!(!open.acknowledges(&end));
        assert!(closed.acknowledges(&end));
        assert!(!closed.acknowledges(&start));
        assert!(!off.acknowledges(&start));
    }

    #[test]
    fn helper_exit_codes_are_distinct_and_round_trip() {
        let mut outcomes = vec![
            HelperExit::Applied,
            HelperExit::Unconfirmed,
            HelperExit::BadRequestFile,
        ];
        outcomes.extend(RefusalReason::ALL.map(HelperExit::Refused));
        let mut codes = std::collections::BTreeSet::new();
        for outcome in outcomes {
            assert!(codes.insert(outcome.code()), "{outcome:?}");
            assert_eq!(HelperExit::from_code(outcome.code()), Some(outcome));
        }
        // Crashes, clap usage errors and panics are not helper outcomes.
        for code in [1, 2, 19, 101, -1] {
            assert_eq!(HelperExit::from_code(code), None, "{code}");
        }
        assert_eq!(
            HelperExit::from_response(
                &RequestedAction::<SettingsUpdate>::Pause {},
                &Response::Refused {
                    reason: RefusalReason::StaleRevision
                }
            ),
            HelperExit::Refused(RefusalReason::StaleRevision)
        );
        assert_eq!(
            HelperExit::from_response(
                &RequestedAction::<SettingsUpdate>::Pause {},
                &Response::Lending {
                    lending_enabled: true
                }
            ),
            HelperExit::Unconfirmed
        );
    }

    #[tokio::test]
    async fn frames_are_bounded() {
        let mut reader = tokio::io::BufReader::new(&b"{\"a\":1}\r\nnext\n"[..]);
        assert_eq!(
            read_frame(&mut reader, 16).await.unwrap().unwrap(),
            b"{\"a\":1}"
        );
        assert_eq!(read_frame(&mut reader, 16).await.unwrap().unwrap(), b"next");
        assert!(read_frame(&mut reader, 16).await.unwrap().is_none());

        let oversized = vec![b'x'; 17];
        let mut reader = tokio::io::BufReader::new(oversized.as_slice());
        assert!(read_frame(&mut reader, 16).await.is_err());
        let mut exact = vec![b'x'; 16];
        exact.push(b'\n');
        let mut reader = tokio::io::BufReader::new(exact.as_slice());
        assert_eq!(
            read_frame(&mut reader, 16).await.unwrap().unwrap().len(),
            16
        );
        let mut incomplete = tokio::io::BufReader::new(&b"partial"[..]);
        assert!(read_frame(&mut incomplete, 16).await.is_err());
    }

    #[test]
    fn staged_request_files_are_bounded_and_strict() {
        let directory = std::env::temp_dir().join(format!(
            "vacancyd-admin-request-test-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        std::fs::create_dir(&directory).unwrap();
        let path = directory.join("request.json");
        let request = Request::new(RequestedAction::SaveSettings {
            settings: settings(),
        });
        std::fs::write(&path, serde_json::to_vec(&request).unwrap()).unwrap();
        assert_eq!(read_request_file(&path).unwrap(), request);

        let mut other_version = request.clone();
        other_version.protocol_version += 1;
        std::fs::write(&path, serde_json::to_vec(&other_version).unwrap()).unwrap();
        assert!(read_request_file(&path).is_err());

        std::fs::write(&path, vec![b' '; MAX_REQUEST_BYTES + 1]).unwrap();
        assert!(read_request_file(&path).is_err());
        assert!(read_request_file(&directory).is_err());
        assert!(read_request_file(&directory.join("missing.json")).is_err());
        std::fs::remove_dir_all(directory).unwrap();
    }
}
