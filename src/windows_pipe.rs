//! Client side of the service's named pipes: open one with the rights its
//! DACL grants, and prove the server end is the running vacancyd service.
//!
//! Shared by the session reporter (`vacancyd-session`, `vacancyd agent`),
//! the owner console and the `vacancyd admin-request` helper, which all talk
//! to a LocalSystem service across the session boundary.

use anyhow::{Context, Result};

const SERVICE_NAME: &str = "vacancyd";

/// Rights the service's pipes grant interactive users, and the rights their
/// clients request: `FILE_GENERIC_READ | FILE_WRITE_DATA`.
///
/// Not `GENERIC_WRITE`: for a pipe that includes `FILE_APPEND_DATA`, the same
/// bit as `FILE_CREATE_PIPE_INSTANCE`, which would let any interactive user
/// add server instances to the service's pipe or use up its instance limit.
pub const INTERACTIVE_PIPE_ACCESS: u32 = 0x0012_008B;

/// Open the pipe with exactly the rights its DACL grants interactive users.
///
/// Tokio's `ClientOptions` requests `GENERIC_READ | GENERIC_WRITE`, and
/// `GENERIC_WRITE` includes the pipe-instance creation right the service no
/// longer grants. Identification-level quality of service matches Tokio's
/// default: the service may identify this client but never impersonate it.
pub fn open_pipe(
    pipe_name: &str,
) -> std::io::Result<tokio::net::windows::named_pipe::NamedPipeClient> {
    use std::os::windows::io::RawHandle;
    use tokio::net::windows::named_pipe::NamedPipeClient;
    use windows_sys::Win32::Foundation::INVALID_HANDLE_VALUE;
    use windows_sys::Win32::Storage::FileSystem::{
        CreateFileW, FILE_FLAG_OVERLAPPED, OPEN_EXISTING, SECURITY_IDENTIFICATION,
        SECURITY_SQOS_PRESENT,
    };

    let name: Vec<u16> = pipe_name.encode_utf16().chain(std::iter::once(0)).collect();
    // SAFETY: `name` is NUL-terminated and outlives the call; no security
    // attributes or template handle are supplied.
    let handle = unsafe {
        CreateFileW(
            name.as_ptr(),
            INTERACTIVE_PIPE_ACCESS,
            0,
            std::ptr::null(),
            OPEN_EXISTING,
            FILE_FLAG_OVERLAPPED | SECURITY_SQOS_PRESENT | SECURITY_IDENTIFICATION,
            std::ptr::null_mut(),
        )
    };
    if handle == INVALID_HANDLE_VALUE {
        return Err(std::io::Error::last_os_error());
    }
    // SAFETY: a freshly opened, overlapped pipe handle that nothing else owns.
    // On failure Tokio closes it.
    unsafe { NamedPipeClient::from_raw_handle(handle as RawHandle) }
}

/// Establish that this pipe instance belongs to the running LocalSystem service.
///
/// A pipe name is not an identity: another local process can create the name
/// while the service is stopped. The reporter therefore binds the connected
/// pipe's kernel-reported PID and session to the Service Control Manager record
/// before it sends any presence data.
pub fn authenticate_server(
    pipe: &tokio::net::windows::named_pipe::NamedPipeClient,
    allow_foreground_daemon: bool,
) -> Result<()> {
    use std::ptr::null;
    use windows_sys::Win32::System::Services::{
        OpenSCManagerW, OpenServiceW, SC_MANAGER_CONNECT, SERVICE_QUERY_CONFIG,
        SERVICE_QUERY_STATUS,
    };

    let before = pipe_server_identity(pipe)?;
    anyhow::ensure!(before.0 != 0, "agent pipe server PID is unavailable");
    if before.1 != 0 {
        anyhow::ensure!(
            allow_foreground_daemon,
            "agent pipe server is not in session 0"
        );
        return authenticate_foreground_daemon(pipe, before);
    }

    let manager = ServiceHandle::new(
        // SAFETY: null machine and database names select the local active SCM database.
        unsafe { OpenSCManagerW(null(), null(), SC_MANAGER_CONNECT) },
        "opening the local Service Control Manager",
    )?;
    let service_name: Vec<u16> = SERVICE_NAME
        .encode_utf16()
        .chain(std::iter::once(0))
        .collect();
    let service = ServiceHandle::new(
        // SAFETY: `service_name` is live and NUL-terminated; `manager` is valid.
        unsafe {
            OpenServiceW(
                manager.0,
                service_name.as_ptr(),
                SERVICE_QUERY_CONFIG | SERVICE_QUERY_STATUS,
            )
        },
        "opening the vacancyd service for identity verification",
    )?;

    let account = service_account(&service)?;
    let first_status = service_status(&service)?;
    validate_server_identity(before, &first_status, &account)?;

    // Sample both authorities again around the checks. If the service stopped,
    // restarted, or the pipe changed underneath authentication, reject this
    // connection and let the normal retry loop establish a fresh one.
    let after = pipe_server_identity(pipe)?;
    let second_status = service_status(&service)?;
    anyhow::ensure!(before == after, "agent pipe server identity changed");
    validate_server_identity(after, &second_status, &account)?;
    Ok(())
}

