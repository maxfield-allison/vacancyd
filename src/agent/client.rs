//! Agent side: run one per interactive session, report what only a session
//! can see.
//!
//! Deliberately tiny. It samples two cheap Win32 calls, writes a JSON line,
//! and reconnects patiently if the service is not up. It holds no policy and
//! makes no decisions — if this process is missing or wedged, the service
//! falls back to its own conservative view.

use std::time::Duration;

use anyhow::{Context, Result};
use tokio::io::AsyncWriteExt;
use tracing::{debug, info, warn};

use super::{unix_time_ms, AgentReport, AGENT_PROTOCOL_VERSION, PIPE_NAME};
use vacancyd::windows_pipe::{authenticate_server, current_session_id, open_pipe};

/// Connect and report until interrupted.
pub async fn run(interval: Duration, allow_foreground_daemon: bool) -> Result<()> {
    anyhow::ensure!(
        !interval.is_zero(),
        "agent interval must be greater than zero"
    );
    anyhow::ensure!(
        current_session_id() != 0,
        "agent requires an interactive session, not session 0"
    );
    let Some(_session_lock) = super::session_lock::SessionLock::acquire()? else {
        info!("an agent already owns this session; exiting");
        return Ok(());
    };
    info!(pipe = PIPE_NAME, "agent starting");
    let mut sequence = 0u64;
    // The error last reported for an unreachable service. Retries run every
    // ten seconds for as long as the service is stopped, so an unchanged
    // error is reported once, not on every attempt.
    let mut unreachable: Option<String> = None;
    loop {
        match connect(allow_foreground_daemon).await {
            Ok(mut pipe) => {
                unreachable = None;
                info!("connected to vacancyd service");
                loop {
                    sequence = sequence.wrapping_add(1).max(1);
                    let report = sample(sequence);
                    let mut line = serde_json::to_string(&report)?;
                    line.push('\n');
                    if let Err(e) = pipe.write_all(line.as_bytes()).await {
                        warn!(error = %e, "lost the service; will reconnect");
                        break;
                    }
                    tokio::time::sleep(interval).await;
                }
            }
            Err(e) => {
                let error = format!("{e:#}");
                if unreachable.as_deref() == Some(error.as_str()) {
                    debug!(error, "service still not reachable; retrying");
                } else {
                    warn!(error, "service not reachable; retrying");
                    unreachable = Some(error);
                }
                tokio::time::sleep(Duration::from_secs(10)).await;
            }
        }
    }
}

async fn connect(
    allow_foreground_daemon: bool,
) -> Result<tokio::net::windows::named_pipe::NamedPipeClient> {
    let pipe = open_pipe(PIPE_NAME)
        .context("opening the vacancyd agent pipe (is the service running?)")?;
    authenticate_server(&pipe, allow_foreground_daemon)?;
    Ok(pipe)
}

fn sample(sequence: u64) -> AgentReport {
    AgentReport {
        protocol_version: AGENT_PROTOCOL_VERSION,
        sequence,
        session_id: current_session_id(),
        idle_seconds: idle_seconds(),
        foreground: foreground_process(),
        agent_version: env!("CARGO_PKG_VERSION").to_string(),
        sampled_at_unix_ms: unix_time_ms(),
    }
}

/// Seconds since the last input in *this* session.
///
/// `GetLastInputInfo` reports only for the calling session — the reason this
/// process exists. `GetTickCount` wraps every ~49.7 days; a wrapped
/// subtraction would yield a huge idle time and wrongly release the veto, so
/// the wrap case is clamped to zero (treated as "just active").
fn idle_seconds() -> u64 {
    use windows_sys::Win32::System::SystemInformation::GetTickCount;
    use windows_sys::Win32::UI::Input::KeyboardAndMouse::{GetLastInputInfo, LASTINPUTINFO};

    let mut lii = LASTINPUTINFO {
        cbSize: std::mem::size_of::<LASTINPUTINFO>() as u32,
        dwTime: 0,
    };
    // SAFETY: `lii.cbSize` is set as the API requires; the call only writes
    // into `lii`.
    let ok = unsafe { GetLastInputInfo(&mut lii) };
    // SAFETY: no preconditions.
    let now = unsafe { GetTickCount() };
    idle_from_ticks(now, (ok != 0).then_some(lii.dwTime))
}

fn idle_from_ticks(now: u32, last_input: Option<u32>) -> u64 {
    last_input.map_or(0, |last| u64::from(now.saturating_sub(last)) / 1000)
}

fn foreground_process() -> Option<String> {
    use windows_sys::Win32::UI::WindowsAndMessaging::{
        GetForegroundWindow, GetWindowThreadProcessId,
    };
    // SAFETY: read-only USER32 queries; a null window is handled.
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
        process_name(pid)
    }
}

fn process_name(pid: u32) -> Option<String> {
    use windows_sys::Win32::{
        Foundation::CloseHandle,
        System::Threading::{
            OpenProcess, QueryFullProcessImageNameW, PROCESS_QUERY_LIMITED_INFORMATION,
        },
    };
    // SAFETY: read-only process handle, no injection or process modification.
    let process = unsafe { OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, 0, pid) };
    if process.is_null() {
        return None;
    }
    let mut buffer = vec![0u16; 32768];
    let mut length = buffer.len() as u32;
    // SAFETY: buffer capacity is supplied; process is closed on all paths.
    let ok = unsafe { QueryFullProcessImageNameW(process, 0, buffer.as_mut_ptr(), &mut length) };
    // SAFETY: successful OpenProcess returned the uniquely owned handle above.
    unsafe {
        CloseHandle(process);
    }
    if ok == 0 {
        return None;
    }
    let path = String::from_utf16(&buffer[..length as usize]).ok()?;
    Some(
        std::path::Path::new(&path)
            .file_stem()?
            .to_string_lossy()
            .into_owned(),
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn live_idle_stays_within_the_api_tick_range() {
        // A service-hosted runner may legitimately have no input for days.
        // This smoke check says nothing about desktop activity or API success;
        // those require the retained interactive-session acceptance test.
        let idle = idle_seconds();
        assert!(
            idle <= u64::from(u32::MAX) / 1000,
            "idle out of range: {idle}s"
        );
    }

    #[test]
    fn idle_arithmetic_accepts_long_inactivity_and_truncates_milliseconds() {
        assert_eq!(idle_from_ticks(181_700_999, Some(0)), 181_700);
        assert_eq!(idle_from_ticks(2_999, Some(1_000)), 1);
        assert_eq!(
            idle_from_ticks(u32::MAX, Some(0)),
            u64::from(u32::MAX) / 1000
        );
    }

    #[test]
    fn failed_query_wrap_or_future_input_never_claims_idle() {
        assert_eq!(idle_from_ticks(u32::MAX, None), 0);
        assert_eq!(idle_from_ticks(100, Some(u32::MAX - 100)), 0);
        assert_eq!(idle_from_ticks(100, Some(101)), 0);
        assert_eq!(idle_from_ticks(100, Some(100)), 0);
    }
}
