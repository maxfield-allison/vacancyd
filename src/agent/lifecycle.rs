//! A hidden top-level window lets Restart Manager shut the reporter down.
//! A message-only window would not receive the session-ending broadcasts.
//! No install, task, service, or machine-shutdown operations belong here.

use std::ptr::null_mut;
use std::sync::atomic::{AtomicUsize, Ordering};

use anyhow::{bail, Context, Result};
use tokio::sync::oneshot;
use windows_sys::Win32::{
    Foundation::{CloseHandle, HANDLE, HWND, LPARAM, LRESULT, WAIT_OBJECT_0, WPARAM},
    System::{
        LibraryLoader::GetModuleHandleW,
        Recovery::{
            RegisterApplicationRestart, RESTART_NO_CRASH, RESTART_NO_HANG, RESTART_NO_REBOOT,
        },
        Threading::{CreateEventW, SetEvent, INFINITE},
    },
    UI::WindowsAndMessaging::{
        CreateWindowExW, DefWindowProcW, DestroyWindow, DispatchMessageW,
        MsgWaitForMultipleObjects, PeekMessageW, PostQuitMessage, RegisterClassW, TranslateMessage,
        UnregisterClassW, MSG, PM_REMOVE, QS_ALLINPUT, WM_CLOSE, WM_ENDSESSION, WM_QUERYENDSESSION,
        WM_QUIT, WNDCLASSW,
    },
};

pub fn register_restart() -> Result<()> {
    // Only patch/upgrade restart. Logon is owned by the MSI Startup shortcut;
    // this isn't crash supervision. The entry point needs no persisted state.
    let command: Vec<u16> = "--restart-manager\0".encode_utf16().collect();
    // SAFETY: command is a live, NUL-terminated UTF-16 string, copied by Windows.
    let result = unsafe {
        RegisterApplicationRestart(
            command.as_ptr(),
            RESTART_NO_CRASH | RESTART_NO_HANG | RESTART_NO_REBOOT,
        )
    };
    if result < 0 {
        bail!("registering session-agent restart failed: HRESULT 0x{result:08x}");
    }
    Ok(())
}

/// Run the window loop on this thread; the reporter owns its async runtime on
/// a worker. Dropping its future closes the pipe and releases the session lock.
pub fn run(
    worker: impl FnOnce(oneshot::Receiver<()>) -> Result<()> + Send + 'static,
) -> Result<()> {
    run_window(Window::create()?, worker)
}

fn run_window(
    window: Window,
    worker: impl FnOnce(oneshot::Receiver<()>) -> Result<()> + Send + 'static,
) -> Result<()> {
    let finished = Event::create()?;
    let finished_handle = finished.0 as usize;
    let (stop, shutdown) = oneshot::channel();
    let thread = std::thread::Builder::new()
        .name("session-reporter".into())
        .spawn(move || {
            // Wake the loop on both ordinary completion and panic. The event
            // remains owned by the parent until this worker has been joined.
            let _wake = SignalOnDrop(finished_handle);
            worker(shutdown)
        })
        .context("starting session reporter")?;
    let loop_result = window.pump(finished.0);
    let _ = stop.send(());
    let worker_result = thread
        .join()
        .map_err(|_| anyhow::anyhow!("session reporter panicked"))?;
    loop_result?;
    worker_result
}

struct Event(HANDLE);

impl Event {
    fn create() -> Result<Self> {
        // SAFETY: unnamed manual-reset event, no borrowed security descriptor.
        let handle = unsafe { CreateEventW(null_mut(), 1, 0, std::ptr::null()) };
        if handle.is_null() {
            return Err(std::io::Error::last_os_error())
                .context("creating reporter completion event");
        }
        Ok(Self(handle))
    }
}

impl Drop for Event {
    fn drop(&mut self) {
        // SAFETY: this owns the handle; the worker has already exited.
        unsafe { CloseHandle(self.0) };
    }
}

struct SignalOnDrop(usize);

impl Drop for SignalOnDrop {
    fn drop(&mut self) {
        // SAFETY: parent keeps this valid event alive until after joining us.
        unsafe { SetEvent(self.0 as HANDLE) };
    }
}

struct Window {
    handle: HWND,
    class: Vec<u16>,
}

impl Window {
    fn create() -> Result<Self> {
        static NEXT: AtomicUsize = AtomicUsize::new(0);
        let class: Vec<u16> = format!(
            "vacancyd-session-{}-{}\0",
            std::process::id(),
            NEXT.fetch_add(1, Ordering::Relaxed)
        )
        .encode_utf16()
        .collect();
        // SAFETY: a null module name retrieves this executable's live module.
        let instance = unsafe { GetModuleHandleW(std::ptr::null()) };
        if instance.is_null() {
            return Err(std::io::Error::last_os_error()).context("finding session-agent module");
        }
        let definition = WNDCLASSW {
            lpfnWndProc: Some(window_proc),
            hInstance: instance,
            lpszClassName: class.as_ptr(),
            // SAFETY: remaining fields are optional handles/pointers or flags.
            ..unsafe { std::mem::zeroed() }
        };
        // SAFETY: definition and its terminated class name live through the call.
        if unsafe { RegisterClassW(&definition) } == 0 {
            return Err(std::io::Error::last_os_error())
                .context("registering session-agent window");
        }
        let mut window = Self {
            handle: null_mut(),
            class,
        };
        // SAFETY: registered class, no parent or window data; deliberately no
        // WS_VISIBLE and no HWND_MESSAGE. Never appears in the taskbar.
        window.handle = unsafe {
            CreateWindowExW(
                0,
                window.class.as_ptr(),
                window.class.as_ptr(),
                0,
                0,
                0,
                0,
                0,
                null_mut(),
                null_mut(),
                instance,
                std::ptr::null(),
            )
        };
        if window.handle.is_null() {
            return Err(std::io::Error::last_os_error()).context("creating session-agent window");
        }
        Ok(window)
    }