/// The interactive `vacancyd agent` command can diagnose `vacancyd run`
/// without installing the service. That explicit foreground mode accepts only
/// the same executable in the same Windows session. The windowless startup
/// reporter never enables this path.
fn authenticate_foreground_daemon(
    pipe: &tokio::net::windows::named_pipe::NamedPipeClient,
    before: (u32, u32),
) -> Result<()> {
    anyhow::ensure!(
        before.1 == current_session_id(),
        "foreground daemon is in another session"
    );
    let current = std::env::current_exe().context("reading the reporter executable path")?;
    let server = process_path(before.0)?;
    anyhow::ensure!(
        current
            .to_string_lossy()
            .eq_ignore_ascii_case(&server.to_string_lossy()),
        "foreground daemon is not this vacancyd executable"
    );
    anyhow::ensure!(
        pipe_server_identity(pipe)? == before,
        "foreground daemon identity changed"
    );
    Ok(())
}

fn process_path(pid: u32) -> Result<std::path::PathBuf> {
    use windows_sys::Win32::{
        Foundation::CloseHandle,
        System::Threading::{
            OpenProcess, QueryFullProcessImageNameW, PROCESS_QUERY_LIMITED_INFORMATION,
        },
    };

    // SAFETY: the handle grants query-only access and is closed below.
    let process = unsafe { OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, 0, pid) };
    anyhow::ensure!(
        !process.is_null(),
        "opening foreground daemon process: {}",
        std::io::Error::last_os_error()
    );
    let mut buffer = vec![0u16; 32768];
    let mut length = buffer.len() as u32;
    // SAFETY: buffer length is supplied and the process handle is valid.
    let ok = unsafe { QueryFullProcessImageNameW(process, 0, buffer.as_mut_ptr(), &mut length) };
    let query_error = (ok == 0).then(std::io::Error::last_os_error);
    // SAFETY: this function uniquely owns the successful process handle.
    unsafe { CloseHandle(process) };
    if let Some(error) = query_error {
        anyhow::bail!("reading foreground daemon path: {error}");
    }
    Ok(std::path::PathBuf::from(String::from_utf16(
        &buffer[..length as usize],
    )?))
}

fn pipe_server_identity(
    pipe: &tokio::net::windows::named_pipe::NamedPipeClient,
) -> Result<(u32, u32)> {
    use std::os::windows::io::AsRawHandle;
    use windows_sys::Win32::System::Pipes::{
        GetNamedPipeServerProcessId, GetNamedPipeServerSessionId,
    };

    let handle = pipe.as_raw_handle() as _;
    let mut process_id = 0u32;
    let mut session_id = 0u32;
    // SAFETY: `handle` is this client's live pipe handle and both outputs live
    // for the duration of their respective calls.
    if unsafe { GetNamedPipeServerProcessId(handle, &mut process_id) } == 0 {
        return Err(std::io::Error::last_os_error())
            .context("reading the agent pipe server process");
    }
    // SAFETY: same live pipe handle and valid out-parameter as above.
    if unsafe { GetNamedPipeServerSessionId(handle, &mut session_id) } == 0 {
        return Err(std::io::Error::last_os_error())
            .context("reading the agent pipe server session");
    }
    Ok((process_id, session_id))
}

