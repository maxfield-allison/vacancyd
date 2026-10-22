//! Windows clients of the admin pipe: the owner console, which asks for
//! administrator approval only when the service says a change needs it, and
//! the `vacancyd admin-request` helper that approval launches.

use std::path::{Path, PathBuf};
use std::time::Duration;

use tokio::io::{AsyncWriteExt, BufReader};

use super::{
    read_frame, HelperExit, RefusalReason, Request, RequestedAction, Response, MAX_RESPONSE_BYTES,
    PIPE_NAME,
};
use crate::settings::SettingsUpdate;
use crate::status_client::{
    ControlRequest, ControlResult, DebugLoggingRequest, SettingsSaveResult, StatusClient,
    CONSOLE_DEBUG_MINUTES,
};

/// Bounds one pipe round trip, including connecting and verifying the server.
const PIPE_TIMEOUT: Duration = Duration::from_secs(5);
/// How long the console waits for an approved helper to finish. Its own
/// pipe round trip is bounded by [`PIPE_TIMEOUT`].
const HELPER_WAIT: Duration = Duration::from_secs(30);

const BUSY: &str = "Another change is in progress. Wait for its result.";
const UNCONFIRMED: &str =
    "Cannot confirm the change. Check current service status before trying again.";
const DECLINED: &str = "Administrator approval was declined. Nothing was changed.";

/// Why a pipe round trip failed before the service answered.
#[derive(Debug)]
pub enum SendError {
    /// The pipe does not exist or would not open.
    Unreachable,
    /// The pipe's server is not the running vacancyd service.
    NotService,
    /// The connection broke or the answer was unreadable.
    Unconfirmed,
}

impl SendError {
    pub fn message(&self) -> &'static str {
        match self {
            Self::Unreachable => "Cannot reach the local service. Check that vacancyd is running.",
            Self::NotService => {
                "The local admin channel does not belong to the vacancyd service. Nothing was sent."
            }
            Self::Unconfirmed => UNCONFIRMED,
        }
    }
}

/// Send one request on the admin pipe as the calling account.
pub async fn send(request: &Request) -> Result<Response, SendError> {
    let pipe = open().await?;
    crate::windows_pipe::authenticate_server(&pipe, false).map_err(|_| SendError::NotService)?;
    let mut line = serde_json::to_vec(request).map_err(|_| SendError::Unconfirmed)?;
    line.push(b'\n');
    let mut stream = BufReader::new(pipe);
    stream
        .write_all(&line)
        .await
        .map_err(|_| SendError::Unconfirmed)?;
    let frame = read_frame(&mut stream, MAX_RESPONSE_BYTES)
        .await
        .map_err(|_| SendError::Unconfirmed)?
        .ok_or(SendError::Unconfirmed)?;
    serde_json::from_slice(&frame).map_err(|_| SendError::Unconfirmed)
}

/// Open the pipe, waiting briefly while every instance is busy.
async fn open() -> Result<tokio::net::windows::named_pipe::NamedPipeClient, SendError> {
    use windows_sys::Win32::Foundation::ERROR_PIPE_BUSY;
    for _ in 0..20 {
        match crate::windows_pipe::open_pipe(PIPE_NAME) {
            Ok(pipe) => return Ok(pipe),
            Err(error) if error.raw_os_error() == Some(ERROR_PIPE_BUSY as i32) => {
                tokio::time::sleep(Duration::from_millis(50)).await;
            }
            Err(_) => return Err(SendError::Unreachable),
        }
    }
    Err(SendError::Unreachable)
}

/// `vacancyd admin-request`: send a staged request and exit with its
/// [`HelperExit`] code. Launched elevated by the console, or run from a
/// prompt by an operator.
pub async fn run_helper(path: &Path) -> i32 {
    let Ok(request) = super::read_request_file(path) else {
        return HelperExit::BadRequestFile.code();
    };
    match tokio::time::timeout(PIPE_TIMEOUT, send(&request)).await {
        Ok(Ok(response)) => HelperExit::from_response(&request.action, &response).code(),
        Ok(Err(_)) | Err(_) => HelperExit::Unconfirmed.code(),
    }
}

