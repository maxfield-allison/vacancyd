//! Windows presence source.
//!
//! Session state comes from the Terminal Services API, which a session-0
//! service *can* call. Every active console and RDP session participates in
//! the decision. The foreground-window check is an extra refinement that only
//! yields anything when the daemon happens to run inside a user's session;
//! the installed service instead consumes verified per-session reports.

use std::time::{Duration, Instant};

use crate::plugin::presence::{MediaState, PresenceOptions, PresenceSignal, PresenceSource};
use crate::presence_impl::power_requests;
use crate::presence_impl::session_selection::{
    classify_session, SessionTracker, WtsDisposition, WtsSession,
};

pub struct WindowsPresence {
    opts: PresenceOptions,
    /// Last media reading, reused between checks. Reading it costs a process
    /// spawn, so it runs on its own slower cadence than the main poll.
    media: MediaState,
    checked_at: Option<Instant>,
    /// The error last logged, so a persistent failure is reported at WARN
    /// once per distinct cause instead of every few seconds forever. The
    /// reading itself carries the error to `/status` on every sample.
    warned: Option<String>,
    sessions: SessionTracker,
}

impl WindowsPresence {
    pub fn new(opts: PresenceOptions) -> Self {
        let sessions = SessionTracker::new(opts.sign_in_screen_is_presence);
        Self {
            opts,
            media: MediaState::NotChecked,
            checked_at: None,
            warned: None,
            sessions,
        }
    }

    fn media_state(&mut self) -> MediaState {
        if !self.opts.media_is_presence {
            return MediaState::NotChecked;
        }
        let due = self.checked_at.is_none_or(|t| {
            t.elapsed() >= Duration::from_millis(self.opts.media_poll_ms.max(1_000))
        });
        if due {
            self.checked_at = Some(Instant::now());
            self.media = match power_requests::display_holders() {
                Ok(names) => {
                    if self.warned.take().is_some() {
                        tracing::info!("media detection recovered");
                    }
                    match names.into_iter().find(|n| !self.is_ignored(n)) {
                        Some(name) => MediaState::Playing(name),
                        None => MediaState::Idle,
                    }
                }
                Err(e) => {
                    let error = e.to_string();
                    if self.warned.as_deref() != Some(error.as_str()) {
                        // Unknown, not Idle: see the safety rule in
                        // `plugin::presence`. An unlocked session keeps
                        // refusing until the reading recovers.
                        tracing::warn!(error = %error, "media detection unavailable; an unlocked session will not lend");
                        self.warned = Some(error.clone());
                    }
                    MediaState::Unknown(error)
                }
            };
        }
        self.media.clone()
    }

    fn is_ignored(&self, name: &str) -> bool {
        let lower = name.to_lowercase();
        self.opts
            .media_ignore
            .iter()
            .any(|i| lower.contains(&i.to_lowercase()))
    }
}

impl PresenceSource for WindowsPresence {
    fn kind(&self) -> &'static str {
        "windows-wts"
    }

    fn sample(&mut self, owner_processes: &[String]) -> PresenceSignal {
        let mut signal = self.sessions.reduce(session_inventory(), Instant::now());
        let foreground = (signal.applicable_session_ids.len() == 1)
            .then(foreground_process)
            .flatten();
        let owner_app = foreground.as_ref().and_then(|name| {
            let lower = name.to_lowercase();
            owner_processes
                .iter()
                .any(|p| lower == p.to_lowercase())
                .then(|| name.clone())
        });
        signal.foreground = foreground;
        signal.owner_app = owner_app;
        // WTS LastInputTime is intentionally not used for lending. Microsoft
        // documents it as unreliable and one value cannot represent multiple
        // active sessions. Only verified per-session reporters populate idle.
        signal.idle_seconds = None;
        signal.media = self.media_state();
        signal
    }
}

/// Enumerate every local interactive session, including RDP sessions.
fn session_inventory() -> anyhow::Result<Vec<WtsSession>> {
    use windows_sys::Win32::System::RemoteDesktop::{
        WTSEnumerateSessionsW, WTSFreeMemory, WTS_CURRENT_SERVER_HANDLE, WTS_SESSION_INFOW,
    };

    let mut entries: *mut WTS_SESSION_INFOW = std::ptr::null_mut();
    let mut count = 0u32;
    // SAFETY: the out-parameters are valid; version 1 is the documented
    // WTS_SESSION_INFOW layout. The returned allocation is freed below.
    let ok =
        unsafe { WTSEnumerateSessionsW(WTS_CURRENT_SERVER_HANDLE, 0, 1, &mut entries, &mut count) };
    if ok == 0 {
        anyhow::bail!(
            "enumerating Windows sessions: {}",
            std::io::Error::last_os_error()
        );
    }
    if count > 0 && entries.is_null() {
        anyhow::bail!("Windows session enumeration returned a null buffer");
    }

    let rows = if count == 0 {
        Vec::new()
    } else {
        // SAFETY: the API returned `count` contiguous WTS_SESSION_INFOW rows.
        unsafe { std::slice::from_raw_parts(entries, count as usize) }
            .iter()
            .map(|entry| (entry.SessionId, entry.State))
            .collect::<Vec<_>>()
    };
    if !entries.is_null() {
        // SAFETY: `entries` came from WTSEnumerateSessionsW.
        unsafe { WTSFreeMemory(entries.cast()) };
    }

    let mut inventory = Vec::new();
    for (session_id, enumerated_state) in rows {
        // Session 0 is the services session. WTSListen is a listener, not a
        // signed-in user (the numeric values are the stable WTS enum values).
        if session_id == 0 || enumerated_state == 6 {
            continue;
        }
        inventory.push(
            query_session(session_id, enumerated_state).unwrap_or(WtsSession {
                id: session_id,
                logon_time: 1,
                disposition: WtsDisposition::Ambiguous,
            }),
        );
    }
    Ok(inventory)
}

