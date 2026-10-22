//! Service side of the agent channel: an ACL'd named pipe that accepts
//! reports from interactive sessions.

use std::sync::Arc;

use anyhow::Result;
use tokio::io::BufReader;
use tokio::sync::Semaphore;
use tracing::{debug, info, warn};

use super::{
    read_frame, unix_time_ms, validate_report, AgentRejectReason, AgentReport,
    AGENT_CONNECTION_LIMIT, INTERACTIVE_PIPE_ACCESS, PIPE_NAME,
};
use crate::state::AppState;

/// Keep the agent channel available for the life of the service, restarting
/// it with backoff if the pipe server stops.
pub async fn serve(state: Arc<AppState>) {
    super::supervise::supervise("agent", || run(state.clone())).await
}

/// Accept agent connections until the pipe server fails, one task per
/// connection.
///
/// Named-pipe servers are single-connection: create an instance, wait for a
/// client, then immediately create the next instance so there is always one
/// listening. That is the documented pattern and it is why the loop below
/// creates before it awaits.
async fn run(state: Arc<AppState>) -> Result<()> {
    state
        .agent_channel_listening
        .store(true, std::sync::atomic::Ordering::Relaxed);
    let result = run_inner(state.clone()).await;
    state
        .agent_channel_listening
        .store(false, std::sync::atomic::Ordering::Relaxed);
    result
}

async fn run_inner(state: Arc<AppState>) -> Result<()> {
    let mut server = create_instance(true).map_err(first_instance_error)?;
    let permits = Arc::new(Semaphore::new(AGENT_CONNECTION_LIMIT));
    // Connections belong to this run. When it fails, dropping the set closes
    // every instance, so a restart can create the pipe as its first instance
    // again instead of joining a pipe someone else may have created.
    let mut connections = tokio::task::JoinSet::new();
    info!(pipe = PIPE_NAME, "agent channel listening");
    loop {
        while connections.try_join_next().is_some() {}
        let permit = permits
            .clone()
            .acquire_owned()
            .await
            .expect("agent connection semaphore closed");
        server.connect().await?;
        let connected = server;
        // Have the next instance ready before servicing this one.
        server = match create_instance(false) {
            Ok(s) => s,
            Err(e) => {
                warn!(error = %e, "could not create next pipe instance");
                return Err(e);
            }
        };
        let st = state.clone();
        // Assign authority order in the accept loop. Handler scheduling can
        // run these tasks in either order, so allocating inside `serve_one`
        // would let an earlier connection receive a later generation.
        let generation = state.begin_agent_connection();
        connections.spawn(async move {
            let _permit = permit;
            if let Err(e) = serve_one(st, connected, generation).await {
                debug!(error = %e, "agent connection ended");
            }
        });
    }
}

async fn serve_one(
    state: Arc<AppState>,
    pipe: tokio::net::windows::named_pipe::NamedPipeServer,
    generation: u64,
) -> Result<()> {
    let _connection = ActiveConnection::new(&state);
    let peer_session_id =
        peer_session_id(&pipe).map_err(|_| reject(&state, AgentRejectReason::PeerIdentity))?;
    if peer_session_id == 0 {
        return Err(reject(&state, AgentRejectReason::SessionZero));
    }
    let result = serve_connected(&state, pipe, peer_session_id, generation).await;
    state.end_agent_connection(peer_session_id, generation);
    result
}

async fn serve_connected(
    state: &AppState,
    pipe: tokio::net::windows::named_pipe::NamedPipeServer,
    peer_session_id: u32,
    generation: u64,
) -> Result<()> {
    let fresh_for = state.agent_report_fresh_for();
    let mut reader = BufReader::new(pipe);
    loop {
        let frame = match tokio::time::timeout(fresh_for, read_frame(&mut reader)).await {
            Ok(Ok(frame)) => frame,
            Ok(Err(error)) => return Err(reject(state, error.reason())),
            Err(_) => return Err(reject(state, AgentRejectReason::Timeout)),
        };
        let Some(frame) = frame else {
            return Ok(());
        };
        if frame.iter().all(u8::is_ascii_whitespace) {
            return Err(reject(state, AgentRejectReason::Empty));
        }
        let report: AgentReport = serde_json::from_slice(&frame)
            .map_err(|_| reject(state, AgentRejectReason::Malformed))?;
        let received_at = std::time::Instant::now();
        let (valid_for, sample_age) =
            validate_report(&report, peer_session_id, unix_time_ms(), fresh_for)
                .map_err(|reason| reject(state, reason))?;
        debug!(
            session = peer_session_id,
            idle = report.idle_seconds,
            "verified agent report"
        );
        if !state.record_agent_report(report, generation, valid_for, sample_age, received_at) {
            return Err(reject(state, AgentRejectReason::ReplayOrSuperseded));
        }
        state.metrics.agent_reports_total.inc();
    }
}

fn reject(state: &AppState, reason: AgentRejectReason) -> anyhow::Error {
    state.record_agent_rejection(reason);
    anyhow::Error::new(reason)
}

struct ActiveConnection(prometheus::IntGauge);

