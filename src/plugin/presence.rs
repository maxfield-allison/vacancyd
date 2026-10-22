//! The **presence** seam: is the owner using the machine?
//!
//! This is the definition the whole daemon turns on, so it is worth stating
//! plainly: **the owner is present when they are sitting at the machine, not
//! when something they launched is still running.** A locked or logged-out
//! machine is lendable even with a game still loaded, because nobody is
//! looking at it.
//!
//! That definition is also what makes an always-on daemon possible. A
//! foreground-window check cannot work from a Windows service (session 0 has
//! no desktop), but session state can — so the strongest signal is also the
//! one that survives the deployment we actually want.
//!
//! Signals, strongest first:
//!
//! | Signal | Meaning | From a service? |
//! | ------ | ------- | --------------- |
//! | No console session, or only the sign-in screen | logged out | ✅ |
//! | Session locked | owner away | ✅ |
//! | Session disconnected | owner away (RDP detached) | ✅ |
//! | Foreground process on the owner list | owner at a game | ⚠️ user session only |
//! | Display-required power request | the owner is watching something | ✅ |
//!
//! On Linux the same session signals come from systemd-logind (see
//! `presence_impl::logind_selection` for which sessions count), and the
//! owner-process list is matched against the service's own process scan.
//!
//! Attributed hardware pressure is deliberately absent from this table. It is
//! measured and enforced per resource; folding it into presence would make
//! load on one GPU refuse unrelated resources.

use std::time::Instant;

use serde::Serialize;

/// Where the interactive console session is.
///
/// On platforms whose presence backend is still the stub (macOS), only
/// `Unknown` is ever constructed — the other variants are the contract every
/// future backend implements against, not dead code.
#[cfg_attr(not(any(windows, target_os = "linux")), allow(dead_code))]
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum SessionState {
    /// Logged in and unlocked.
    Active,
    /// Logged in but locked — the owner stepped away.
    Locked,
    /// Session exists but is disconnected.
    Disconnected,
    /// Nobody logged in.
    LoggedOut,
    /// Could not determine. With [`SessionAuthority::NotRequired`] this is a
    /// platform with no presence backend (macOS), where other signals govern
    /// rather than a permanent veto. With [`SessionAuthority::Ambiguous`] a
    /// backend exists and has no trustworthy answer yet: it could not be
    /// read (Linux without a reachable systemd-logind), its reply was
    /// inconsistent, or the view has not been confirmed by a second identical
    /// read. That refuses, with `basis` naming which.
    Unknown,
}

/// Whether the service has enough session-scoped evidence to use the finer
/// idle and foreground signals safely.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum SessionAuthority {
    /// Every applicable interactive session has a fresh, verified report.
    Ready,
    /// No active unlocked session needs a reporter for this observation.
    NotRequired,
    /// At least one applicable session has no fresh post-transition report.
    Missing,
    /// Windows could not establish a complete, self-consistent session view.
    Ambiguous,
}

/// Internal freshness boundary for one applicable Windows session.
///
/// This is deliberately omitted from `/status`: the monotonic timestamp has
/// meaning only inside this process. The session IDs and authority verdict are
/// exposed separately.
#[derive(Debug, Clone)]
pub struct SessionRequirement {
    pub session_id: u32,
    pub fresh_after: Instant,
}

/// Whether something is holding the machine awake to be *watched*.
///
/// Idle time cannot distinguish "walked away" from "watching a film" — both
/// are hours without input on an unlocked session. This does, by reading the
/// display-wake request the player itself asserts.
///
/// Only Windows has a backend today, so elsewhere just `NotChecked` is ever
/// constructed — the rest are the contract a future backend implements
/// against, not dead code.
#[cfg_attr(not(windows), allow(dead_code))]
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum MediaState {
    /// Detection is off, or this platform has no backend for it. Media then
    /// has no bearing on the decision either way.
    NotChecked,
    /// Checked: nothing is keeping the display awake.
    Idle,
    /// Checked: something is, and it is named here.
    Playing(String),
    /// Enabled but unreadable, with why: typically not elevated, or output
    /// in a shape the reader does not recognize. Treated as "might be
    /// playing": it keeps an unlocked session refusing wherever a confirmed
    /// quiet reading would have let it lend, so it can never make the
    /// daemon lend *more*.
    Unknown(String),
}

