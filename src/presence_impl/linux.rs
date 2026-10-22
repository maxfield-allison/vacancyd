//! Linux presence source: systemd-logind over the system D-Bus.
//!
//! The service reads `org.freedesktop.login1` itself, so no per-session
//! reporter is needed: one `ListSessions` call and one `GetAll` per session
//! each poll, over a connection kept open between polls. Which sessions count
//! and how their state maps onto the tiers lives in `logind_selection`.
//!
//! The configured owner processes are matched against the service's own
//! process scan, taken only while an unlocked session is in the foreground
//! (the only state in which an owner process changes the answer) and only
//! over the processes of that session's user.
//!
//! Not read on Linux: the foreground window, media playback and precise idle
//! time. Those need a per-session agent and are later parity work.

use std::collections::HashMap;
use std::time::Duration;

use sysinfo::{ProcessRefreshKind, ProcessesToUpdate, System, UpdateKind};
use zbus::{OwnedObjectPath, OwnedVariant};

use crate::plugin::presence::{PresenceOptions, PresenceSignal, PresenceSource, SessionAuthority};
use crate::presence_impl::logind_selection::{LogindSession, LogindTracker};
use crate::presence_impl::owner_processes::owner_match;

const LOGIN1: &str = "org.freedesktop.login1";
const MANAGER_PATH: &str = "/org/freedesktop/login1";
const MANAGER: &str = "org.freedesktop.login1.Manager";
const SESSION: &str = "org.freedesktop.login1.Session";
/// Longest one logind read may take before the sample fails closed. The
/// poll runs on a blocking worker, so this bounds how stale presence can get.
const CALL_TIMEOUT: Duration = Duration::from_secs(2);

pub struct LogindPresence {
    sessions: LogindTracker,
    bus: Option<zbus::Connection>,
    processes: System,
    /// Set once a read has failed, so an absent logind is reported at WARN
    /// once rather than every poll; cleared by the next good read.
    warned: bool,
    /// The same, for an owner-process scan that cannot see the user.
    warned_blind: bool,
}

impl LogindPresence {
    pub fn new(opts: PresenceOptions) -> Self {
        Self {
            sessions: LogindTracker::new(opts.sign_in_screen_is_presence),
            bus: None,
            processes: System::new(),
            warned: false,
            warned_blind: false,
        }
    }

    fn inventory(&mut self) -> Result<Vec<LogindSession>, String> {
        // The poll runs on Tokio's blocking pool, where blocking on the
        // daemon's own runtime is allowed; zbus's reader task runs there too.
        let handle = tokio::runtime::Handle::try_current()
            .map_err(|_| "no async runtime for the D-Bus client".to_string())?;
        let bus = self.bus.take();
        let result = handle.block_on(async move {
            let bus = match bus {
                Some(bus) => bus,
                None => tokio::time::timeout(
                    CALL_TIMEOUT,
                    zbus::connection::Builder::system()
                        .method_timeout(CALL_TIMEOUT)
                        .build(),
                )
                .await
                .map_err(|_| {
                    zbus::Error::Failure("connecting to the system bus timed out".into())
                })??,
            };
            let sessions = read_sessions(&bus).await;
            Ok::<_, zbus::Error>((bus, sessions))
        });
        match result {
            Ok((bus, Ok(sessions))) => {
                self.bus = Some(bus);
                Ok(sessions)
            }
            // Drop the connection after any failure; the next poll reconnects.
            Ok((_, Err(error))) | Err(error) => Err(error.to_string()),
        }
    }

    fn owner_app(
        &mut self,
        uids: &[u32],
        owner_processes: &[String],
    ) -> Result<Option<String>, String> {
        if owner_processes.is_empty() || uids.is_empty() {
            return Ok(None);
        }
        self.processes.refresh_processes_specifics(
            ProcessesToUpdate::All,
            true,
            ProcessRefreshKind::nothing()
                .with_exe(UpdateKind::Always)
                .with_cmd(UpdateKind::Always)
                .with_user(UpdateKind::OnlyIfNotSet),
        );
        scan_owner_processes(
            self.processes.processes().values().map(|process| {
                let exe = process.exe().and_then(|path| path.to_str());
                let arg0 = process.cmd().first().and_then(|arg| arg.to_str());
                (
                    process.user_id().map(|uid| **uid),
                    [process.name().to_str(), exe, arg0],
                )
            }),
            uids,
            owner_processes,
        )
    }
}

