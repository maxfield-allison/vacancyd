//! Platform-independent reduction of a systemd-logind session inventory.
//!
//! The D-Bus reads live in `linux`; keeping the counting policy here makes
//! the recorded logind shapes (a headless server with only SSH sessions, a
//! GNOME desktop, a greeter after logout, an unreachable logind) testable on
//! every CI runner, the same way `session_selection` does for Windows.
//!
//! Which sessions mean "someone is at the machine":
//!
//! | Session | Counts? |
//! | ------- | ------- |
//! | class `user` (or `user-*`), graphical type (`x11`, `wayland`, `mir`) | yes, local or remote |
//! | class `user` (or `user-*`), not remote, attached to a seat | yes (a local console login) |
//! | class `user`, remote and not graphical (SSH, `ssh -t`) | no |
//! | class `user`, not remote, no seat, not graphical (`su`, `machinectl shell`) | no |
//! | state `closing` (logged out, processes lingering) | no |
//! | class `greeter` | no, but an active one is the sign-in screen |
//! | class `manager`, `background`, `lock-screen` and any other class | no |

use std::collections::HashSet;

use crate::plugin::presence::{
    MediaState, PresenceSignal, SessionAuthority, SessionState, SessionView,
};

/// The subset of one `org.freedesktop.login1.Session` object the policy reads.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct LogindSession {
    pub id: String,
    pub uid: u32,
    pub class: String,
    /// logind `Type`: `x11`, `wayland`, `mir`, `tty`, `unspecified`, ...
    pub kind: String,
    pub remote: bool,
    /// Seat name, empty when the session has no seat.
    pub seat: String,
    /// logind `State`: `online`, `active` or `closing`.
    pub state: String,
    /// In the foreground of its seat (always true for a seatless session).
    pub active: bool,
    pub locked_hint: bool,
    pub idle_hint: bool,
    /// `IdleSinceHintMonotonic`, CLOCK_MONOTONIC microseconds; 0 when the
    /// session has never reported an idle hint.
    pub idle_since_monotonic_us: u64,
}

#[derive(Clone, Copy, Debug, Eq, Hash, Ord, PartialEq, PartialOrd)]
enum Disposition {
    /// A counted session in the foreground and unlocked.
    ActiveUnlocked,
    /// A counted session with its lock screen up.
    Locked,
    /// A counted session that is logged in but not in the foreground of its
    /// seat: the owner switched user or to another virtual terminal.
    Background,
    /// A greeter in the foreground of its seat: the sign-in screen.
    SignInScreen,
}

enum Role {
    Counted(String),
    Greeter,
    Ignored(String),
}

fn is_graphical(kind: &str) -> bool {
    matches!(kind, "x11" | "wayland" | "mir")
}

fn role(session: &LogindSession) -> Role {
    if session.state == "closing" {
        return Role::Ignored("closing: logged out, processes still running".into());
    }
    let user_class = session.class == "user" || session.class.starts_with("user-");
    if session.class == "greeter" {
        return Role::Greeter;
    }
    if !user_class {
        return Role::Ignored(format!("class {} is not a user login", session.class));
    }
    if is_graphical(&session.kind) {
        let place = if session.remote { "remote" } else { "local" };
        return Role::Counted(format!("{place} graphical session ({})", session.kind));
    }
    if !session.remote && !session.seat.is_empty() {
        return Role::Counted(format!("local console login on {}", session.seat));
    }
    if session.remote {
        Role::Ignored("remote non-graphical login (SSH)".into())
    } else {
        Role::Ignored("no seat and not graphical".into())
    }
}

/// What one reduction produced: the signal, plus the users whose unlocked
/// foreground sessions made it `Active`, for the owner-process scan.
pub struct LogindView {
    pub signal: PresenceSignal,
    pub present_uids: Vec<u32>,
}

#[derive(Default)]
pub struct LogindTracker {
    /// The previous complete view of counted sessions and greeters. A view in
    /// which nobody is active or locked must be read twice in a row before it
    /// lends, as on Windows, so a transition during sign-in, sign-out or a
    /// user switch cannot open lending.
    previous_view: Option<Vec<(String, Disposition)>>,
    /// `owner.sign_in_screen_is_presence`: keep an active greeter refusing.
    sign_in_screen_is_presence: bool,
}