/// The veto cause while media detection is enabled but unreadable. Borrowers
/// see the cause, so it names the failed input and not the error text,
/// which `/status` carries in `presence.media_error`.
pub const MEDIA_UNKNOWN_CAUSE: &str = "media detection unavailable";

/// One presence observation.
#[derive(Debug, Clone, Serialize)]
pub struct PresenceSignal {
    pub session: SessionState,
    /// Kernel-issued Windows session identifier for the session whose state
    /// was sampled. Absent on platforms without an equivalent source.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub session_id: Option<u32>,
    /// All active, unlocked console and RDP sessions participating in this
    /// decision, sorted by their kernel-issued session ID.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub applicable_session_ids: Vec<u32>,
    /// Whether the session inventory and all required reporter observations
    /// are complete enough to authorize idle lending.
    pub session_authority: SessionAuthority,
    /// Monotonic report cutoffs, used only while merging agent observations.
    #[serde(skip)]
    pub session_requirements: Vec<SessionRequirement>,
    /// Foreground process name, where observable.
    pub foreground: Option<String>,
    /// Set when `foreground` is on the configured owner list.
    pub owner_app: Option<String>,
    /// Seconds since the owner last touched keyboard or mouse, where the
    /// platform reports it. `None` means unknown, which is treated as
    /// "recently active" — never as idle.
    pub idle_seconds: Option<u64>,
    /// Whether media playback is holding the display awake.
    pub media: MediaState,
    /// Why the backend reached this state, in one line. Diagnostic text for
    /// `/status` and the veto cause; never parsed.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub basis: Option<String>,
    /// Every session a backend that enumerates sessions (logind) read, with
    /// whether and why it counted.
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub sessions: Vec<SessionView>,
}

/// One enumerated session as `/status` shows it.
#[cfg_attr(not(any(target_os = "linux", test)), allow(dead_code))]
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct SessionView {
    pub id: String,
    pub class: String,
    #[serde(rename = "type")]
    pub kind: String,
    pub remote: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub seat: Option<String>,
    pub state: String,
    pub active: bool,
    pub locked: bool,
    /// Whether this session means someone is at the machine.
    pub counted: bool,
    pub reason: String,
}

impl Default for PresenceSignal {
    fn default() -> Self {
        Self {
            session: SessionState::Unknown,
            session_id: None,
            applicable_session_ids: Vec::new(),
            session_authority: SessionAuthority::NotRequired,
            session_requirements: Vec::new(),
            foreground: None,
            owner_app: None,
            idle_seconds: None,
            media: MediaState::NotChecked,
            basis: None,
            sessions: Vec::new(),
        }
    }
}

/// Knobs a presence backend needs, passed in rather than read from the
/// global config so the seam stays independent of how vacancyd is configured.
///
/// Read only by backends that implement media detection — Windows today —
/// so on other targets these are the contract, not dead code.
#[cfg_attr(not(windows), allow(dead_code))]
#[derive(Debug, Clone)]
pub struct PresenceOptions {
    /// Treat media playback as owner presence.
    pub media_is_presence: bool,
    /// How often to re-read the media signal, in milliseconds. Decoupled
    /// from the main poll because reading it costs a process spawn.
    pub media_poll_ms: u64,
    /// Holders to disregard, for the occasional application that asserts a
    /// display request and never releases it.
    pub media_ignore: Vec<String>,
    /// Keep the sign-in screen (Windows) or an active display-manager
    /// greeter (Linux) refusing. Off by default: a sign-in screen with nobody
    /// signed in is the logged-out tier and lends.
    pub sign_in_screen_is_presence: bool,
    /// `owner.presence = "headless"`: do not consult session presence at all.
    pub headless: bool,
}

