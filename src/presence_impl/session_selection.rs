//! Platform-independent reduction of Windows session inventory.
//!
//! Raw WTS calls live in `windows`; keeping the topology policy here makes
//! fast switching, RDP, ambiguity, the sign-in screen, and logon-epoch
//! handling testable on every CI runner.

use std::collections::{HashMap, HashSet};
use std::time::Instant;

use crate::plugin::presence::{
    MediaState, PresenceSignal, SessionAuthority, SessionRequirement, SessionState,
};

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum WtsDisposition {
    ActiveUnlocked,
    ActiveLocked,
    Disconnected,
    /// A connected session with no signed-in user: the sign-in screen after a
    /// reboot or sign-out, or the Switch user screen. Nobody is at a desktop.
    SignInScreen,
    /// A successfully queried WTSDown session with no signed-in user.
    /// Windows can retain this entry after sign-out alongside the console.
    UnownedDown,
    Ambiguous,
}

/// Classify a successfully queried, consistent WTSINFOEX session. Numeric
/// states and lock flags are the stable Windows API values. Query failures,
/// truncated replies and mismatched IDs/states must remain ambiguous before
/// reaching this function.
pub(super) fn classify_session(state: i32, has_user: bool, flags: i32) -> WtsDisposition {
    if !has_user {
        return match state {
            1 => WtsDisposition::SignInScreen, // WTSConnected
            8 => WtsDisposition::UnownedDown,  // WTSDown
            _ => WtsDisposition::Ambiguous,
        };
    }
    match (state, flags) {
        (0, 1) => WtsDisposition::ActiveUnlocked,
        (0, 0) => WtsDisposition::ActiveLocked,
        (4, _) => WtsDisposition::Disconnected,
        _ => WtsDisposition::Ambiguous,
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct WtsSession {
    pub id: u32,
    pub logon_time: i64,
    pub disposition: WtsDisposition,
}

#[derive(Clone, Copy, Debug)]
struct SeenSession {
    disposition: WtsDisposition,
    fresh_after: Instant,
}

#[derive(Default)]
pub struct SessionTracker {
    seen: HashMap<(u32, i64), SeenSession>,
    /// The previous complete, consistent inventory. A view with nobody signed
    /// in to an active session must be seen twice in a row before it lends.
    previous_view: Option<Vec<(u32, i64, WtsDisposition)>>,
    /// `owner.sign_in_screen_is_presence`: keep the sign-in screen refusing,
    /// as an unresolved session, instead of treating it as nobody present.
    sign_in_screen_is_presence: bool,
}

impl SessionTracker {
    pub fn new(sign_in_screen_is_presence: bool) -> Self {
        Self {
            sign_in_screen_is_presence,
            ..Self::default()
        }
    }

    /// Reduce one complete WTS inventory into the base presence observation.
    ///
    /// `Err` means the inventory itself could not be trusted. That, an
    /// inconsistent inventory and a view not yet confirmed are all emitted as
    /// an unknown, ambiguous session: it refuses, so Windows never turns an
    /// unreadable desktop into permission to lend, and it does not claim that
    /// someone is at the desk either.
    pub fn reduce(
        &mut self,
        inventory: anyhow::Result<Vec<WtsSession>>,
        now: Instant,
    ) -> PresenceSignal {
        let mut sessions = match inventory {
            Ok(sessions) => sessions,
            Err(error) => {
                return self.fail_closed(
                    SessionState::Unknown,
                    None,
                    format!("Windows session inventory could not be read: {error:#}"),
                )
            }
        };
        sessions.sort_by_key(|session| session.id);
        let first_id = sessions.first().map(|session| session.id);

        let mut ids = HashSet::new();
        if sessions.iter().any(|session| {
            !ids.insert(session.id)
                || (session.logon_time <= 0
                    && session.disposition == WtsDisposition::ActiveUnlocked)
                || session.disposition == WtsDisposition::Ambiguous
        }) {
            return self.fail_closed(
                SessionState::Unknown,
                first_id,
                "Windows session inventory is inconsistent or in transition".into(),
            );
        }
        if self.sign_in_screen_is_presence
            && sessions
                .iter()
                .any(|session| session.disposition == WtsDisposition::SignInScreen)
        {
            // The operator's policy, not missing evidence: the sign-in screen
            // is treated as someone at the machine.
            return self.fail_closed(
                SessionState::Active,
                first_id,
                "sign-in screen shown and owner.sign_in_screen_is_presence keeps it refusing"
                    .into(),
            );
        }

        let view = sessions
            .iter()
            .map(|session| (session.id, session.logon_time, session.disposition))
            .collect::<Vec<_>>();
        let confirmed = self.previous_view.as_ref() == Some(&view);
        self.previous_view = Some(view);

        let mut next = HashMap::new();
        let mut requirements = Vec::new();
        let mut saw_locked = false;
        let mut saw_disconnected = false;
        let mut saw_sign_in = false;
        let mut saw_unowned_down = false;

        for session in &sessions {
            let key = (session.id, session.logon_time);
            let fresh_after = self
                .seen
                .get(&key)
                .filter(|seen| seen.disposition == WtsDisposition::ActiveUnlocked)
                .map(|seen| seen.fresh_after)
                .unwrap_or(now);
            next.insert(
                key,
                SeenSession {
                    disposition: session.disposition,
                    fresh_after,
                },
            );
            match session.disposition {
                WtsDisposition::ActiveUnlocked => requirements.push(SessionRequirement {
                    session_id: session.id,
                    fresh_after,
                }),
                WtsDisposition::ActiveLocked => saw_locked = true,
                WtsDisposition::Disconnected => saw_disconnected = true,
                WtsDisposition::SignInScreen => saw_sign_in = true,
                WtsDisposition::UnownedDown => saw_unowned_down = true,
                WtsDisposition::Ambiguous => unreachable!("handled above"),
            }
        }
        self.seen = next;

        // Nobody signed in to an active session, and the sign-in screen (or no
        // session at all, or unowned Down residue) is part of the picture.
        // One sample can be a transition during sign-in, sign-out, or user
        // switching, so lending needs two identical consecutive reads.
        if requirements.is_empty()
            && (sessions.is_empty() || saw_sign_in || saw_unowned_down)
            && !confirmed
        {
            return unresolved_signal(
                SessionState::Unknown,
                sessions.first().map(|session| session.id),
                "Windows session view not yet confirmed by a second identical read".into(),
            );
        }

        requirements.sort_by_key(|requirement| requirement.session_id);
        let applicable_session_ids = requirements
            .iter()
            .map(|requirement| requirement.session_id)
            .collect::<Vec<_>>();
        let session = if !requirements.is_empty() {
            SessionState::Active
        } else if saw_locked {
            SessionState::Locked
        } else if saw_disconnected {
            SessionState::Disconnected
        } else {
            SessionState::LoggedOut
        };
        let session_authority = if requirements.is_empty() {
            SessionAuthority::NotRequired
        } else {
            SessionAuthority::Missing
        };

        PresenceSignal {
            session,
            session_id: applicable_session_ids.first().copied(),
            applicable_session_ids,
            session_authority,
            session_requirements: requirements,
            foreground: None,
            owner_app: None,
            idle_seconds: None,
            media: MediaState::NotChecked,
            basis: None,
            sessions: Vec::new(),
        }
    }

    fn fail_closed(
        &mut self,
        session: SessionState,
        observed_session_id: Option<u32>,
        basis: String,
    ) -> PresenceSignal {
        self.seen.clear();
        self.previous_view = None;
        unresolved_signal(session, observed_session_id, basis)
    }
}

/// A refusing observation with no applicable session to report on: `basis`
/// says why, and becomes the veto cause.
fn unresolved_signal(
    session: SessionState,
    observed_session_id: Option<u32>,
    basis: String,
) -> PresenceSignal {
    PresenceSignal {
        session,
        session_id: observed_session_id,
        applicable_session_ids: Vec::new(),
        session_authority: SessionAuthority::Ambiguous,
        session_requirements: Vec::new(),
        foreground: None,
        owner_app: None,
        idle_seconds: None,
        media: MediaState::NotChecked,
        basis: Some(basis),
        sessions: Vec::new(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;

    fn session(id: u32, logon_time: i64, disposition: WtsDisposition) -> WtsSession {
        WtsSession {
            id,
            logon_time,
            disposition,
        }
    }

    #[test]
    fn empty_inventory_is_logged_out() {
        let mut tracker = SessionTracker::default();
        let first = tracker.reduce(Ok(Vec::new()), Instant::now());
        assert_eq!(first.session, SessionState::Unknown);
        assert_eq!(first.session_authority, SessionAuthority::Ambiguous);
        let signal = tracker.reduce(Ok(Vec::new()), Instant::now());
        assert_eq!(signal.session, SessionState::LoggedOut);
        assert_eq!(signal.session_authority, SessionAuthority::NotRequired);
    }

    #[test]
    fn one_empty_poll_between_active_samples_never_claims_logout() {
        let mut tracker = SessionTracker::default();
        let now = Instant::now();
        tracker.reduce(
            Ok(vec![session(7, 70, WtsDisposition::ActiveUnlocked)]),
            now,
        );
        let empty = tracker.reduce(Ok(Vec::new()), now + Duration::from_secs(1));
        assert_eq!(empty.session_authority, SessionAuthority::Ambiguous);
        let active = tracker.reduce(
            Ok(vec![session(7, 70, WtsDisposition::ActiveUnlocked)]),
            now + Duration::from_secs(2),
        );
        assert_eq!(
            active.session_requirements[0].fresh_after,
            now + Duration::from_secs(2)
        );
    }

    #[test]
    fn active_sessions_are_sorted_and_all_required() {
        let now = Instant::now();
        let signal = SessionTracker::default().reduce(
            Ok(vec![
                session(8, 80, WtsDisposition::ActiveUnlocked),
                session(3, 30, WtsDisposition::Disconnected),
                session(7, 70, WtsDisposition::ActiveUnlocked),
            ]),
            now,
        );
        assert_eq!(signal.session, SessionState::Active);
        assert_eq!(signal.applicable_session_ids, vec![7, 8]);
        assert_eq!(signal.session_authority, SessionAuthority::Missing);
        assert!(signal
            .session_requirements
            .iter()
            .all(|requirement| requirement.fresh_after == now));
    }

    #[test]
    fn locked_precedes_disconnected_when_nobody_is_active() {
        let signal = SessionTracker::default().reduce(
            Ok(vec![
                session(3, 30, WtsDisposition::Disconnected),
                session(7, 70, WtsDisposition::ActiveLocked),
            ]),
            Instant::now(),
        );
        assert_eq!(signal.session, SessionState::Locked);
    }

    #[test]
    fn unlock_and_reused_session_id_require_new_reports() {
        let mut tracker = SessionTracker::default();
        let first = Instant::now();
        tracker.reduce(
            Ok(vec![session(7, 70, WtsDisposition::ActiveUnlocked)]),
            first,
        );
        let steady = tracker.reduce(
            Ok(vec![session(7, 70, WtsDisposition::ActiveUnlocked)]),
            first + Duration::from_secs(1),
        );
        assert_eq!(steady.session_requirements[0].fresh_after, first);

        tracker.reduce(
            Ok(vec![session(7, 70, WtsDisposition::ActiveLocked)]),
            first + Duration::from_secs(2),
        );
        let unlocked = tracker.reduce(
            Ok(vec![session(7, 70, WtsDisposition::ActiveUnlocked)]),
            first + Duration::from_secs(3),
        );
        assert_eq!(
            unlocked.session_requirements[0].fresh_after,
            first + Duration::from_secs(3)
        );

        let reused = tracker.reduce(
            Ok(vec![session(7, 71, WtsDisposition::ActiveUnlocked)]),
            first + Duration::from_secs(4),
        );
        assert_eq!(
            reused.session_requirements[0].fresh_after,
            first + Duration::from_secs(4)
        );
    }

    fn lends(signal: &PresenceSignal) -> bool {
        crate::plugin::presence::owner_present(signal, true, 0).is_none()
    }

    fn sign_in(id: u32) -> WtsSession {
        session(id, 0, WtsDisposition::SignInScreen)
    }

    #[test]
    fn signed_out_console_with_userless_down_session_lends_after_confirmation() {
        let mut tracker = SessionTracker::default();
        let now = Instant::now();
        // The observed sign-out topology: a successfully queried, user-less
        // WTSDown session 2 and a user-less WTSConnected console session 3.
        let inventory = vec![
            session(2, 0, classify_session(8, false, 0)),
            session(3, 0, classify_session(1, false, 0)),
        ];
        let first = tracker.reduce(Ok(inventory.clone()), now);
        assert!(!lends(&first));

        let confirmed = tracker.reduce(Ok(inventory), now + Duration::from_secs(1));
        assert_eq!(confirmed.session, SessionState::LoggedOut);
        assert_eq!(confirmed.session_authority, SessionAuthority::NotRequired);
        assert!(confirmed.applicable_session_ids.is_empty());
        assert!(lends(&confirmed));
    }

    #[test]
    fn unowned_down_requires_confirmation_even_without_a_console() {
        let mut tracker = SessionTracker::default();
        let now = Instant::now();
        let inventory = vec![session(2, 0, classify_session(8, false, 0))];
        assert!(!lends(&tracker.reduce(Ok(inventory.clone()), now)));
        assert!(lends(&tracker.reduce(Ok(inventory.clone()), now)));
        assert!(!lends(
            &tracker.reduce(Err(anyhow::anyhow!("WTS read failed")), now)
        ));
        assert!(!lends(&tracker.reduce(Ok(inventory.clone()), now)));
        assert!(lends(&tracker.reduce(Ok(inventory), now)));
    }

    #[test]
    fn unowned_down_does_not_hide_an_active_console_or_rdp_owner() {
        let mut tracker = SessionTracker::default();
        let now = Instant::now();
        let down = session(2, 0, classify_session(8, false, 0));
        let signed_out = vec![down.clone(), sign_in(3)];
        tracker.reduce(Ok(signed_out.clone()), now);
        assert!(lends(&tracker.reduce(Ok(signed_out), now)));

        let owner = tracker.reduce(
            Ok(vec![
                down.clone(),
                session(3, 30, classify_session(0, true, 1)),
            ]),
            now,
        );
        assert_eq!(owner.session, SessionState::Active);
        assert_eq!(owner.applicable_session_ids, vec![3]);
        assert_eq!(owner.session_authority, SessionAuthority::Missing);
        assert!(!lends(&owner));

        let rdp = tracker.reduce(
            Ok(vec![
                down,
                sign_in(3),
                session(7, 70, classify_session(0, true, 1)),
            ]),
            now,
        );
        assert_eq!(rdp.applicable_session_ids, vec![7]);
        assert_eq!(rdp.session_authority, SessionAuthority::Missing);
        assert!(!lends(&rdp));
    }

    #[test]
    fn down_with_a_user_and_other_userless_states_remain_ambiguous() {
        assert_eq!(classify_session(8, true, 0), WtsDisposition::Ambiguous);
        for state in [0, 2, 3, 4, 5, 6, 7, 9, -1, 10] {
            assert_eq!(classify_session(state, false, 0), WtsDisposition::Ambiguous);
        }
        assert_eq!(classify_session(0, true, -1), WtsDisposition::Ambiguous);
        assert_eq!(classify_session(0, true, 0), WtsDisposition::ActiveLocked);
        assert_eq!(classify_session(4, true, 0), WtsDisposition::Disconnected);
    }

    #[test]
    fn sign_in_screen_with_nobody_signed_in_lends_after_two_samples() {
        let mut tracker = SessionTracker::default();
        let now = Instant::now();
        let first = tracker.reduce(Ok(vec![sign_in(1)]), now);
        // Not yet confirmed: refusing, and not claiming anyone is at the desk.
        assert_eq!(first.session, SessionState::Unknown);
        assert_eq!(first.session_authority, SessionAuthority::Ambiguous);
        assert!(!lends(&first));
        assert_eq!(
            crate::plugin::presence::owner_present(&first, false, 0).as_deref(),
            Some("Windows session view not yet confirmed by a second identical read")
        );
        let second = tracker.reduce(Ok(vec![sign_in(1)]), now + Duration::from_secs(1));
        assert_eq!(second.session, SessionState::LoggedOut);
        assert_eq!(second.session_authority, SessionAuthority::NotRequired);
        assert!(second.applicable_session_ids.is_empty());
        assert!(lends(&second));
        let third = tracker.reduce(Ok(vec![sign_in(1)]), now + Duration::from_secs(2));
        assert_eq!(third.session, SessionState::LoggedOut);
    }

    #[test]
    fn one_signed_in_user_needs_a_reporter_and_does_not_lend() {
        let mut tracker = SessionTracker::default();
        for offset in 0..2 {
            let signal = tracker.reduce(
                Ok(vec![session(1, 10, WtsDisposition::ActiveUnlocked)]),
                Instant::now() + Duration::from_secs(offset),
            );
            assert_eq!(signal.session, SessionState::Active);
            assert_eq!(signal.session_authority, SessionAuthority::Missing);
            assert!(!lends(&signal));
        }
    }

    #[test]
    fn locked_user_lends_without_waiting_for_confirmation() {
        let signal = SessionTracker::default().reduce(
            Ok(vec![session(1, 10, WtsDisposition::ActiveLocked)]),
            Instant::now(),
        );
        assert_eq!(signal.session, SessionState::Locked);
        assert!(lends(&signal));
    }

    #[test]
    fn disconnected_rdp_user_and_empty_console_lend_after_two_samples() {
        // Switch user, or an RDP user who disconnected: the old session is
        // disconnected and the console shows the sign-in screen.
        let view = || {
            Ok(vec![
                session(3, 30, WtsDisposition::Disconnected),
                sign_in(1),
            ])
        };
        let mut tracker = SessionTracker::default();
        let now = Instant::now();
        let first = tracker.reduce(view(), now);
        assert_eq!(first.session_authority, SessionAuthority::Ambiguous);
        let second = tracker.reduce(view(), now + Duration::from_secs(1));
        assert_eq!(second.session, SessionState::Disconnected);
        assert_eq!(second.session_authority, SessionAuthority::NotRequired);
        assert!(lends(&second));
    }

    #[test]
    fn active_rdp_user_governs_while_the_console_shows_sign_in() {
        let signal = SessionTracker::default().reduce(
            Ok(vec![
                sign_in(1),
                session(3, 30, WtsDisposition::ActiveUnlocked),
            ]),
            Instant::now(),
        );
        assert_eq!(signal.session, SessionState::Active);
        assert_eq!(signal.applicable_session_ids, vec![3]);
        assert_eq!(signal.session_authority, SessionAuthority::Missing);
        assert!(!lends(&signal));
    }

    #[test]
    fn a_failed_read_vetoes_and_restarts_sign_in_confirmation() {
        let mut tracker = SessionTracker::default();
        let now = Instant::now();
        tracker.reduce(Ok(vec![sign_in(1)]), now);
        let lending = tracker.reduce(Ok(vec![sign_in(1)]), now + Duration::from_secs(1));
        assert!(lends(&lending));
        let failed = tracker.reduce(
            Err(anyhow::anyhow!("WTS query failed")),
            now + Duration::from_secs(2),
        );
        assert_eq!(failed.session_authority, SessionAuthority::Ambiguous);
        assert!(!lends(&failed));
        let after = tracker.reduce(Ok(vec![sign_in(1)]), now + Duration::from_secs(3));
        assert_eq!(after.session_authority, SessionAuthority::Ambiguous);
        assert!(!lends(&after));
    }

    #[test]
    fn a_single_sign_in_sample_between_active_samples_never_lends() {
        let mut tracker = SessionTracker::default();
        let now = Instant::now();
        let active = || Ok(vec![session(1, 10, WtsDisposition::ActiveUnlocked)]);
        tracker.reduce(active(), now);
        let flicker = tracker.reduce(Ok(vec![sign_in(1)]), now + Duration::from_secs(1));
        assert_eq!(flicker.session_authority, SessionAuthority::Ambiguous);
        assert!(!lends(&flicker));
        let back = tracker.reduce(active(), now + Duration::from_secs(2));
        assert_eq!(back.session, SessionState::Active);
        assert_eq!(
            back.session_requirements[0].fresh_after,
            now + Duration::from_secs(2)
        );

        // A confirmed sign-in view interrupted by one different view must be
        // confirmed again before it lends.
        tracker.reduce(Ok(vec![sign_in(1)]), now + Duration::from_secs(3));
        tracker.reduce(Ok(vec![sign_in(1)]), now + Duration::from_secs(4));
        tracker.reduce(Ok(vec![sign_in(2)]), now + Duration::from_secs(5));
        let changed = tracker.reduce(Ok(vec![sign_in(1)]), now + Duration::from_secs(6));
        assert_eq!(changed.session_authority, SessionAuthority::Ambiguous);
    }

    #[test]
    fn advanced_setting_keeps_the_sign_in_screen_refusing() {
        let mut tracker = SessionTracker::new(true);
        let now = Instant::now();
        for offset in 0..3 {
            for view in [
                vec![sign_in(1)],
                vec![session(3, 30, WtsDisposition::Disconnected), sign_in(1)],
            ] {
                let signal = tracker.reduce(Ok(view), now + Duration::from_secs(offset));
                assert_eq!(signal.session, SessionState::Active);
                assert_eq!(signal.session_authority, SessionAuthority::Ambiguous);
                assert!(!lends(&signal));
                assert!(crate::plugin::presence::owner_present(&signal, false, 0)
                    .unwrap()
                    .contains("sign_in_screen_is_presence"));
            }
        }
        // Signed-in states keep their normal meaning under the setting.
        let locked = tracker.reduce(
            Ok(vec![session(1, 10, WtsDisposition::ActiveLocked)]),
            now + Duration::from_secs(4),
        );
        assert_eq!(locked.session, SessionState::Locked);
        tracker.reduce(Ok(Vec::new()), now + Duration::from_secs(5));
        let empty = tracker.reduce(Ok(Vec::new()), now + Duration::from_secs(6));
        assert_eq!(empty.session, SessionState::LoggedOut);
    }

    #[test]
    fn ambiguity_and_invalid_logon_identity_fail_closed() {
        for inventory in [
            Err(anyhow::anyhow!("query failed")),
            Ok(vec![session(7, 70, WtsDisposition::Ambiguous)]),
            Ok(vec![
                session(7, 70, WtsDisposition::ActiveUnlocked),
                session(7, 70, WtsDisposition::ActiveUnlocked),
            ]),
            Ok(vec![session(7, 0, WtsDisposition::ActiveUnlocked)]),
        ] {
            let signal = SessionTracker::default().reduce(inventory, Instant::now());
            assert_eq!(signal.session, SessionState::Unknown);
            assert_eq!(signal.session_authority, SessionAuthority::Ambiguous);
            assert!(!lends(&signal));
            for unlocked_is_presence in [true, false] {
                let cause =
                    crate::plugin::presence::owner_present(&signal, unlocked_is_presence, 300)
                        .unwrap();
                assert!(cause.starts_with("Windows session inventory"), "{cause}");
            }
        }
    }
}