/// What happened to one change the console submitted.
enum Submitted {
    /// The service answered on the console's own connection.
    Answered(Box<Response>),
    /// It needed an administrator, and an approved helper finished.
    Elevated(HelperExit),
    /// A step failed before any answer; the text says what to do next.
    Failed(&'static str),
}

/// Submit as the signed-in account; when the service answers that an
/// administrator is required, ask Windows for approval and resubmit through
/// the elevated helper.
async fn submit(action: RequestedAction, owner_window: isize) -> Submitted {
    let request = Request::new(action);
    match tokio::time::timeout(PIPE_TIMEOUT, send(&request)).await {
        Ok(Ok(Response::Refused {
            reason: RefusalReason::AdministratorRequired,
        })) => {}
        Ok(Ok(response)) => return Submitted::Answered(Box::new(response)),
        Ok(Err(error)) => return Submitted::Failed(error.message()),
        Err(_) => return Submitted::Failed(UNCONFIRMED),
    }
    match tokio::task::spawn_blocking(move || elevate(&request, owner_window)).await {
        Ok(Ok(exit)) => Submitted::Elevated(exit),
        Ok(Err(error)) => Submitted::Failed(error.message()),
        Err(_) => Submitted::Failed(UNCONFIRMED),
    }
}

fn helper_failure(exit: HelperExit) -> &'static str {
    match exit {
        HelperExit::Refused(reason) => reason.message(),
        HelperExit::Applied | HelperExit::Unconfirmed => UNCONFIRMED,
        HelperExit::BadRequestFile => {
            "The approved helper could not read the change. Nothing was changed."
        }
    }
}

/// The owner console's Pause, Resume and hardware switches.
pub async fn set_lending(
    client: &StatusClient,
    request: ControlRequest,
    owner_window: isize,
) -> ControlResult {
    if request.resource.as_ref().is_some_and(|id| {
        id.is_empty()
            || id.len() > 256
            || id == "."
            || id == ".."
            || id.chars().any(char::is_control)
    }) {
        return ControlResult::Failed {
            reason: "Invalid resource identifier.",
        };
    }
    let enabled = request.enabled;
    let action = match request.resource {
        None if enabled => RequestedAction::Resume {},
        None => RequestedAction::Pause {},
        Some(resource) => RequestedAction::SetResource { resource, enabled },
    };
    switch(client, action, enabled, owner_window).await
}

/// The owner console's debug logging switch. Opening or ending a window
/// needs an administrator, so a standard console reaches it through
/// approval.
pub async fn set_debug_logging(
    client: &StatusClient,
    request: DebugLoggingRequest,
    owner_window: isize,
) -> ControlResult {
    let action = if request.enabled {
        RequestedAction::StartDebug {
            minutes: CONSOLE_DEBUG_MINUTES,
        }
    } else {
        RequestedAction::EndDebug {}
    };
    switch(client, action, request.enabled, owner_window).await
}

/// Submit one on/off change and report it as applied only on a matching
/// acknowledgement or the approved helper's success status.
async fn switch(
    client: &StatusClient,
    action: RequestedAction,
    enabled: bool,
    owner_window: isize,
) -> ControlResult {
    client
        .exclusive(ControlResult::Failed { reason: BUSY }, async {
            match submit(action.clone(), owner_window).await {
                Submitted::Answered(response) => match *response {
                    response if response.acknowledges(&action) => {
                        ControlResult::Applied { enabled }
                    }
                    Response::Refused { reason } => ControlResult::Failed {
                        reason: reason.message(),
                    },
                    _ => ControlResult::Failed {
                        reason:
                            "The acknowledgement did not match the change. Check current status.",
                    },
                },
                Submitted::Elevated(HelperExit::Applied) => ControlResult::Applied { enabled },
                Submitted::Elevated(exit) => ControlResult::Failed {
                    reason: helper_failure(exit),
                },
                Submitted::Failed(reason) => ControlResult::Failed { reason },
            }
        })
        .await
}

/// The owner console's Save settings. Saving always needs an administrator,
/// so a standard console reaches it through approval; the stale-revision and
/// validation answers are the same ones the HTTP settings API gives.
pub async fn save_settings(
    client: &StatusClient,
    settings: SettingsUpdate,
    owner_window: isize,
) -> SettingsSaveResult {
    let failed = |reason: &str| SettingsSaveResult::Failed {
        reason: reason.into(),
    };
    client
        .exclusive(failed(BUSY), async {
            match submit(RequestedAction::SaveSettings { settings }, owner_window).await {
                Submitted::Answered(response) => match *response {
                    Response::Saved {
                        settings,
                        restart_required,
                    } if restart_required == settings.application.restart_required => SettingsSaveResult::Saved {
                        settings,
                        restart_required,
                    },
                    Response::Refused { reason } => failed(reason.message()),
                    _ => failed("The local service returned an invalid save acknowledgement."),
                },
                // The helper can return only its status, so read back what
                // was saved, including its new revision.
                Submitted::Elevated(HelperExit::Applied) => match tokio::time::timeout(
                    Duration::from_secs(2),
                    client.read_settings_once(),
                )
                .await
                {
                    Ok(Ok(settings)) => SettingsSaveResult::Saved {
                        restart_required: settings.application.restart_required,
                        settings: Box::new(settings),
                    },
                    _ => failed(
                        "Settings were saved, but reading them back failed. Refresh settings before making another change.",
                    ),
                },
                Submitted::Elevated(exit) => failed(helper_failure(exit)),
                Submitted::Failed(reason) => failed(reason),
            }
        })
        .await
}