impl Default for PresenceOptions {
    fn default() -> Self {
        Self {
            media_is_presence: true,
            media_poll_ms: 5_000,
            media_ignore: Vec::new(),
            sign_in_screen_is_presence: false,
            headless: false,
        }
    }
}

/// Samples owner presence for one platform.
pub trait PresenceSource: Send + Sync {
    fn kind(&self) -> &'static str;
    fn sample(&mut self, owner_processes: &[String]) -> PresenceSignal;
}

/// The presence source compiled for this platform, unless the operator
/// declared the host headless.
pub fn for_platform(opts: PresenceOptions) -> Box<dyn PresenceSource> {
    if opts.headless {
        return Box::new(HeadlessPresence);
    }
    #[cfg(windows)]
    {
        Box::new(crate::presence_impl::windows::WindowsPresence::new(opts))
    }
    #[cfg(target_os = "linux")]
    {
        Box::new(crate::presence_impl::linux::LogindPresence::new(opts))
    }
    #[cfg(not(any(windows, target_os = "linux")))]
    {
        let _ = opts;
        Box::new(crate::presence_impl::fallback::UnknownPresence)
    }
}

/// `owner.presence = "headless"`: the operator states nobody sits at this
/// machine, so session state, the owner-process list, idle and media are not
/// read. It reports the logged-out tier so attributed pressure calibrates
/// against the unattended profile, and says why in `basis`.
pub struct HeadlessPresence;

impl PresenceSource for HeadlessPresence {
    fn kind(&self) -> &'static str {
        "headless"
    }

    fn sample(&mut self, _owner_processes: &[String]) -> PresenceSignal {
        PresenceSignal {
            session: SessionState::LoggedOut,
            basis: Some("owner.presence = \"headless\": session presence is not consulted".into()),
            ..PresenceSignal::default()
        }
    }
}

/// Decide whether the owner is present. `Some(reason)` stops lending.
///
/// `unlocked_is_presence` reflects the config option of the same name: with
/// it on (the default), an unlocked desktop is presence on its own; with it
/// off, only an explicit policy signal such as a listed process counts.
/// `idle_lends_after_s`: when set, an unlocked session that has seen no input
/// for this many seconds stops counting as presence. 0 disables it — an
/// unlocked session is then presence no matter how long it has sat untouched.
///
/// The daemon itself calls [`assess`], which also says which policy let an
/// active session lend; this is the veto alone, as the tests read it.
#[cfg(test)]
pub fn owner_present(
    p: &PresenceSignal,
    unlocked_is_presence: bool,
    idle_lends_after_s: u64,
) -> Option<String> {
    match assess(p, unlocked_is_presence, idle_lends_after_s) {
        Verdict::Present(reason) => Some(reason),
        Verdict::Lends(_) => None,
    }
}

/// The owner check's answer, with the policy that let an active session
/// lend, so the published basis can name it rather than still reading as
/// "active and unlocked".
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Verdict {
    /// The owner is present, for this reason. Lending stops.
    Present(String),
    /// Not present. `Some` when the session is active and unlocked and a
    /// configured policy is what releases it.
    Lends(Option<ActiveRelease>),
}

/// Why an active, unlocked session does not count as presence.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ActiveRelease {
    /// `owner.idle_lends_after_s`: no input for at least that long.
    Idle { idle_s: u64, threshold_s: u64 },
    /// `owner.unlocked_session_is_presence = false`.
    UnlockedIsNotPresence,
}

impl ActiveRelease {
    /// One clause for `basis`.
    pub fn describe(self) -> String {
        match self {
            Self::Idle {
                idle_s,
                threshold_s,
            } => format!(
                "idle lending: no input for {idle_s} s (owner.idle_lends_after_s = {threshold_s})"
            ),
            Self::UnlockedIsNotPresence => {
                "lending while unlocked (owner.unlocked_session_is_presence = false)".into()
            }
        }
    }
}

