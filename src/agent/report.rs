//! Shared wire format for the CLI agent, background agent and service.
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use serde::{Deserialize, Serialize};
use tokio::io::{AsyncBufRead, AsyncBufReadExt};

#[cfg(windows)]
pub const PIPE_NAME: &str = r"\\.\pipe\vacancyd-agent";
#[cfg(windows)]
pub use vacancyd::windows_pipe::INTERACTIVE_PIPE_ACCESS;
#[cfg_attr(not(any(windows, test)), allow(dead_code))]
pub const AGENT_PROTOCOL_VERSION: u32 = 1;
#[cfg_attr(not(any(windows, test)), allow(dead_code))]
pub const MAX_AGENT_REPORT_BYTES: usize = 4 * 1024;
pub const AGENT_CONNECTION_LIMIT: usize = 32;

/// Fixed, low-cardinality rejection reasons safe for metrics and status.
#[cfg_attr(not(windows), allow(dead_code))]
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, thiserror::Error)]
#[serde(rename_all = "snake_case")]
pub enum AgentRejectReason {
    #[error("pipe peer identity unavailable")]
    PeerIdentity,
    #[error("session 0 is not interactive")]
    SessionZero,
    #[error("claimed session does not match pipe peer")]
    SessionMismatch,
    #[error("incompatible agent protocol")]
    ProtocolMismatch,
    #[error("agent and service versions differ")]
    VersionMismatch,
    #[error("agent sequence must be positive")]
    ZeroSequence,
    #[error("superseded or replayed agent report")]
    ReplayOrSuperseded,
    #[error("agent sample is from the future")]
    FutureSample,
    #[error("agent sample is stale")]
    StaleSample,
    #[error("malformed agent report")]
    Malformed,
    #[error("agent report exceeds the frame limit")]
    Oversized,
    #[error("incomplete agent report")]
    Incomplete,
    #[error("empty agent report")]
    Empty,
    #[error("agent report timed out")]
    Timeout,
    #[error("agent channel I/O failed")]
    ChannelIo,
}

impl AgentRejectReason {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::PeerIdentity => "peer_identity",
            Self::SessionZero => "session_zero",
            Self::SessionMismatch => "session_mismatch",
            Self::ProtocolMismatch => "protocol_mismatch",
            Self::VersionMismatch => "version_mismatch",
            Self::ZeroSequence => "zero_sequence",
            Self::ReplayOrSuperseded => "replay_or_superseded",
            Self::FutureSample => "future_sample",
            Self::StaleSample => "stale_sample",
            Self::Malformed => "malformed",
            Self::Oversized => "oversized",
            Self::Incomplete => "incomplete",
            Self::Empty => "empty",
            Self::Timeout => "timeout",
            Self::ChannelIo => "channel_io",
        }
    }
}

