//! Service side of the local admin channel: a named pipe on which the
//! service learns from the kernel which account is asking before it changes
//! lending.
//!
//! One request per connection: the client writes one JSON line, the service
//! reads it, takes the identity of the account that wrote it from the pipe
//! (`ImpersonateNamedPipeClient`), answers with one JSON line and closes.
//! The rules for who may do what are platform-neutral and live in
//! [`vacancyd::local_admin::permits`]; this module only proves the account.

use std::sync::Arc;
use std::time::Duration;

use anyhow::{Context, Result};
use tokio::io::{AsyncReadExt, AsyncWriteExt, BufReader};
use tokio::sync::Semaphore;
use tracing::{debug, info, warn};
use vacancyd::local_admin::{
    read_frame, Caller, RefusalReason, Request, Response, MAX_REQUEST_BYTES, PIPE_NAME,
    PROTOCOL_VERSION,
};

use super::server::SecurityDescriptor;
use super::INTERACTIVE_PIPE_ACCESS;
use crate::state::AppState;

/// Concurrent admin connections. A person clicks these; a small bound keeps
/// a local process from tying up the service with idle connections.
const ADMIN_CONNECTION_LIMIT: usize = 8;
/// Bounds reading a request and writing its answer.
const REQUEST_TIMEOUT: Duration = Duration::from_secs(5);

/// Keep the admin channel available for the life of the service.
pub async fn serve(state: Arc<AppState>) {
    super::supervise::supervise("local admin", || run(state.clone())).await
}

async fn run(state: Arc<AppState>) -> Result<()> {
    let mut server = create_instance(true).map_err(first_instance_error)?;
    let permits = Arc::new(Semaphore::new(ADMIN_CONNECTION_LIMIT));
    let mut connections = tokio::task::JoinSet::new();
    let _listening = super::supervise::Listening::mark(&state.admin_channel_listening);
    info!(pipe = PIPE_NAME, "local admin channel listening");
    loop {
        while connections.try_join_next().is_some() {}
        let permit = permits
            .clone()
            .acquire_owned()
            .await
            .expect("admin connection semaphore closed");
        server.connect().await?;
        let connected = server;
        server = create_instance(false)?;
        let st = state.clone();
        connections.spawn(async move {
            let _permit = permit;
            if let Err(e) = serve_one(&st, connected).await {
                debug!(error = %e, "admin connection ended");
            }
        });
    }
}

async fn serve_one(
    state: &AppState,
    pipe: tokio::net::windows::named_pipe::NamedPipeServer,
) -> Result<()> {
    let mut stream = BufReader::new(pipe);
    let Some(frame) =
        tokio::time::timeout(REQUEST_TIMEOUT, read_frame(&mut stream, MAX_REQUEST_BYTES))
            .await
            .context("admin request timed out")??
    else {
        return Ok(());
    };
    // Windows can report the client's identity only after data has been
    // read, and then reports the account that wrote it.
    let caller = peer_caller(stream.get_ref())?;
    let response = match serde_json::from_slice::<Request<serde_json::Value>>(&frame) {
        Ok(request) if request.protocol_version == PROTOCOL_VERSION => {
            crate::api::perform_local_admin(state, &caller, request.action)
        }
        _ => {
            warn!("local admin request was not understood");
            Response::Refused {
                reason: RefusalReason::InvalidRequest,
            }
        }
    };
    let mut line = serde_json::to_vec(&response)?;
    line.push(b'\n');
    tokio::time::timeout(REQUEST_TIMEOUT, stream.write_all(&line))
        .await
        .context("admin response timed out")??;
    // Hold the instance until the client has read the answer and closed, so
    // closing this end cannot discard it.
    let _ = tokio::time::timeout(REQUEST_TIMEOUT, stream.read(&mut [0u8; 1])).await;
    Ok(())
}

/// The account that wrote the frame just read, from its token.
///
/// The client opens the pipe at identification level, which lets the
/// service read the token but never act as the client. The impersonation
/// lasts only for `OpenThreadToken` on this thread, with no await in
/// between, so no other task can run under it.
fn peer_caller(pipe: &tokio::net::windows::named_pipe::NamedPipeServer) -> Result<Caller> {
    use std::os::windows::io::AsRawHandle;
    use windows_sys::Win32::Security::{RevertToSelf, TOKEN_QUERY};
    use windows_sys::Win32::System::Pipes::ImpersonateNamedPipeClient;
    use windows_sys::Win32::System::Threading::{GetCurrentThread, OpenThreadToken};

    // SAFETY: the connected server instance's live handle; a request frame
    // has been read from it.
    if unsafe { ImpersonateNamedPipeClient(pipe.as_raw_handle() as _) } == 0 {
        return Err(std::io::Error::last_os_error()).context("identifying the admin pipe client");
    }
    let mut token = std::ptr::null_mut();
    // SAFETY: the current thread's pseudo handle and a valid out-parameter.
    // OpenAsSelf checks access as the service, because an
    // identification-level token cannot open objects itself.
    let opened = unsafe { OpenThreadToken(GetCurrentThread(), TOKEN_QUERY, 1, &mut token) };
    let open_error = std::io::Error::last_os_error();
    // SAFETY: ends the impersonation started above, on the same thread.
    if unsafe { RevertToSelf() } == 0 {
        // This worker thread would otherwise keep running service tasks
        // under the client's identity.
        std::process::abort();
    }
    if opened == 0 {
        return Err(open_error).context("reading the admin pipe client's token");
    }
    let token = Token(token);
    let (sid, system) = token.user()?;
    let administrator = system || token.is_administrator()?;
    Ok(Caller::Account {
        account: sid,
        administrator,
    })
}