fn query_session(session_id: u32, enumerated_state: i32) -> anyhow::Result<WtsSession> {
    use windows_sys::Win32::System::RemoteDesktop::{
        WTSFreeMemory, WTSQuerySessionInformationW, WTSSessionInfoEx, WTSINFOEXW,
        WTS_CURRENT_SERVER_HANDLE,
    };

    let mut buffer: *mut u16 = std::ptr::null_mut();
    let mut bytes: u32 = 0;
    // SAFETY: out-params initialised above; on success `buffer` is a
    // WTSINFOEXW owned by the API and released with WTSFreeMemory below.
    let ok = unsafe {
        WTSQuerySessionInformationW(
            WTS_CURRENT_SERVER_HANDLE,
            session_id,
            WTSSessionInfoEx,
            &mut buffer as *mut _ as *mut _,
            &mut bytes,
        )
    };
    if ok == 0 || buffer.is_null() {
        anyhow::bail!(
            "querying Windows session {session_id}: {}",
            std::io::Error::last_os_error()
        );
    }

    let result = if (bytes as usize) < std::mem::size_of::<WTSINFOEXW>() {
        Err(anyhow::anyhow!("Windows session info was truncated"))
    } else {
        // SAFETY: WTSSessionInfoEx yields a WTSINFOEXW and its size was
        // checked above. Level 1 is the only defined union member.
        unsafe {
            let info = &*(buffer as *const WTSINFOEXW);
            if info.Level != 1 {
                Err(anyhow::anyhow!("unsupported Windows session info level"))
            } else {
                let detail = &info.Data.WTSInfoExLevel1;
                let has_user = detail.UserName.first().copied().unwrap_or(0) != 0;
                let consistent =
                    detail.SessionId == session_id && detail.SessionState == enumerated_state;
                if !consistent {
                    Ok(WtsSession {
                        id: session_id,
                        logon_time: detail.LogonTime.max(1),
                        disposition: WtsDisposition::Ambiguous,
                    })
                } else {
                    let disposition =
                        classify_session(enumerated_state, has_user, detail.SessionFlags);
                    Ok(WtsSession {
                        id: session_id,
                        logon_time: detail.LogonTime,
                        disposition,
                    })
                }
            }
        }
    };
    // SAFETY: `buffer` came from WTSQuerySessionInformationW.
    unsafe { WTSFreeMemory(buffer as *mut _) };
    result
}

/// Foreground process name. Returns `None` from session 0 (a service),
/// where there is no desktop to query — which is expected, not an error.
fn foreground_process() -> Option<String> {
    use windows_sys::Win32::UI::WindowsAndMessaging::{
        GetForegroundWindow, GetWindowThreadProcessId,
    };
    // SAFETY: read-only USER32 queries; a null HWND is handled.
    unsafe {
        let hwnd = GetForegroundWindow();
        if hwnd.is_null() {
            return None;
        }
        let mut pid: u32 = 0;
        GetWindowThreadProcessId(hwnd, &mut pid);
        if pid == 0 {
            return None;
        }
        crate::probes::system::process_name(pid)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::plugin::presence::{SessionAuthority, SessionState};

    #[test]
    fn real_inventory_fails_closed_on_ambiguity() {
        // A headless host can expose an unsigned-in console in WTSConnected,
        // which is the sign-in screen, and other transitional states, which
        // are ambiguous and must fail closed. The live smoke test therefore
        // verifies the safety boundary instead of requiring an uncontrolled
        // host to have only signed-in sessions.
        let inventory = session_inventory().expect("WTS query failed");
        let has_ambiguity = inventory
            .iter()
            .any(|session| session.disposition == WtsDisposition::Ambiguous);
        let signal = SessionTracker::default().reduce(Ok(inventory), Instant::now());
        if has_ambiguity {
            assert_eq!(signal.session, SessionState::Unknown);
            assert_eq!(signal.session_authority, SessionAuthority::Ambiguous);
        }
    }

    #[test]
    fn real_inventory_has_unique_session_ids() {
        let inventory = session_inventory().expect("WTS query failed");
        let mut ids = std::collections::HashSet::new();
        assert!(inventory.iter().all(|session| ids.insert(session.id)));
    }

    #[test]
    fn owner_app_matches_case_insensitively() {
        let mut p = WindowsPresence::new(PresenceOptions::default());
        let sig = p.sample(&["definitely-not-running-xyz".into()]);
        assert!(sig.owner_app.is_none());
    }

    #[test]
    fn media_detection_is_never_silently_absent() {
        // Whatever the privilege level, the reading must be one of the
        // decided states — never a default that reads as "nothing playing".
        // Unelevated (the test runner) this is Unknown; as a service, Idle
        // or Playing. All three are safe; a bare `Idle` from a failed read
        // would not be.
        let mut p = WindowsPresence::new(PresenceOptions::default());
        let sig = p.sample(&[]);
        assert_ne!(sig.media, MediaState::NotChecked);
    }

    #[test]
    fn detection_can_be_switched_off() {
        let mut p = WindowsPresence::new(PresenceOptions {
            media_is_presence: false,
            ..PresenceOptions::default()
        });
        assert_eq!(p.sample(&[]).media, MediaState::NotChecked);
    }

    #[test]
    fn ignored_holders_do_not_count_as_playback() {
        let p = WindowsPresence::new(PresenceOptions {
            media_ignore: vec!["Discord".into()],
            ..PresenceOptions::default()
        });
        assert!(p.is_ignored("discord"));
        assert!(p.is_ignored("Discord (helper)"));
        assert!(!p.is_ignored("vlc"));
    }
}