impl LogindTracker {
    pub fn new(sign_in_screen_is_presence: bool) -> Self {
        Self {
            sign_in_screen_is_presence,
            ..Self::default()
        }
    }

    /// Reduce one complete logind inventory. `Err` is logind unreachable or a
    /// reply that could not be read, and refuses as an unknown session.
    /// `now_monotonic_us` is CLOCK_MONOTONIC, the clock logind stamps idle
    /// hints with.
    pub fn reduce(
        &mut self,
        inventory: Result<Vec<LogindSession>, String>,
        now_monotonic_us: u64,
    ) -> LogindView {
        let sessions = match inventory {
            Ok(sessions) => sessions,
            Err(error) => {
                return self.fail_closed(format!("systemd-logind unavailable: {error}"), Vec::new())
            }
        };
        let mut ids = HashSet::new();
        if let Some(duplicate) = sessions.iter().find(|s| !ids.insert(s.id.as_str())) {
            return self.fail_closed(
                format!(
                    "systemd-logind reply malformed: session {} listed twice",
                    duplicate.id
                ),
                Vec::new(),
            );
        }

        let mut views = Vec::with_capacity(sessions.len());
        let mut relevant = Vec::new();
        let mut unlocked = Vec::new();
        for session in &sessions {
            let (counted, reason, disposition) = match role(session) {
                Role::Ignored(reason) => (false, reason, None),
                Role::Greeter => {
                    let disposition = session.active.then_some(Disposition::SignInScreen);
                    let reason = if session.active {
                        "sign-in screen in the foreground".into()
                    } else {
                        "sign-in screen in the background".into()
                    };
                    (false, reason, disposition)
                }
                Role::Counted(reason) => {
                    if !matches!(session.state.as_str(), "active" | "online") {
                        return self.fail_closed(
                            format!(
                                "systemd-logind reply malformed: session {} has state {:?}",
                                session.id, session.state
                            ),
                            views,
                        );
                    }
                    let disposition = if session.locked_hint {
                        Disposition::Locked
                    } else if session.active {
                        unlocked.push(session);
                        Disposition::ActiveUnlocked
                    } else {
                        Disposition::Background
                    };
                    (true, reason, Some(disposition))
                }
            };
            if let Some(disposition) = disposition {
                relevant.push((session.id.clone(), disposition));
            }
            views.push(SessionView {
                id: session.id.clone(),
                class: session.class.clone(),
                kind: session.kind.clone(),
                remote: session.remote,
                seat: (!session.seat.is_empty()).then(|| session.seat.clone()),
                state: session.state.clone(),
                active: session.active,
                locked: session.locked_hint,
                counted,
                reason,
            });
        }
        relevant.sort();

        let dispositions = relevant.iter().map(|(_, d)| *d).collect::<HashSet<_>>();
        let sign_in = dispositions.contains(&Disposition::SignInScreen);
        if sign_in && self.sign_in_screen_is_presence {
            return self.fail_closed_active(
                "sign-in screen shown and owner.sign_in_screen_is_presence keeps it refusing"
                    .into(),
                views,
            );
        }

        let confirmed = self.previous_view.as_ref() == Some(&relevant);
        self.previous_view = Some(relevant);

        if !unlocked.is_empty() {
            let mut unlocked_ids = unlocked.iter().map(|s| s.id.as_str()).collect::<Vec<_>>();
            unlocked_ids.sort_unstable();
            let mut present_uids = unlocked.iter().map(|s| s.uid).collect::<Vec<_>>();
            present_uids.sort_unstable();
            present_uids.dedup();
            // The least idle session decides; one without a trustworthy
            // reading makes the whole answer unknown, never idle.
            let idle_seconds = unlocked
                .iter()
                .map(|s| idle_seconds(s, now_monotonic_us))
                .collect::<Option<Vec<_>>>()
                .and_then(|values| values.into_iter().min());
            let signal = PresenceSignal {
                session: SessionState::Active,
                idle_seconds,
                basis: Some(format!(
                    "logind: session {} active and unlocked",
                    unlocked_ids.join(", ")
                )),
                sessions: views,
                ..base_signal(SessionAuthority::NotRequired)
            };
            return LogindView {
                signal,
                present_uids,
            };
        }

        if dispositions.contains(&Disposition::Locked) {
            let signal = PresenceSignal {
                session: SessionState::Locked,
                basis: Some(
                    "logind: no unlocked session in the foreground; a counted session is locked"
                        .into(),
                ),
                sessions: views,
                ..base_signal(SessionAuthority::NotRequired)
            };
            return LogindView {
                signal,
                present_uids: Vec::new(),
            };
        }

        // Nobody is active or locked. That can be a moment inside sign-in,
        // sign-out or a user switch, so it lends only once the identical view
        // has been read twice in a row. Until then the session is unknown,
        // not active: nobody was seen at the machine, and a console must not
        // show someone at the desk.
        if !confirmed {
            return LogindView {
                signal: PresenceSignal {
                    session: SessionState::Unknown,
                    basis: Some(
                        "logind: session view not yet confirmed by a second identical read".into(),
                    ),
                    sessions: views,
                    ..base_signal(SessionAuthority::Ambiguous)
                },
                present_uids: Vec::new(),
            };
        }
        let (session, basis) = if dispositions.contains(&Disposition::Background) {
            (
                SessionState::Disconnected,
                "logind: a counted session is logged in but not in the foreground",
            )
        } else if sign_in {
            (
                SessionState::LoggedOut,
                "logind: sign-in screen with nobody signed in",
            )
        } else {
            (
                SessionState::LoggedOut,
                "logind: no local or graphical user session",
            )
        };
        LogindView {
            signal: PresenceSignal {
                session,
                basis: Some(basis.into()),
                sessions: views,
                ..base_signal(SessionAuthority::NotRequired)
            },
            present_uids: Vec::new(),
        }
    }