/// [`owner_present`], keeping the reason an active session lends.
pub fn assess(p: &PresenceSignal, unlocked_is_presence: bool, idle_lends_after_s: u64) -> Verdict {
    match p.session {
        // Attendance does not itself claim a resource. Per-resource pressure
        // protects unattended owner work independently of this policy result.
        SessionState::LoggedOut | SessionState::Locked | SessionState::Disconnected => {
            Verdict::Lends(None)
        }
        SessionState::Active => {
            if matches!(
                p.session_authority,
                SessionAuthority::Missing | SessionAuthority::Ambiguous
            ) {
                return Verdict::Present(match p.session_authority {
                    SessionAuthority::Missing => "session evidence incomplete".into(),
                    SessionAuthority::Ambiguous => p
                        .basis
                        .clone()
                        .unwrap_or_else(|| "session state unavailable".into()),
                    _ => unreachable!(),
                });
            }
            if let Some(app) = &p.owner_app {
                return Verdict::Present(app.clone());
            }
            // Something is deliberately holding the display awake: a film, a
            // stream, a presentation. It takes no input for hours, so without
            // this it is indistinguishable from an empty chair — and lending
            // the GPU out from under a decoder is exactly the stutter this
            // daemon exists to avoid. Checked before `unlocked_is_presence`
            // so it protects playback even for owners who opt into lending
            // while they work.
            if let MediaState::Playing(app) = &p.media {
                return Verdict::Present(format!("media playback ({app})"));
            }
            // What would let this unlocked session lend: the owner lends
            // while working, or it has gone untouched (the owner walked away
            // without locking). Only a real idle reading counts: unknown idle
            // is never read as "nobody's here".
            let release = if !unlocked_is_presence {
                Some(ActiveRelease::UnlockedIsNotPresence)
            } else {
                p.idle_seconds
                    .filter(|&idle_s| idle_lends_after_s > 0 && idle_s >= idle_lends_after_s)
                    .map(|idle_s| ActiveRelease::Idle {
                        idle_s,
                        threshold_s: idle_lends_after_s,
                    })
            };
            match release {
                None => Verdict::Present(
                    p.foreground
                        .clone()
                        .unwrap_or_else(|| "session unlocked".into()),
                ),
                // Both releases rest on media having been read: a film takes
                // no input, and "lend while I work" never meant "lend while I
                // watch". Unreadable media could be a film, so it refuses.
                Some(_) if matches!(p.media, MediaState::Unknown(_)) => {
                    Verdict::Present(MEDIA_UNKNOWN_CAUSE.into())
                }
                Some(release) => Verdict::Lends(Some(release)),
            }
        }
        SessionState::Unknown => {
            // A backend that exists but has no trustworthy answer refuses,
            // naming what is missing. Only a platform with no backend at all
            // (NotRequired) defers to the other signals.
            if p.session_authority == SessionAuthority::Ambiguous {
                return Verdict::Present(
                    p.basis
                        .clone()
                        .unwrap_or_else(|| "session state unavailable".into()),
                );
            }
            if let Some(app) = &p.owner_app {
                return Verdict::Present(app.clone());
            }
            if let MediaState::Playing(app) = &p.media {
                return Verdict::Present(format!("media playback ({app})"));
            }
            if let MediaState::Unknown(_) = &p.media {
                return Verdict::Present(MEDIA_UNKNOWN_CAUSE.into());
            }
            Verdict::Lends(None)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn present(
        p: &PresenceSignal,
        unlocked_is_presence: bool,
        idle_lends_after_s: u64,
    ) -> Option<String> {
        owner_present(p, unlocked_is_presence, idle_lends_after_s)
    }

    fn sig(session: SessionState, fg: Option<&str>, owner: Option<&str>) -> PresenceSignal {
        PresenceSignal {
            session,
            session_id: None,
            applicable_session_ids: Vec::new(),
            session_authority: SessionAuthority::Ready,
            session_requirements: Vec::new(),
            foreground: fg.map(String::from),
            owner_app: owner.map(String::from),
            idle_seconds: None,
            media: MediaState::Idle,
            basis: None,
            sessions: Vec::new(),
        }
    }

    #[test]
    fn away_states_are_lendable() {
        for s in [
            SessionState::Locked,
            SessionState::LoggedOut,
            SessionState::Disconnected,
        ] {
            assert!(present(&sig(s, None, None), true, 0).is_none());
        }
    }

    #[test]
    fn locked_is_lendable_even_with_a_game_loaded() {
        // The owner walked away leaving a game sitting at a menu: still
        // lendable, because presence is about the human, not about what is
        // still resident. Resource pressure is evaluated separately.
        let p = sig(SessionState::Locked, Some("Marathon"), Some("Marathon"));
        assert!(present(&p, true, 0).is_none());
    }

    #[test]
    fn an_unlocked_session_can_be_opted_out_of_policy() {
        // Resource pressure may still refuse individual dependencies; it is
        // deliberately not folded into this machine-wide policy decision.
        let p = sig(SessionState::Active, Some("Code"), None);
        assert!(present(&p, false, 0).is_none());
    }

    #[test]
    fn unlocked_desktop_is_presence_by_default() {
        let p = sig(SessionState::Active, Some("Code"), None);
        assert_eq!(present(&p, true, 0).as_deref(), Some("Code"));
    }

    #[test]
    fn unlocked_can_be_opted_out_of() {
        let p = sig(SessionState::Active, Some("Code"), None);
        assert!(present(&p, false, 0).is_none());
        // ...but a listed game still counts.
        let g = sig(SessionState::Active, Some("Marathon"), Some("Marathon"));
        assert_eq!(present(&g, false, 0).as_deref(), Some("Marathon"));
    }

    fn idle(session: SessionState, secs: Option<u64>) -> PresenceSignal {
        PresenceSignal {
            session,
            session_id: None,
            applicable_session_ids: Vec::new(),
            session_authority: SessionAuthority::Ready,
            session_requirements: Vec::new(),
            foreground: Some("Code".into()),
            owner_app: None,
            idle_seconds: secs,
            media: MediaState::Idle,
            basis: None,
            sessions: Vec::new(),
        }
    }

    #[test]
    fn idle_releases_an_unlocked_session() {
        // The owner walked away without locking - the case that otherwise
        // costs the entire budget.
        let p = idle(SessionState::Active, Some(600));
        assert!(present(&p, true, 300).is_none());
    }

    #[test]
    fn the_verdict_names_the_policy_that_lets_an_active_session_lend() {
        let p = idle(SessionState::Active, Some(600));
        assert_eq!(
            assess(&p, true, 300),
            Verdict::Lends(Some(ActiveRelease::Idle {
                idle_s: 600,
                threshold_s: 300
            }))
        );
        assert_eq!(
            ActiveRelease::Idle {
                idle_s: 600,
                threshold_s: 300
            }
            .describe(),
            "idle lending: no input for 600 s (owner.idle_lends_after_s = 300)"
        );
        // Lending while working is its own policy, whatever idle says.
        assert_eq!(
            assess(&p, false, 300),
            Verdict::Lends(Some(ActiveRelease::UnlockedIsNotPresence))
        );
        assert_eq!(
            assess(&idle(SessionState::Active, Some(10)), false, 300),
            Verdict::Lends(Some(ActiveRelease::UnlockedIsNotPresence))
        );
        // Away states lend on their own, with nothing to add.
        assert_eq!(
            assess(&idle(SessionState::Locked, Some(10)), true, 300),
            Verdict::Lends(None)
        );
        assert_eq!(
            assess(&idle(SessionState::Active, Some(10)), true, 300),
            Verdict::Present("Code".into())
        );
    }

    #[test]
    fn recent_input_still_counts_as_presence() {
        let p = idle(SessionState::Active, Some(30));
        assert_eq!(present(&p, true, 300).as_deref(), Some("Code"));
    }

    #[test]
    fn unknown_idle_is_never_treated_as_idle() {
        // The safety rule: no agent, a dead agent, or a stale report must
        // never make the machine MORE willing to lend.
        let p = idle(SessionState::Active, None);
        assert_eq!(present(&p, true, 300).as_deref(), Some("Code"));
    }

    #[test]
    fn zero_threshold_disables_idle_lending() {
        let p = idle(SessionState::Active, Some(86_400));
        assert_eq!(present(&p, true, 0).as_deref(), Some("Code"));
    }

    #[test]
    fn a_listed_game_outranks_idle() {
        // Idle time must not release the veto for a running game - a game
        // takes no keyboard input during a cutscene.
        let mut p = idle(SessionState::Active, Some(9_999));
        p.foreground = Some("Marathon".into());
        p.owner_app = Some("Marathon".into());
        assert_eq!(present(&p, true, 300).as_deref(), Some("Marathon"));
    }

    fn watching(secs: Option<u64>, media: MediaState) -> PresenceSignal {
        PresenceSignal {
            session: SessionState::Active,
            session_id: None,
            applicable_session_ids: Vec::new(),
            session_authority: SessionAuthority::Ready,
            session_requirements: Vec::new(),
            foreground: Some("mpv".into()),
            owner_app: None,
            idle_seconds: secs,
            media,
            basis: None,
            sessions: Vec::new(),
        }
    }

    #[test]
    fn a_film_outranks_idle() {
        // Two hours without touching the keyboard, because the owner is
        // watching a film. Idle time alone would hand the GPU away mid-scene.
        let p = watching(Some(7_200), MediaState::Playing("vlc".into()));
        assert_eq!(
            present(&p, true, 900).as_deref(),
            Some("media playback (vlc)")
        );
    }

    #[test]
    fn media_protects_playback_even_when_lending_while_working() {
        // unlocked_is_presence = false means "lend while I work". It must
        // still not mean "lend while I watch".
        let p = watching(Some(7_200), MediaState::Playing("Plex".into()));
        assert_eq!(
            present(&p, false, 900).as_deref(),
            Some("media playback (Plex)")
        );
    }

    fn unreadable() -> MediaState {
        MediaState::Unknown("powercfg /requests: unrecognized format".into())
    }

    #[test]
    fn unknown_media_never_releases_on_idle() {
        // Detection enabled but unreadable (not elevated, or a format the
        // reader refuses). "I cannot tell if a film is playing" must never
        // resolve to "no film is playing", and the cause names the input
        // that failed rather than the foreground app.
        let p = watching(Some(7_200), unreadable());
        assert_eq!(present(&p, true, 900).as_deref(), Some(MEDIA_UNKNOWN_CAUSE));
    }

    #[test]
    fn unknown_media_refuses_when_lending_while_working() {
        // With unlocked_session_is_presence = false, unknown
        // media used to have no effect, so a machine whose media probe
        // failed lent during playback. It must refuse, as Playing would.
        for idle in [None, Some(5), Some(7_200)] {
            let p = watching(idle, unreadable());
            for threshold in [0, 900] {
                assert_eq!(
                    present(&p, false, threshold).as_deref(),
                    Some(MEDIA_UNKNOWN_CAUSE),
                    "idle {idle:?}, threshold {threshold}"
                );
            }
        }
    }

    #[test]
    fn unknown_media_does_not_hide_a_stronger_cause() {
        // Recent input on an unlocked desktop is presence on its own; the
        // cause stays the foreground app.
        let p = watching(Some(10), unreadable());
        assert_eq!(present(&p, true, 900).as_deref(), Some("mpv"));
        // A listed owner app outranks it too.
        let mut game = watching(Some(7_200), unreadable());
        game.owner_app = Some("Marathon".into());
        assert_eq!(present(&game, false, 900).as_deref(), Some("Marathon"));
    }

    #[test]
    fn unknown_media_does_not_block_an_away_session() {
        // Nobody is looking at a locked, disconnected or logged-out machine,
        // whatever media might be doing.
        for session in [
            SessionState::Locked,
            SessionState::Disconnected,
            SessionState::LoggedOut,
        ] {
            let mut p = watching(Some(0), unreadable());
            p.session = session;
            assert!(present(&p, false, 0).is_none(), "{session:?}");
        }
    }

    #[test]
    fn unknown_media_refuses_without_a_session_backend() {
        let mut p = sig(SessionState::Unknown, None, None);
        p.session_authority = SessionAuthority::NotRequired;
        p.media = unreadable();
        assert_eq!(present(&p, true, 0).as_deref(), Some(MEDIA_UNKNOWN_CAUSE));
    }

    #[test]
    fn a_confirmed_quiet_machine_still_lends_on_idle() {
        // The counterpart: once media is actually checked and quiet, idle
        // lending works as designed. Otherwise the guard would cost the
        // whole feature.
        let p = watching(Some(7_200), MediaState::Idle);
        assert!(present(&p, true, 900).is_none());
        // ...and with detection switched off entirely, likewise.
        let off = watching(Some(7_200), MediaState::NotChecked);
        assert!(present(&off, true, 900).is_none());
    }

    #[test]
    fn incomplete_or_ambiguous_active_session_evidence_vetoes() {
        for authority in [SessionAuthority::Missing, SessionAuthority::Ambiguous] {
            let mut p = idle(SessionState::Active, Some(9_999));
            p.session_authority = authority;
            assert!(present(&p, false, 300).is_some());
        }
    }

    #[test]
    fn locked_with_media_playing_is_still_lendable() {
        // The tier-3 rule does not bend: nobody is looking at a locked
        // screen, whatever is still decoding behind it.
        let mut p = watching(Some(10), MediaState::Playing("vlc".into()));
        p.session = SessionState::Locked;
        assert!(present(&p, true, 900).is_none());
    }

    #[test]
    fn unknown_platform_defers_to_other_signals() {
        let mut p = sig(SessionState::Unknown, None, None);
        p.session_authority = SessionAuthority::NotRequired;
        assert!(present(&p, true, 0).is_none());
    }

    #[test]
    fn an_unreadable_presence_backend_vetoes_and_names_itself() {
        // Linux with systemd-logind unreachable: the backend exists and
        // failed, which must not read as the macOS "no backend" case.
        let mut p = sig(SessionState::Unknown, None, None);
        p.session_authority = SessionAuthority::Ambiguous;
        p.basis = Some("systemd-logind unavailable: connection refused".into());
        for unlocked_is_presence in [true, false] {
            assert_eq!(
                present(&p, unlocked_is_presence, 300).as_deref(),
                Some("systemd-logind unavailable: connection refused")
            );
        }
    }

    #[test]
    fn headless_presence_is_logged_out_and_ignores_owner_processes() {
        let mut source = for_platform(PresenceOptions {
            headless: true,
            ..PresenceOptions::default()
        });
        assert_eq!(source.kind(), "headless");
        let p = source.sample(&["Marathon".into()]);
        assert_eq!(p.session, SessionState::LoggedOut);
        assert_eq!(p.owner_app, None);
        assert!(p.basis.unwrap().contains("headless"));
    }

    #[test]
    fn a_linux_active_session_needs_no_reporter() {
        // logind is itself complete evidence: NotRequired on an active
        // session is presence, not ambiguity, and idle lending still works
        // from a trustworthy idle reading.
        let mut p = idle(SessionState::Active, None);
        p.session_authority = SessionAuthority::NotRequired;
        p.foreground = None;
        assert_eq!(present(&p, true, 300).as_deref(), Some("session unlocked"));
        p.idle_seconds = Some(900);
        assert!(present(&p, true, 300).is_none());
    }
}