fn service_status(
    service: &ServiceHandle,
) -> Result<windows_sys::Win32::System::Services::SERVICE_STATUS_PROCESS> {
    use std::mem::{size_of, MaybeUninit};
    use windows_sys::Win32::System::Services::{
        QueryServiceStatusEx, SC_STATUS_PROCESS_INFO, SERVICE_STATUS_PROCESS,
    };

    let mut status = MaybeUninit::<SERVICE_STATUS_PROCESS>::zeroed();
    let mut needed = 0u32;
    // SAFETY: the output points to a correctly sized, aligned status value;
    // successful return means Windows initialized it completely.
    let ok = unsafe {
        QueryServiceStatusEx(
            service.0,
            SC_STATUS_PROCESS_INFO,
            status.as_mut_ptr().cast(),
            size_of::<SERVICE_STATUS_PROCESS>() as u32,
            &mut needed,
        )
    };
    if ok == 0 {
        return Err(std::io::Error::last_os_error())
            .context("querying the vacancyd service process");
    }
    // SAFETY: QueryServiceStatusEx succeeded and filled the whole structure.
    Ok(unsafe { status.assume_init() })
}

fn service_account(service: &ServiceHandle) -> Result<String> {
    use std::mem::size_of;
    use windows_sys::Win32::System::Services::{QueryServiceConfigW, QUERY_SERVICE_CONFIGW};

    // QueryServiceConfigW documents an 8 KiB maximum. `usize` storage gives
    // the returned QUERY_SERVICE_CONFIGW and its embedded pointers sufficient
    // alignment, unlike a Vec<u8> cast.
    const MAX_SERVICE_CONFIG_BYTES: usize = 8 * 1024;
    let mut storage = vec![0usize; MAX_SERVICE_CONFIG_BYTES.div_ceil(size_of::<usize>())];
    let bytes = (storage.len() * size_of::<usize>()) as u32;
    let mut needed = 0u32;
    // SAFETY: storage is aligned and large enough for the documented maximum;
    // the service handle has SERVICE_QUERY_CONFIG access.
    let ok = unsafe {
        QueryServiceConfigW(
            service.0,
            storage.as_mut_ptr().cast::<QUERY_SERVICE_CONFIGW>(),
            bytes,
            &mut needed,
        )
    };
    if ok == 0 {
        return Err(std::io::Error::last_os_error())
            .context("querying the vacancyd service account");
    }

    // SAFETY: the successful query initialized QUERY_SERVICE_CONFIGW at the
    // start of the aligned storage. The string pointer is bounds-checked below.
    let config = unsafe { &*storage.as_ptr().cast::<QUERY_SERVICE_CONFIGW>() };
    bounded_wide_string(
        config.lpServiceStartName,
        storage.as_ptr().cast(),
        bytes as usize,
    )
    .context("reading the vacancyd service account")
}

fn bounded_wide_string(pointer: *const u16, base: *const u8, bytes: usize) -> Result<String> {
    anyhow::ensure!(!pointer.is_null(), "service account is absent");
    let start = base as usize;
    let end = start
        .checked_add(bytes)
        .context("service configuration buffer overflow")?;
    let address = pointer as usize;
    anyhow::ensure!(
        address >= start && address < end && address.is_multiple_of(std::mem::align_of::<u16>()),
        "service account pointer is outside its configuration buffer"
    );
    let available = (end - address) / std::mem::size_of::<u16>();
    // SAFETY: the pointer is aligned, within `base..base+bytes`, and the slice
    // is capped at that allocation's end.
    let units = unsafe { std::slice::from_raw_parts(pointer, available) };
    let length = units
        .iter()
        .position(|unit| *unit == 0)
        .context("service account is not NUL-terminated")?;
    String::from_utf16(&units[..length]).context("service account is not valid UTF-16")
}