struct Token(windows_sys::Win32::Foundation::HANDLE);

impl Token {
    /// The token's user as a string SID, and whether it is LocalSystem.
    fn user(&self) -> Result<(String, bool)> {
        use windows_sys::Win32::Foundation::LocalFree;
        use windows_sys::Win32::Security::Authorization::ConvertSidToStringSidW;
        use windows_sys::Win32::Security::{
            GetTokenInformation, IsWellKnownSid, TokenUser, WinLocalSystemSid, TOKEN_USER,
        };

        let mut needed = 0u32;
        // SAFETY: a size query with no buffer; it fails and reports the size.
        unsafe { GetTokenInformation(self.0, TokenUser, std::ptr::null_mut(), 0, &mut needed) };
        anyhow::ensure!(
            needed as usize >= std::mem::size_of::<TOKEN_USER>() && needed <= 4096,
            "unexpected token user size {needed}"
        );
        // `usize` storage aligns the TOKEN_USER and the SID that follows it.
        let mut storage = vec![0usize; (needed as usize).div_ceil(std::mem::size_of::<usize>())];
        let bytes = (storage.len() * std::mem::size_of::<usize>()) as u32;
        // SAFETY: the buffer is aligned and at least `needed` bytes long.
        if unsafe {
            GetTokenInformation(
                self.0,
                TokenUser,
                storage.as_mut_ptr().cast(),
                bytes,
                &mut needed,
            )
        } == 0
        {
            return Err(std::io::Error::last_os_error()).context("reading the token user");
        }
        // SAFETY: the successful query initialized a TOKEN_USER at the start
        // of the buffer, whose SID pointer points inside the same buffer.
        let sid = unsafe { (*storage.as_ptr().cast::<TOKEN_USER>()).User.Sid };
        // SAFETY: `sid` is valid while `storage` lives.
        let system = unsafe { IsWellKnownSid(sid, WinLocalSystemSid) } != 0;
        let mut text = std::ptr::null_mut();
        // SAFETY: valid SID; Windows allocates `text`, released below.
        if unsafe { ConvertSidToStringSidW(sid, &mut text) } == 0 {
            return Err(std::io::Error::last_os_error()).context("formatting the token user");
        }
        // SAFETY: a NUL-terminated string from the successful conversion.
        let converted = unsafe {
            let length = (0..).take_while(|i| *text.add(*i) != 0).count();
            String::from_utf16(std::slice::from_raw_parts(text, length))
        };
        // SAFETY: allocated by ConvertSidToStringSidW.
        unsafe { LocalFree(text.cast()) };
        Ok((converted.context("token user is not valid UTF-16")?, system))
    }

    /// Whether `BUILTIN\Administrators` is enabled in this token. Under UAC
    /// a signed-in administrator's everyday token holds the group for deny
    /// only, so this is true only for an elevated caller.
    fn is_administrator(&self) -> Result<bool> {
        use windows_sys::Win32::Security::{
            CheckTokenMembership, CreateWellKnownSid, WinBuiltinAdministratorsSid,
            SECURITY_MAX_SID_SIZE,
        };

        // `u32` storage gives the SID its required alignment.
        let mut sid = [0u32; (SECURITY_MAX_SID_SIZE as usize).div_ceil(4)];
        let mut size = std::mem::size_of_val(&sid) as u32;
        // SAFETY: the buffer holds SECURITY_MAX_SID_SIZE bytes, as `size` says.
        if unsafe {
            CreateWellKnownSid(
                WinBuiltinAdministratorsSid,
                std::ptr::null_mut(),
                sid.as_mut_ptr().cast(),
                &mut size,
            )
        } == 0
        {
            return Err(std::io::Error::last_os_error()).context("building the Administrators SID");
        }
        let mut member = 0;
        // SAFETY: an impersonation token opened with TOKEN_QUERY, a valid SID
        // and a valid out-parameter.
        if unsafe { CheckTokenMembership(self.0, sid.as_mut_ptr().cast(), &mut member) } == 0 {
            return Err(std::io::Error::last_os_error())
                .context("checking Administrators membership");
        }
        Ok(member != 0)
    }
}

impl Drop for Token {
    fn drop(&mut self) {
        // SAFETY: this wrapper uniquely owns the token handle.
        unsafe { windows_sys::Win32::Foundation::CloseHandle(self.0) };
    }
}

/// Create one admin pipe instance with the agent pipe's DACL: LocalSystem
/// and Administrators full control, interactive users read and write data.
/// Any signed-in user may connect and pause; what else a caller may do is
/// decided from its token, not from reaching the pipe.
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
    // A first instance that already exists belongs to someone else, which a
    // client would then be talking to; refuse rather than join it.
    opts.first_pipe_instance(first);
    opts.reject_remote_clients(true);
    opts.max_instances(ADMIN_CONNECTION_LIMIT + 1);
    // SAFETY: `attrs` is a well-formed SECURITY_ATTRIBUTES whose descriptor
    // is valid for the duration of this call.
    let server = unsafe {
        opts.create_with_security_attributes_raw(PIPE_NAME, &mut attrs as *mut _ as *mut _)?
    };
    Ok(server)
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
            "creating {PIPE_NAME}: another vacancyd, usually the installed service, may already own it; the local admin channel stays down while it does"
        ))
    } else {
        error
    }
}