/// Why the elevated helper produced no result.
#[derive(Debug)]
pub enum ElevationError {
    /// The owner chose No, or closed the approval prompt.
    Declined,
    /// The helper is missing, or the request could not be staged or launched.
    Launch,
    /// The approved helper did not finish in time. Its outcome is unknown.
    TimedOut,
    /// The helper ended with a status it never returns, such as a crash.
    Unexpected,
}

impl ElevationError {
    pub fn message(&self) -> &'static str {
        match self {
            Self::Declined => DECLINED,
            Self::Launch => "Could not start the administrator approval step. Nothing was changed.",
            Self::TimedOut | Self::Unexpected => UNCONFIRMED,
        }
    }
}

/// Stage `request` where only this account and administrators can read it,
/// run `vacancyd.exe admin-request` through the Windows approval prompt, and
/// return its result. Blocks until the prompt is answered and the helper
/// exits; call it from a blocking thread.
pub fn elevate(request: &Request, owner_window: isize) -> Result<HelperExit, ElevationError> {
    use windows_sys::Win32::Foundation::{CloseHandle, ERROR_CANCELLED, WAIT_OBJECT_0};
    use windows_sys::Win32::System::Com::{
        CoInitializeEx, CoUninitialize, COINIT_APARTMENTTHREADED, COINIT_DISABLE_OLE1DDE,
    };
    use windows_sys::Win32::System::Threading::{GetExitCodeProcess, WaitForSingleObject};
    use windows_sys::Win32::UI::Shell::{
        ShellExecuteExW, SEE_MASK_NOASYNC, SEE_MASK_NOCLOSEPROCESS, SHELLEXECUTEINFOW,
    };
    use windows_sys::Win32::UI::WindowsAndMessaging::SW_HIDE;

    // The helper is installed beside the console, in a directory only
    // administrators can change.
    let helper = std::env::current_exe()
        .map_err(|_| ElevationError::Launch)?
        .with_file_name("vacancyd.exe");
    if !helper.is_file() {
        return Err(ElevationError::Launch);
    }
    let staged = StagedRequest::create(request).map_err(|_| ElevationError::Launch)?;
    let verb = wide("runas");
    let file = wide(&helper.to_string_lossy());
    // Windows paths cannot contain quotation marks.
    let parameters = wide(&format!(
        "admin-request --request \"{}\"",
        staged.path.display()
    ));
    // SAFETY: an all-zero SHELLEXECUTEINFOW is valid: null pointers and
    // handles, and zero flags.
    let mut info: SHELLEXECUTEINFOW = unsafe { std::mem::zeroed() };
    info.cbSize = std::mem::size_of::<SHELLEXECUTEINFOW>() as u32;
    info.fMask = SEE_MASK_NOCLOSEPROCESS | SEE_MASK_NOASYNC;
    info.hwnd = owner_window as _;
    info.lpVerb = verb.as_ptr();
    info.lpFile = file.as_ptr();
    info.lpParameters = parameters.as_ptr();
    info.nShow = SW_HIDE;
    // ShellExecuteEx documents COM initialization on the calling thread.
    // SAFETY: no reserved pointer; balanced by CoUninitialize below when it
    // succeeds, on this same thread.
    let com = unsafe {
        CoInitializeEx(
            std::ptr::null(),
            (COINIT_APARTMENTTHREADED | COINIT_DISABLE_OLE1DDE) as u32,
        )
    };
    // SAFETY: every string is NUL-terminated and outlives the call; `info` is
    // fully initialized above.
    let launched = unsafe { ShellExecuteExW(&mut info) };
    let launch_error = std::io::Error::last_os_error();
    if com >= 0 {
        // SAFETY: balances the successful CoInitializeEx above.
        unsafe { CoUninitialize() };
    }
    if launched == 0 {
        return Err(
            if launch_error.raw_os_error() == Some(ERROR_CANCELLED as i32) {
                ElevationError::Declined
            } else {
                ElevationError::Launch
            },
        );
    }
    let process = info.hProcess;
    if process.is_null() {
        return Err(ElevationError::Launch);
    }
    // SAFETY: `process` is the helper handle SEE_MASK_NOCLOSEPROCESS gave us.
    let waited = unsafe { WaitForSingleObject(process, HELPER_WAIT.as_millis() as u32) };
    let mut code = 0u32;
    // SAFETY: same live handle; `code` is a valid out-parameter.
    let read = waited == WAIT_OBJECT_0 && unsafe { GetExitCodeProcess(process, &mut code) } != 0;
    // SAFETY: this function owns the handle and closes it once.
    unsafe { CloseHandle(process) };
    if waited != WAIT_OBJECT_0 {
        return Err(ElevationError::TimedOut);
    }
    if !read {
        return Err(ElevationError::Unexpected);
    }
    HelperExit::from_code(code as i32).ok_or(ElevationError::Unexpected)
}