impl ActiveConnection {
    fn new(state: &AppState) -> Self {
        let gauge = state.metrics.agent_active_connections.clone();
        gauge.inc();
        Self(gauge)
    }
}

impl Drop for ActiveConnection {
    fn drop(&mut self) {
        self.0.dec();
    }
}

fn peer_session_id(pipe: &tokio::net::windows::named_pipe::NamedPipeServer) -> Result<u32> {
    use std::os::windows::io::AsRawHandle;
    use windows_sys::Win32::System::Pipes::GetNamedPipeClientSessionId;

    let mut session_id = 0u32;
    // SAFETY: this is the connected server instance's live kernel handle and
    // `session_id` is a valid out-parameter for the duration of the call.
    let ok = unsafe { GetNamedPipeClientSessionId(pipe.as_raw_handle() as _, &mut session_id) };
    if ok == 0 {
        anyhow::bail!(
            "reading agent pipe peer session: {}",
            std::io::Error::last_os_error()
        );
    }
    Ok(session_id)
}

/// Create one pipe instance, granting interactive users access.
///
/// The default DACL for a pipe created by LocalSystem does not admit ordinary
/// users, so an unprivileged agent could not connect. This grants exactly
/// what is needed and no more:
///
/// - `SY` (LocalSystem) and `BA` (Administrators): full control
/// - `IU` (Interactive Users, including local RDP sessions): read, and write
///   data, but not create pipe instances
///
/// Interactive users only — not `WD`/Everyone — so a background service or
/// network logon cannot post presence. Remote pipe clients are separately
/// disabled while local RDP sessions remain supported.
fn create_instance(first: bool) -> Result<tokio::net::windows::named_pipe::NamedPipeServer> {
    use tokio::net::windows::named_pipe::ServerOptions;

    let sd = SecurityDescriptor::from_sddl(&format!(
        "D:(A;;GA;;;SY)(A;;GA;;;BA)(A;;{INTERACTIVE_PIPE_ACCESS:#x};;;IU)"
    ))?;
    let mut attrs = windows_sys::Win32::Security::SECURITY_ATTRIBUTES {
        nLength: std::mem::size_of::<windows_sys::Win32::Security::SECURITY_ATTRIBUTES>() as u32,
        lpSecurityDescriptor: sd.0,
        bInheritHandle: 0,
    };

    let mut opts = ServerOptions::new();
    opts.first_pipe_instance(first);
    opts.reject_remote_clients(true);
    opts.max_instances(AGENT_CONNECTION_LIMIT + 1);
    // SAFETY: `attrs` is a well-formed SECURITY_ATTRIBUTES whose descriptor
    // is valid for the duration of this call; `sd` is dropped afterwards and
    // the kernel has copied what it needs by then.
    let server = unsafe {
        opts.create_with_security_attributes_raw(PIPE_NAME, &mut attrs as *mut _ as *mut _)?
    };
    Ok(server)
}

/// A security descriptor built from SDDL, freed on drop.
pub(super) struct SecurityDescriptor(pub(super) *mut std::ffi::c_void);

impl SecurityDescriptor {
    pub(super) fn from_sddl(sddl: &str) -> Result<Self> {
        use windows_sys::Win32::Security::Authorization::ConvertStringSecurityDescriptorToSecurityDescriptorW;

        let wide: Vec<u16> = sddl.encode_utf16().chain(std::iter::once(0)).collect();
        let mut psd: *mut std::ffi::c_void = std::ptr::null_mut();
        // SAFETY: `wide` is a NUL-terminated UTF-16 string; `psd` receives an
        // allocation owned by us and released in `Drop`.
        let ok = unsafe {
            ConvertStringSecurityDescriptorToSecurityDescriptorW(
                wide.as_ptr(),
                1, // SDDL_REVISION_1
                &mut psd,
                std::ptr::null_mut(),
            )
        };
        if ok == 0 || psd.is_null() {
            anyhow::bail!(
                "building pipe security descriptor: {}",
                std::io::Error::last_os_error()
            );
        }
        Ok(Self(psd))
    }
}

impl Drop for SecurityDescriptor {
    fn drop(&mut self) {
        if !self.0.is_null() {
            // SAFETY: allocated by ConvertStringSecurityDescriptorToSecurityDescriptorW.
            unsafe { windows_sys::Win32::Foundation::LocalFree(self.0) };
        }
    }
}

/// The first instance is created with FILE_FLAG_FIRST_PIPE_INSTANCE, so
/// "access denied" here usually means another vacancyd, normally the
/// installed service, already owns the pipe. The channel is optional and
/// retried; this makes the warning say why.
fn first_instance_error(error: anyhow::Error) -> anyhow::Error {
    let denied = error
        .chain()
        .filter_map(|cause| cause.downcast_ref::<std::io::Error>())
        .any(|io| io.kind() == std::io::ErrorKind::PermissionDenied);
    if denied {
        error.context(format!(
            "creating {PIPE_NAME}: another vacancyd, usually the installed service, may already own it; the agent channel stays down while it does"
        ))
    } else {
        error
    }
}