    /// logind could not be read: an unknown session that refuses.
    fn fail_closed(&mut self, basis: String, sessions: Vec<SessionView>) -> LogindView {
        self.previous_view = None;
        LogindView {
            signal: PresenceSignal {
                session: SessionState::Unknown,
                basis: Some(basis),
                sessions,
                ..base_signal(SessionAuthority::Ambiguous)
            },
            present_uids: Vec::new(),
        }
    }

    /// logind was read and the policy keeps it refusing, as on Windows.
    fn fail_closed_active(&mut self, basis: String, sessions: Vec<SessionView>) -> LogindView {
        self.previous_view = None;
        LogindView {
            signal: PresenceSignal {
                session: SessionState::Active,
                basis: Some(basis),
                sessions,
                ..base_signal(SessionAuthority::Ambiguous)
            },
            present_uids: Vec::new(),
        }
    }
}

fn base_signal(session_authority: SessionAuthority) -> PresenceSignal {
    PresenceSignal {
        session_authority,
        // No media backend on Linux: absent rather than failing, as in the
        // fallback source.
        media: MediaState::NotChecked,
        ..PresenceSignal::default()
    }
}

/// Seconds without input, only from a reading the desktop actually manages.
///
/// A desktop that sets `IdleHint` stamps `IdleSinceHintMonotonic` whenever the
/// hint changes. `IdleHint = true` with a stamp gives the time since the
/// session went idle, which undercounts the time since the last input by the
/// desktop's own idle delay, never overcounts it. `IdleHint = false` with a
/// stamp is a managed "not idle". A zero stamp means the hint has never been
/// set, so idle is unknown and cannot lend.
fn idle_seconds(session: &LogindSession, now_monotonic_us: u64) -> Option<u64> {
    let since = session.idle_since_monotonic_us;
    if since == 0 || since > now_monotonic_us {
        return None;
    }
    if !session.idle_hint {
        return Some(0);
    }
    Some((now_monotonic_us - since) / 1_000_000)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::plugin::presence::owner_present;

    const NOW: u64 = 5_000_000_000; // 5000 s of uptime, in microseconds

    fn ssh(id: &str) -> LogindSession {
        LogindSession {
            id: id.into(),
            uid: 1001,
            class: "user".into(),
            kind: "tty".into(),
            remote: true,
            seat: String::new(),
            state: "active".into(),
            active: true,
            locked_hint: false,
            idle_hint: false,
            idle_since_monotonic_us: 0,
        }
    }

    /// The `user@.service` manager session every login creates: local and
    /// active, which is exactly why class decides first.
    fn manager(id: &str) -> LogindSession {
        LogindSession {
            class: "manager".into(),
            kind: "unspecified".into(),
            remote: false,
            ..ssh(id)
        }
    }

    fn gnome(id: &str) -> LogindSession {
        LogindSession {
            id: id.into(),
            uid: 1000,
            class: "user".into(),
            kind: "wayland".into(),
            remote: false,
            seat: "seat0".into(),
            state: "active".into(),
            active: true,
            locked_hint: false,
            idle_hint: false,
            idle_since_monotonic_us: NOW - 60_000_000,
        }
    }

    fn greeter(id: &str, active: bool) -> LogindSession {
        LogindSession {
            uid: 120,
            class: "greeter".into(),
            state: if active { "active" } else { "online" }.into(),
            active,
            idle_since_monotonic_us: 0,
            ..gnome(id)
        }
    }

    fn lends(view: &LogindView) -> bool {
        owner_present(&view.signal, true, 0).is_none()
    }

    fn lends_idle(view: &LogindView, after_s: u64) -> bool {
        owner_present(&view.signal, true, after_s).is_none()
    }

    #[test]
    fn headless_server_with_only_ssh_sessions_lends_after_two_reads() {
        // Recorded on a headless Debian 13 host: one manager session and
        // several SSH sessions, two of them closing.
        let inventory = || {
            let mut closing = ssh("31");
            closing.state = "closing".into();
            Ok(vec![manager("1"), closing, ssh("634"), ssh("951")])
        };
        let mut tracker = LogindTracker::default();
        let first = tracker.reduce(inventory(), NOW);
        assert_eq!(first.signal.session_authority, SessionAuthority::Ambiguous);
        assert!(!lends(&first));
        let second = tracker.reduce(inventory(), NOW + 3_000_000);
        assert_eq!(second.signal.session, SessionState::LoggedOut);
        assert_eq!(
            second.signal.session_authority,
            SessionAuthority::NotRequired
        );
        assert!(lends(&second));
        assert_eq!(second.signal.sessions.len(), 4);
        assert!(second.signal.sessions.iter().all(|s| !s.counted));
    }

    #[test]
    fn live_61_an_unconfirmed_view_is_unknown_not_active() {
        // Before the first confirmed read, and again right after logind
        // returns, the view is unconfirmed: it refuses, and it must not
        // report someone at the desk while doing so.
        let mut tracker = LogindTracker::default();
        let first = tracker.reduce(Ok(vec![greeter("c1", true)]), NOW);
        assert_eq!(first.signal.session, SessionState::Unknown);
        assert_eq!(first.signal.session_authority, SessionAuthority::Ambiguous);
        let reason = owner_present(&first.signal, false, 0).unwrap();
        assert!(reason.contains("not yet confirmed"), "{reason}");
        assert_eq!(
            tracker
                .reduce(Ok(vec![greeter("c1", true)]), NOW + 1)
                .signal
                .session,
            SessionState::LoggedOut
        );
        tracker.reduce(Err("connection refused".into()), NOW + 2);
        let returned = tracker.reduce(Ok(vec![greeter("c1", true)]), NOW + 3);
        assert_eq!(returned.signal.session, SessionState::Unknown);
        assert!(!lends(&returned));
        assert_eq!(
            tracker
                .reduce(Ok(vec![greeter("c1", true)]), NOW + 4)
                .signal
                .session,
            SessionState::LoggedOut
        );
    }

    #[test]
    fn ssh_sessions_coming_and_going_do_not_restart_confirmation() {
        let mut tracker = LogindTracker::default();
        tracker.reduce(Ok(vec![ssh("7")]), NOW);
        let churned = tracker.reduce(Ok(vec![ssh("7"), ssh("8")]), NOW + 1);
        assert_eq!(churned.signal.session, SessionState::LoggedOut);
        let fewer = tracker.reduce(Ok(vec![ssh("8")]), NOW + 2);
        assert!(lends(&fewer));
    }

    #[test]
    fn active_unlocked_gnome_session_vetoes_without_a_reporter() {
        let view = LogindTracker::default().reduce(Ok(vec![gnome("2"), ssh("9")]), NOW);
        assert_eq!(view.signal.session, SessionState::Active);
        assert_eq!(view.signal.session_authority, SessionAuthority::NotRequired);
        assert_eq!(view.present_uids, vec![1000]);
        assert_eq!(
            owner_present(&view.signal, true, 0).as_deref(),
            Some("session unlocked")
        );
    }

    #[test]
    fn locked_session_lends_without_waiting_for_confirmation() {
        let mut locked = gnome("2");
        locked.locked_hint = true;
        let view = LogindTracker::default().reduce(Ok(vec![locked]), NOW);
        assert_eq!(view.signal.session, SessionState::Locked);
        assert!(lends(&view));
    }

    #[test]
    fn idle_hint_with_a_stamp_past_the_threshold_lends_on_idle() {
        let mut idle = gnome("2");
        idle.idle_hint = true;
        idle.idle_since_monotonic_us = NOW - 900_000_000;
        let view = LogindTracker::default().reduce(Ok(vec![idle]), NOW);
        assert_eq!(view.signal.session, SessionState::Active);
        assert_eq!(view.signal.idle_seconds, Some(900));
        assert!(lends_idle(&view, 600));
        assert!(!lends_idle(&view, 1_200));
        assert!(!lends_idle(&view, 0));
    }

    #[test]
    fn a_desktop_that_never_sets_idle_hint_never_lends_on_idle() {
        // IdleSinceHint 0: the desktop does not manage the hint at all.
        let mut unmanaged = gnome("2");
        unmanaged.idle_since_monotonic_us = 0;
        let view = LogindTracker::default().reduce(Ok(vec![unmanaged.clone()]), NOW);
        assert_eq!(view.signal.idle_seconds, None);
        assert!(!lends_idle(&view, 1));
        // An idle hint without a stamp, or a stamp from the future, is no
        // better.
        unmanaged.idle_hint = true;
        let view = LogindTracker::default().reduce(Ok(vec![unmanaged.clone()]), NOW);
        assert_eq!(view.signal.idle_seconds, None);
        unmanaged.idle_since_monotonic_us = NOW + 1;
        let view = LogindTracker::default().reduce(Ok(vec![unmanaged]), NOW);
        assert_eq!(view.signal.idle_seconds, None);
        assert!(!lends_idle(&view, 1));
    }

    #[test]
    fn a_managed_not_idle_hint_reads_as_zero() {
        let view = LogindTracker::default().reduce(Ok(vec![gnome("2")]), NOW);
        assert_eq!(view.signal.idle_seconds, Some(0));
    }

    #[test]
    fn one_session_without_an_idle_reading_keeps_every_session_present() {
        let mut idle = gnome("2");
        idle.idle_hint = true;
        idle.idle_since_monotonic_us = NOW - 900_000_000;
        let mut unmanaged = gnome("3");
        unmanaged.seat = "seat1".into();
        unmanaged.idle_since_monotonic_us = 0;
        let view = LogindTracker::default().reduce(Ok(vec![idle, unmanaged]), NOW);
        assert_eq!(view.signal.idle_seconds, None);
        assert!(!lends_idle(&view, 600));
    }

    #[test]
    fn greeter_only_is_logged_out_after_two_reads() {
        let mut tracker = LogindTracker::default();
        let first = tracker.reduce(Ok(vec![greeter("c1", true)]), NOW);
        assert!(!lends(&first));
        let second = tracker.reduce(Ok(vec![greeter("c1", true)]), NOW + 1);
        assert_eq!(second.signal.session, SessionState::LoggedOut);
        assert!(lends(&second));
    }

    #[test]
    fn sign_in_screen_setting_keeps_an_active_greeter_refusing() {
        let mut tracker = LogindTracker::new(true);
        for offset in 0..3 {
            let view = tracker.reduce(Ok(vec![greeter("c1", true)]), NOW + offset);
            assert_eq!(view.signal.session_authority, SessionAuthority::Ambiguous);
            assert!(!lends(&view));
            assert!(owner_present(&view.signal, true, 0)
                .unwrap()
                .contains("sign_in_screen_is_presence"));
        }
        // A greeter in the background is not the sign-in screen, and nobody
        // signed in still lends once confirmed.
        tracker.reduce(Ok(vec![greeter("c1", false)]), NOW + 4);
        let view = tracker.reduce(Ok(vec![greeter("c1", false)]), NOW + 5);
        assert_eq!(view.signal.session, SessionState::LoggedOut);
        let mut locked = gnome("2");
        locked.locked_hint = true;
        let view = tracker.reduce(Ok(vec![locked]), NOW + 6);
        assert_eq!(view.signal.session, SessionState::Locked);
    }

    #[test]
    fn switching_user_to_the_greeter_is_disconnected_after_two_reads() {
        let mut away = gnome("2");
        away.active = false;
        away.state = "online".into();
        let view = || Ok(vec![away.clone(), greeter("c3", true)]);
        let mut tracker = LogindTracker::default();
        assert!(!lends(&tracker.reduce(view(), NOW)));
        let second = tracker.reduce(view(), NOW + 1);
        assert_eq!(second.signal.session, SessionState::Disconnected);
        assert!(lends(&second));
    }

    #[test]
    fn another_users_unlocked_session_governs_over_a_background_one() {
        let mut away = gnome("2");
        away.active = false;
        away.state = "online".into();
        let mut other = gnome("5");
        other.uid = 1002;
        let view = LogindTracker::default().reduce(Ok(vec![away, other]), NOW);
        assert_eq!(view.signal.session, SessionState::Active);
        assert_eq!(view.present_uids, vec![1002]);
    }

    #[test]
    fn local_console_counts_and_seatless_local_sessions_do_not() {
        let mut console = ssh("4");
        console.remote = false;
        console.seat = "seat0".into();
        let view = LogindTracker::default().reduce(Ok(vec![console.clone()]), NOW);
        assert_eq!(view.signal.session, SessionState::Active);

        console.seat = String::new();
        let mut tracker = LogindTracker::default();
        tracker.reduce(Ok(vec![console.clone()]), NOW);
        assert_eq!(
            tracker.reduce(Ok(vec![console]), NOW + 1).signal.session,
            SessionState::LoggedOut
        );
    }

    #[test]
    fn only_user_classes_count_even_on_a_seat() {
        for class in ["manager", "background", "lock-screen", "none"] {
            let mut session = gnome("2");
            session.class = class.into();
            let mut tracker = LogindTracker::default();
            let first = tracker.reduce(Ok(vec![session.clone()]), NOW);
            assert_eq!(first.signal.session_authority, SessionAuthority::Ambiguous);
            let second = tracker.reduce(Ok(vec![session]), NOW + 1);
            assert_eq!(second.signal.session, SessionState::LoggedOut, "{class}");
            assert!(!second.signal.sessions[0].counted);
        }
        for class in ["user-early", "user-incomplete"] {
            let mut session = gnome("2");
            session.class = class.into();
            let view = LogindTracker::default().reduce(Ok(vec![session]), NOW);
            assert_eq!(view.signal.session, SessionState::Active, "{class}");
        }
    }

    #[test]
    fn remote_graphical_session_counts_like_rdp_on_windows() {
        let mut xrdp = gnome("6");
        xrdp.remote = true;
        xrdp.seat = String::new();
        xrdp.kind = "x11".into();
        let view = LogindTracker::default().reduce(Ok(vec![xrdp]), NOW);
        assert_eq!(view.signal.session, SessionState::Active);
    }

    #[test]
    fn logind_unreachable_is_unknown_and_vetoes() {
        let mut tracker = LogindTracker::default();
        tracker.reduce(Ok(Vec::new()), NOW);
        assert!(lends(&tracker.reduce(Ok(Vec::new()), NOW + 1)));
        let failed = tracker.reduce(
            Err("org.freedesktop.DBus.Error.ServiceUnknown".into()),
            NOW + 2,
        );
        assert_eq!(failed.signal.session, SessionState::Unknown);
        assert_eq!(failed.signal.session_authority, SessionAuthority::Ambiguous);
        let reason = owner_present(&failed.signal, false, 0).unwrap();
        assert!(reason.contains("systemd-logind"), "{reason}");
        // A failure restarts confirmation.
        assert!(!lends(&tracker.reduce(Ok(Vec::new()), NOW + 3)));
    }

    #[test]
    fn malformed_replies_fail_closed() {
        let mut bad_state = gnome("2");
        bad_state.state = "zombie".into();
        for inventory in [vec![ssh("1"), ssh("1")], vec![bad_state]] {
            let view = LogindTracker::default().reduce(Ok(inventory), NOW);
            assert_eq!(view.signal.session, SessionState::Unknown);
            assert!(!lends(&view));
        }
    }
}