/// Match the owner list against the processes of the users in `uids`.
///
/// A user with a counted session always has processes of their own (a shell,
/// the desktop shell), so a scan that finds none of theirs cannot see them:
/// vacancyd is in a container without the host's PID namespace, or `/proc`
/// hides other users' processes. That is an error, never "no owner process",
/// because a blind scan would lend while a listed game runs.
fn scan_owner_processes<'a>(
    processes: impl Iterator<Item = (Option<u32>, [Option<&'a str>; 3])>,
    uids: &[u32],
    owner_processes: &[String],
) -> Result<Option<String>, String> {
    let mut visible = false;
    for (uid, names) in processes {
        // A process whose owner cannot be read is kept: an unreadable
        // owner must not hide a listed game.
        if uid.is_some_and(|uid| !uids.contains(&uid)) {
            continue;
        }
        visible |= uid.is_some();
        if let Some(app) = owner_match(names.into_iter().flatten(), owner_processes) {
            return Ok(Some(app));
        }
    }
    if visible {
        Ok(None)
    } else {
        Err(format!(
            "owner.processes cannot be checked: no process of uid {} is visible \
             (in a container, run it in the host PID namespace)",
            uids.iter()
                .map(u32::to_string)
                .collect::<Vec<_>>()
                .join(", ")
        ))
    }
}

async fn read_sessions(bus: &zbus::Connection) -> zbus::Result<Vec<LogindSession>> {
    let listed = bus
        .call_method(
            Some(LOGIN1),
            MANAGER_PATH,
            Some(MANAGER),
            "ListSessions",
            &(),
        )
        .await?;
    let listed: Vec<(String, u32, String, String, OwnedObjectPath)> =
        listed.body().deserialize()?;
    let mut sessions = Vec::with_capacity(listed.len());
    for (id, uid, _user, _seat, path) in listed {
        let reply = bus
            .call_method(
                Some(LOGIN1),
                path.as_str(),
                Some("org.freedesktop.DBus.Properties"),
                "GetAll",
                &(SESSION,),
            )
            .await;
        let reply = match reply {
            Ok(reply) => reply,
            // The session ended between the two calls. That is a logout, not
            // a failure: leaving it out of this read is exact.
            Err(error) if session_disappeared(&error) => continue,
            Err(error) => return Err(error),
        };
        let properties: HashMap<String, OwnedVariant> = reply.body().deserialize()?;
        sessions.push(session_from(id, uid, &properties)?);
    }
    Ok(sessions)
}

fn session_disappeared(error: &zbus::Error) -> bool {
    match error {
        zbus::Error::FDO(error) => matches!(
            error.as_ref(),
            zbus::fdo::Error::UnknownObject(_) | zbus::fdo::Error::UnknownInterface(_)
        ),
        zbus::Error::MethodError(name, _, _) => matches!(
            name.as_str(),
            "org.freedesktop.DBus.Error.UnknownObject"
                | "org.freedesktop.DBus.Error.UnknownInterface"
                | "org.freedesktop.login1.NoSuchSession"
        ),
        _ => false,
    }
}

fn session_from(
    id: String,
    uid: u32,
    properties: &HashMap<String, OwnedVariant>,
) -> zbus::Result<LogindSession> {
    fn get<T>(properties: &HashMap<String, OwnedVariant>, name: &str) -> zbus::Result<T>
    where
        T: TryFrom<OwnedVariant>,
        T::Error: Into<zbus::Error>,
    {
        let value = properties
            .get(name)
            .ok_or_else(|| zbus::Error::Failure(format!("logind session property {name} missing")))?
            .try_clone()?;
        T::try_from(value).map_err(Into::into)
    }
    let (seat, _): (String, OwnedObjectPath) = get(properties, "Seat")?;
    Ok(LogindSession {
        id,
        uid,
        class: get(properties, "Class")?,
        kind: get(properties, "Type")?,
        remote: get(properties, "Remote")?,
        seat,
        state: get(properties, "State")?,
        active: get(properties, "Active")?,
        locked_hint: get(properties, "LockedHint")?,
        idle_hint: get(properties, "IdleHint")?,
        idle_since_monotonic_us: get(properties, "IdleSinceHintMonotonic")?,
    })
}

/// CLOCK_MONOTONIC in microseconds, the clock logind's
/// `IdleSinceHintMonotonic` is stamped with.
fn monotonic_now_us() -> u64 {
    let mut now = libc::timespec {
        tv_sec: 0,
        tv_nsec: 0,
    };
    // SAFETY: `now` is a valid, writable timespec; CLOCK_MONOTONIC is always
    // available on Linux.
    let rc = unsafe { libc::clock_gettime(libc::CLOCK_MONOTONIC, &mut now) };
    if rc != 0 {
        // 0 makes every idle stamp look like the future: idle unknown.
        return 0;
    }
    (now.tv_sec as u64)
        .saturating_mul(1_000_000)
        .saturating_add(now.tv_nsec as u64 / 1_000)
}

impl PresenceSource for LogindPresence {
    fn kind(&self) -> &'static str {
        "logind"
    }

    fn sample(&mut self, owner_processes: &[String]) -> PresenceSignal {
        let inventory = self.inventory();
        match &inventory {
            Ok(_) => self.warned = false,
            Err(error) if !self.warned => {
                self.warned = true;
                tracing::warn!(
                    error = %error,
                    "systemd-logind is unavailable; lending is refused until it can be read (set owner.presence = \"headless\" on a host nobody sits at; in a container, docs/container.md lists what reading it needs)"
                );
            }
            Err(_) => {}
        }
        let view = self.sessions.reduce(inventory, monotonic_now_us());
        let mut signal = view.signal;
        match self.owner_app(&view.present_uids, owner_processes) {
            Ok(app) => {
                signal.owner_app = app;
                self.warned_blind = false;
            }
            Err(reason) => {
                if !self.warned_blind {
                    self.warned_blind = true;
                    tracing::warn!("{reason}; lending is refused while the session is unlocked");
                }
                // Refuses exactly as an unreadable session does.
                signal.session_authority = SessionAuthority::Ambiguous;
                signal.basis = Some(reason);
            }
        }
        signal
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn logind_properties_survive_the_dbus_wire_round_trip() {
        let seat = zbus::Structure::from((
            "seat0",
            OwnedObjectPath::try_from("/org/freedesktop/login1/seat/seat0").unwrap(),
        ));
        let properties: HashMap<String, OwnedVariant> = HashMap::from([
            ("Class".into(), zbus::Str::from("user").into()),
            ("Type".into(), zbus::Str::from("wayland").into()),
            ("Remote".into(), false.into()),
            ("Seat".into(), seat.try_into().unwrap()),
            ("State".into(), zbus::Str::from("active").into()),
            ("Active".into(), true.into()),
            ("LockedHint".into(), false.into()),
            ("IdleHint".into(), false.into()),
            ("IdleSinceHintMonotonic".into(), 100u64.into()),
        ]);
        let message = zbus::Message::method_call(MANAGER_PATH, "GetAll")
            .build(&properties)
            .unwrap();
        let mut decoded: HashMap<String, OwnedVariant> = message.body().deserialize().unwrap();
        let session = session_from("7".into(), 1000, &decoded).unwrap();
        assert_eq!(session.id, "7");
        assert_eq!(session.uid, 1000);
        assert_eq!(session.seat, "seat0");
        assert!(session.active);
        assert!(!session.locked_hint);
        assert_eq!(session.idle_since_monotonic_us, 100);
        decoded.remove("Active");
        assert!(session_from("7".into(), 1000, &decoded).is_err());
        decoded.insert("Active".into(), zbus::Str::from("true").into());
        assert!(session_from("7".into(), 1000, &decoded).is_err());
    }

    #[test]
    fn only_disappeared_session_errors_may_be_skipped() {
        assert!(session_disappeared(
            &zbus::fdo::Error::UnknownObject("gone".into()).into()
        ));
        assert!(session_disappeared(
            &zbus::fdo::Error::UnknownInterface("gone".into()).into()
        ));
        assert!(!session_disappeared(
            &zbus::fdo::Error::AccessDenied("denied".into()).into()
        ));
        assert!(!session_disappeared(&zbus::Error::Failure(
            "bus unavailable".into()
        )));
    }

    fn owners() -> Vec<String> {
        vec!["Marathon".to_string()]
    }

    #[test]
    fn a_listed_process_of_the_present_user_is_the_owner_app() {
        let processes = [
            (Some(0), [Some("systemd"), None, None]),
            (Some(1000), [Some("gnome-shell"), None, None]),
            (Some(1000), [Some("Marathon.x86_64"), None, None]),
        ];
        assert_eq!(
            scan_owner_processes(processes.into_iter(), &[1000], &owners()),
            Ok(Some("Marathon".to_string()))
        );
    }

    #[test]
    fn another_users_game_is_not_the_owner_app() {
        let processes = [
            (Some(1000), [Some("gnome-shell"), None, None]),
            (Some(1001), [Some("Marathon"), None, None]),
        ];
        assert_eq!(
            scan_owner_processes(processes.into_iter(), &[1000], &owners()),
            Ok(None)
        );
    }

    #[test]
    fn an_unreadable_owner_still_matches() {
        let processes = [(None, [Some("Marathon"), None, None])];
        assert_eq!(
            scan_owner_processes(processes.into_iter(), &[1000], &owners()),
            Ok(Some("Marathon".to_string()))
        );
    }

    #[test]
    fn a_scan_that_sees_none_of_the_users_processes_is_an_error() {
        // A container's own PID namespace: only vacancyd itself is visible.
        let processes = [(Some(65532), [Some("vacancyd"), None, None])];
        let error = scan_owner_processes(processes.into_iter(), &[1000, 1001], &owners())
            .expect_err("a blind scan must not report no owner process");
        assert!(error.contains("uid 1000, 1001"), "{error}");
        assert!(error.contains("host PID namespace"), "{error}");
        assert!(scan_owner_processes(std::iter::empty(), &[1000], &owners()).is_err());
    }
}