fn wide(text: &str) -> Vec<u16> {
    text.encode_utf16().chain(std::iter::once(0)).collect()
}

/// A request file readable by its creator, administrators and LocalSystem
/// only, removed when dropped.
struct StagedRequest {
    path: PathBuf,
}

impl StagedRequest {
    fn create(request: &Request) -> std::io::Result<Self> {
        use std::io::Write;
        let path = std::env::temp_dir().join(format!(
            "vacancyd-admin-request-{}-{}.json",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map_err(std::io::Error::other)?
                .as_nanos()
        ));
        let mut file = create_private(&path)?;
        let staged = Self { path };
        file.write_all(&serde_json::to_vec(request)?)?;
        file.sync_all()?;
        Ok(staged)
    }
}

impl Drop for StagedRequest {
    fn drop(&mut self) {
        let _ = std::fs::remove_file(&self.path);
    }
}

/// Create a new file whose protected DACL admits only LocalSystem,
/// Administrators and the file's owner (this account). The same descriptor
/// as the daemon's `secure_file::create_new`, which lives in the binary.
fn create_private(path: &Path) -> std::io::Result<std::fs::File> {
    use std::os::windows::{ffi::OsStrExt, io::FromRawHandle};
    use windows_sys::Win32::{
        Foundation::{LocalFree, GENERIC_WRITE, INVALID_HANDLE_VALUE},
        Security::{
            Authorization::ConvertStringSecurityDescriptorToSecurityDescriptorW,
            SECURITY_ATTRIBUTES,
        },
        Storage::FileSystem::{CreateFileW, CREATE_NEW, FILE_ATTRIBUTE_NORMAL},
    };
    let sddl = wide("D:P(A;;FA;;;SY)(A;;FA;;;BA)(A;;FA;;;OW)");
    let mut descriptor = std::ptr::null_mut();
    // SAFETY: NUL-terminated SDDL and a valid out pointer; the descriptor is
    // released below after CreateFileW has copied it.
    if unsafe {
        ConvertStringSecurityDescriptorToSecurityDescriptorW(
            sddl.as_ptr(),
            1,
            &mut descriptor,
            std::ptr::null_mut(),
        )
    } == 0
    {
        return Err(std::io::Error::last_os_error());
    }
    let name: Vec<u16> = path.as_os_str().encode_wide().chain(Some(0)).collect();
    let attributes = SECURITY_ATTRIBUTES {
        nLength: std::mem::size_of::<SECURITY_ATTRIBUTES>() as u32,
        lpSecurityDescriptor: descriptor,
        bInheritHandle: 0,
    };
    // CREATE_NEW refuses an existing file or link, whose ACL CreateFileW
    // would not replace.
    // SAFETY: all pointers remain valid for the call; no handle is inherited.
    let handle = unsafe {
        CreateFileW(
            name.as_ptr(),
            GENERIC_WRITE,
            0,
            &attributes,
            CREATE_NEW,
            FILE_ATTRIBUTE_NORMAL,
            std::ptr::null_mut(),
        )
    };
    let error = std::io::Error::last_os_error();
    // SAFETY: the descriptor allocation belongs to LocalFree.
    unsafe { LocalFree(descriptor) };
    if handle == INVALID_HANDLE_VALUE {
        return Err(error);
    }
    // SAFETY: a uniquely owned, successfully created file handle.
    Ok(unsafe { std::fs::File::from_raw_handle(handle) })
}
