//! Fallback presence source for platforms without a native backend yet
//! (macOS and other non-Windows, non-Linux targets).
//!
//! Reports [`SessionState::Unknown`] with no reporter requirement. The owner
//! check then has nothing to veto on: no session state, no owner-process
//! scan, no idle and no media reading. Lending on these platforms is decided
//! by the lending switches, capacity thresholds and any enabled pressure
//! input alone, so an attended desktop can be lent. That is a known limit of
//! the platform, stated in the README platform table, not a conservative
//! default.

use crate::plugin::presence::{
    MediaState, PresenceSignal, PresenceSource, SessionAuthority, SessionState,
};

pub struct UnknownPresence;

impl PresenceSource for UnknownPresence {
    fn kind(&self) -> &'static str {
        "fallback"
    }

    fn sample(&mut self, _owner_processes: &[String]) -> PresenceSignal {
        PresenceSignal {
            session: SessionState::Unknown,
            session_id: None,
            applicable_session_ids: Vec::new(),
            session_authority: SessionAuthority::NotRequired,
            session_requirements: Vec::new(),
            foreground: None,
            owner_app: None,
            idle_seconds: None,
            // No media backend on this platform yet. `NotChecked` rather
            // than `Unknown`, because the feature is absent rather than
            // failing — suspending idle lending everywhere would be wrong.
            media: MediaState::NotChecked,
            basis: Some(
                "no presence backend on this platform; session presence is not consulted".into(),
            ),
            sessions: Vec::new(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn no_backend_defers_to_the_other_signals() {
        let p = UnknownPresence.sample(&["Marathon".into()]);
        assert_eq!(p.session, SessionState::Unknown);
        assert_eq!(p.owner_app, None);
        assert!(crate::plugin::presence::owner_present(&p, true, 0).is_none());
    }
}