#[derive(Debug, thiserror::Error)]
pub enum FrameError {
    #[error(transparent)]
    Io(#[from] std::io::Error),
    #[error("{0}")]
    Rejected(AgentRejectReason),
}

impl FrameError {
    #[cfg_attr(not(windows), allow(dead_code))]
    pub fn reason(&self) -> AgentRejectReason {
        match self {
            Self::Io(_) => AgentRejectReason::ChannelIo,
            Self::Rejected(reason) => *reason,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct AgentReport {
    /// Wire contract version. Incompatible reporters are ignored safely.
    pub protocol_version: u32,
    /// Strictly increasing within one pipe connection.
    pub sequence: u64,
    /// Windows session containing the reporting process.
    pub session_id: u32,
    /// Seconds since the last input in that session, not session 0.
    pub idle_seconds: u64,
    /// Foreground executable's file stem, without `.exe`.
    pub foreground: Option<String>,
    /// Same Cargo package version for foreground and windowless entry points.
    pub agent_version: String,
    /// UTC milliseconds when the sample was taken. The service uses this,
    /// rather than arrival time alone, so buffered old reports cannot relax
    /// the owner veto after a stalled connection catches up.
    pub sampled_at_unix_ms: u64,
}

#[cfg_attr(not(any(windows, test)), allow(dead_code))]
pub fn unix_time_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis()
        .try_into()
        .unwrap_or(u64::MAX)
}

/// Validate facts the service can establish independently of the payload.
/// The pipe peer's session comes from the kernel, not from this report.
#[cfg_attr(not(any(windows, test)), allow(dead_code))]
pub fn validate_report(
    report: &AgentReport,
    peer_session_id: u32,
    now_unix_ms: u64,
    fresh_for: Duration,
) -> Result<(Duration, Duration), AgentRejectReason> {
    if peer_session_id == 0 {
        return Err(AgentRejectReason::SessionZero);
    }
    if report.session_id != peer_session_id {
        return Err(AgentRejectReason::SessionMismatch);
    }
    if report.protocol_version != AGENT_PROTOCOL_VERSION {
        return Err(AgentRejectReason::ProtocolMismatch);
    }
    if report.sequence == 0 {
        return Err(AgentRejectReason::ZeroSequence);
    }
    if report.agent_version != env!("CARGO_PKG_VERSION") {
        return Err(AgentRejectReason::VersionMismatch);
    }
    if report.sampled_at_unix_ms > now_unix_ms {
        return Err(AgentRejectReason::FutureSample);
    }
    let age = Duration::from_millis(now_unix_ms - report.sampled_at_unix_ms);
    if age >= fresh_for {
        return Err(AgentRejectReason::StaleSample);
    }
    Ok((fresh_for - age, age))
}

/// Read one newline-delimited frame without allowing attacker-controlled
/// input to grow a buffer beyond the protocol limit.
#[cfg_attr(not(any(windows, test)), allow(dead_code))]
pub async fn read_frame<R: AsyncBufRead + Unpin>(
    reader: &mut R,
) -> Result<Option<Vec<u8>>, FrameError> {
    let mut frame = Vec::new();
    loop {
        let available = reader.fill_buf().await?;
        if available.is_empty() {
            if !frame.is_empty() {
                return Err(FrameError::Rejected(AgentRejectReason::Incomplete));
            }
            return Ok(None);
        }

        if let Some(newline) = available.iter().position(|byte| *byte == b'\n') {
            if frame.len() + newline > MAX_AGENT_REPORT_BYTES {
                return Err(FrameError::Rejected(AgentRejectReason::Oversized));
            }
            frame.extend_from_slice(&available[..newline]);
            reader.consume(newline + 1);
            if frame.last() == Some(&b'\r') {
                frame.pop();
            }
            return Ok(Some(frame));
        }

        if frame.len() + available.len() > MAX_AGENT_REPORT_BYTES {
            return Err(FrameError::Rejected(AgentRejectReason::Oversized));
        }
        let consumed = available.len();
        frame.extend_from_slice(available);
        reader.consume(consumed);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tokio::io::BufReader;

    fn report() -> AgentReport {
        AgentReport {
            protocol_version: AGENT_PROTOCOL_VERSION,
            sequence: 1,
            session_id: 7,
            idle_seconds: 30,
            foreground: Some("Code".into()),
            agent_version: env!("CARGO_PKG_VERSION").into(),
            sampled_at_unix_ms: 10_000,
        }
    }

    #[test]
    fn report_must_match_peer_protocol_version_and_freshness() {
        let fresh_for = Duration::from_secs(15);
        assert_eq!(
            validate_report(&report(), 7, 12_000, fresh_for).unwrap(),
            (Duration::from_secs(13), Duration::from_secs(2))
        );

        let mut wrong_session = report();
        wrong_session.session_id = 8;
        assert_eq!(
            validate_report(&wrong_session, 7, 12_000, fresh_for).unwrap_err(),
            AgentRejectReason::SessionMismatch
        );

        let mut wrong_protocol = report();
        wrong_protocol.protocol_version += 1;
        assert_eq!(
            validate_report(&wrong_protocol, 7, 12_000, fresh_for).unwrap_err(),
            AgentRejectReason::ProtocolMismatch
        );

        let mut zero_sequence = report();
        zero_sequence.sequence = 0;
        assert!(validate_report(&zero_sequence, 7, 12_000, fresh_for).is_err());

        let mut wrong_version = report();
        wrong_version.agent_version = "0.0.0".into();
        assert_eq!(
            validate_report(&wrong_version, 7, 12_000, fresh_for).unwrap_err(),
            AgentRejectReason::VersionMismatch
        );

        assert!(validate_report(&report(), 7, 25_000, fresh_for).is_err());
        assert!(validate_report(&report(), 7, 9_999, fresh_for).is_err());
        assert!(validate_report(&report(), 0, 12_000, fresh_for).is_err());
    }

    #[tokio::test]
    async fn frame_reader_is_bounded_and_preserves_following_frames() {
        let bytes = b"first\r\nsecond\n";
        let mut reader = BufReader::with_capacity(3, &bytes[..]);
        assert_eq!(read_frame(&mut reader).await.unwrap().unwrap(), b"first");
        assert_eq!(read_frame(&mut reader).await.unwrap().unwrap(), b"second");
        assert!(read_frame(&mut reader).await.unwrap().is_none());

        let oversized = vec![b'x'; MAX_AGENT_REPORT_BYTES + 1];
        let mut reader = BufReader::with_capacity(128, oversized.as_slice());
        assert_eq!(
            read_frame(&mut reader).await.unwrap_err().reason(),
            AgentRejectReason::Oversized
        );

        let mut incomplete = BufReader::new(&b"partial"[..]);
        assert_eq!(
            read_frame(&mut incomplete).await.unwrap_err().reason(),
            AgentRejectReason::Incomplete
        );
    }
}