fn validate_server_identity(
    pipe: (u32, u32),
    status: &windows_sys::Win32::System::Services::SERVICE_STATUS_PROCESS,
    account: &str,
) -> Result<()> {
    use windows_sys::Win32::System::Services::{SERVICE_RUNNING, SERVICE_WIN32_OWN_PROCESS};

    anyhow::ensure!(pipe.1 == 0, "agent pipe server is not in session 0");
    anyhow::ensure!(
        status.dwCurrentState == SERVICE_RUNNING,
        "vacancyd service is not running"
    );
    anyhow::ensure!(
        status.dwProcessId != 0 && status.dwProcessId == pipe.0,
        "agent pipe server is not the running vacancyd service"
    );
    anyhow::ensure!(
        status.dwServiceType & SERVICE_WIN32_OWN_PROCESS != 0,
        "vacancyd service is not an own-process service"
    );
    anyhow::ensure!(
        account.eq_ignore_ascii_case("LocalSystem"),
        "vacancyd service is not configured for LocalSystem"
    );
    Ok(())
}

struct ServiceHandle(windows_sys::Win32::System::Services::SC_HANDLE);

impl ServiceHandle {
    fn new(
        handle: windows_sys::Win32::System::Services::SC_HANDLE,
        operation: &'static str,
    ) -> Result<Self> {
        if handle.is_null() {
            return Err(std::io::Error::last_os_error()).context(operation);
        }
        Ok(Self(handle))
    }
}

impl Drop for ServiceHandle {
    fn drop(&mut self) {
        // SAFETY: this wrapper uniquely owns a successful SCM/service handle.
        unsafe {
            windows_sys::Win32::System::Services::CloseServiceHandle(self.0);
        }
    }
}

pub fn current_session_id() -> u32 {
    use windows_sys::Win32::System::RemoteDesktop::ProcessIdToSessionId;
    use windows_sys::Win32::System::Threading::GetCurrentProcessId;
    let mut id: u32 = 0;
    // SAFETY: writes only into `id`; failure leaves it zero.
    unsafe {
        ProcessIdToSessionId(GetCurrentProcessId(), &mut id);
    }
    id
}

#[cfg(test)]
mod tests {
    use super::*;
    use windows_sys::Win32::System::Services::{
        SERVICE_RUNNING, SERVICE_STATUS_PROCESS, SERVICE_WIN32_OWN_PROCESS,
    };

    fn running_service(process_id: u32) -> SERVICE_STATUS_PROCESS {
        SERVICE_STATUS_PROCESS {
            dwServiceType: SERVICE_WIN32_OWN_PROCESS,
            dwCurrentState: SERVICE_RUNNING,
            dwControlsAccepted: 0,
            dwWin32ExitCode: 0,
            dwServiceSpecificExitCode: 0,
            dwCheckPoint: 0,
            dwWaitHint: 0,
            dwProcessId: process_id,
            dwServiceFlags: 0,
        }
    }

    #[test]
    fn session_id_resolves() {
        // An interactive session is never 0; a service is. Either way the
        // call must succeed and not panic.
        let _ = current_session_id();
    }

    #[test]
    fn server_identity_requires_matching_system_service() {
        let status = running_service(42);
        assert!(validate_server_identity((42, 0), &status, "LocalSystem").is_ok());
        assert!(validate_server_identity((41, 0), &status, "LocalSystem").is_err());
        assert!(validate_server_identity((42, 1), &status, "LocalSystem").is_err());
        assert!(validate_server_identity((42, 0), &status, "LocalService").is_err());

        let mut stopped = status;
        stopped.dwCurrentState = 1; // SERVICE_STOPPED
        assert!(validate_server_identity((42, 0), &stopped, "LocalSystem").is_err());
    }

    #[test]
    fn service_account_string_must_stay_inside_the_query_buffer() {
        let account = [
            b'L' as u16,
            b'o' as u16,
            b'c' as u16,
            b'a' as u16,
            b'l' as u16,
            b'S' as u16,
            b'y' as u16,
            b's' as u16,
            b't' as u16,
            b'e' as u16,
            b'm' as u16,
            0,
        ];
        let base = account.as_ptr().cast::<u8>();
        let bytes = std::mem::size_of_val(&account);
        assert_eq!(
            bounded_wide_string(account.as_ptr(), base, bytes).unwrap(),
            "LocalSystem"
        );
        // The end pointer is outside the half-open allocation range.
        let end = unsafe { base.add(bytes) }.cast::<u16>();
        assert!(bounded_wide_string(end, base, bytes).is_err());
    }
}