    fn pump(&self, finished: HANDLE) -> Result<()> {
        loop {
            // SAFETY: finished is kept alive by run(); the window and its queue
            // belong to this thread. Wait uses no polling or periodic timer.
            let result =
                unsafe { MsgWaitForMultipleObjects(1, &finished, 0, INFINITE, QS_ALLINPUT) };
            if result == WAIT_OBJECT_0 {
                return Ok(());
            }
            if result != WAIT_OBJECT_0 + 1 {
                return Err(std::io::Error::last_os_error())
                    .context("waiting for session-agent messages");
            }
            // SAFETY: MSG consists only of integer fields and nullable handles.
            let mut message: MSG = unsafe { std::mem::zeroed() };
            // SAFETY: live output structure; drain only this thread's queue.
            while unsafe { PeekMessageW(&mut message, null_mut(), 0, 0, PM_REMOVE) } != 0 {
                if message.message == WM_QUIT {
                    return Ok(());
                }
                // SAFETY: messages were obtained from this thread's queue.
                unsafe {
                    TranslateMessage(&message);
                    DispatchMessageW(&message);
                }
            }
        }
    }
}

impl Drop for Window {
    fn drop(&mut self) {
        // SAFETY: this thread owns the window and unique class. Window proc
        // never destroys it, avoiding stale HWND reuse/double destruction.
        unsafe {
            if !self.handle.is_null() {
                DestroyWindow(self.handle);
            }
            UnregisterClassW(self.class.as_ptr(), GetModuleHandleW(std::ptr::null()));
        }
    }
}

unsafe extern "system" fn window_proc(
    hwnd: HWND,
    message: u32,
    wparam: WPARAM,
    lparam: LPARAM,
) -> LRESULT {
    match message {
        // A query is not consent to *exit yet*: another application can veto.
        WM_QUERYENDSESSION => 1,
        WM_ENDSESSION => {
            if wparam != 0 {
                // SAFETY: posts to the calling window thread's own queue.
                unsafe { PostQuitMessage(0) };
            }
            0
        }
        WM_CLOSE => {
            // SAFETY: as above; destruction happens after the worker exits.
            unsafe { PostQuitMessage(0) };
            0
        }
        // SAFETY: unchanged arguments from Windows' window-procedure dispatch.
        _ => unsafe { DefWindowProcW(hwnd, message, wparam, lparam) },
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use windows_sys::Win32::UI::WindowsAndMessaging::{
        IsWindowVisible, PostMessageW, SendMessageW, WM_APP,
    };

    fn take_quit() -> bool {
        // SAFETY: MSG consists only of integer fields and nullable handles.
        let mut message: MSG = unsafe { std::mem::zeroed() };
        // WM_QUIT is generated only after pending messages have been drained.
        // Use the same unfiltered queue handling as the reporter's window loop.
        // SAFETY: only this test thread's queue and its own hidden windows.
        while unsafe { PeekMessageW(&mut message, null_mut(), 0, 0, PM_REMOVE) } != 0 {
            if message.message == WM_QUIT {
                return true;
            }
            // SAFETY: messages were obtained from this thread's queue.
            unsafe {
                TranslateMessage(&message);
                DispatchMessageW(&message);
            }
        }
        false
    }

    #[test]
    fn quit_is_observed_after_pending_window_messages() {
        let window = Window::create().unwrap();
        // SAFETY: both messages target only our own hidden test window.
        unsafe {
            assert_ne!(PostMessageW(window.handle, WM_APP, 0, 0), 0);
            SendMessageW(window.handle, WM_CLOSE, 0, 0);
        }
        assert!(take_quit());
    }

    #[test]
    fn hidden_window_waits_for_confirmed_shutdown() {
        let window = Window::create().unwrap();
        // SAFETY: every message targets our own hidden test window, not the OS
        // or a running agent. This never requests an actual machine shutdown.
        unsafe {
            assert_eq!(IsWindowVisible(window.handle), 0);
            assert_eq!(SendMessageW(window.handle, WM_QUERYENDSESSION, 0, 1), 1);
            assert!(!take_quit());
            SendMessageW(window.handle, WM_ENDSESSION, 0, 1);
            assert!(!take_quit(), "canceled shutdown must keep reporting");
            SendMessageW(window.handle, WM_ENDSESSION, 1, 1);
            assert!(take_quit());
            SendMessageW(window.handle, WM_CLOSE, 0, 0);
            assert!(take_quit());
        }
    }

    #[test]
    fn completed_worker_wakes_window_loop_and_propagates_errors() {
        run(|_| Ok(())).unwrap();
        assert!(run(|_| bail!("test reporter failure"))
            .unwrap_err()
            .to_string()
            .contains("test reporter failure"));
        assert!(run(|_| panic!("test reporter panic"))
            .unwrap_err()
            .to_string()
            .contains("panicked"));
    }

    #[test]
    fn window_close_cancels_and_joins_reporter() {
        let window = Window::create().unwrap();
        // SAFETY: queued only to our own hidden window, before entering its loop.
        assert_ne!(unsafe { PostMessageW(window.handle, WM_CLOSE, 0, 0) }, 0);
        let stopped = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
        let observed = stopped.clone();
        run_window(window, move |shutdown| {
            shutdown
                .blocking_recv()
                .context("shutdown sender disappeared")?;
            observed.store(true, Ordering::SeqCst);
            Ok(())
        })
        .unwrap();
        assert!(stopped.load(Ordering::SeqCst));
    }
}
