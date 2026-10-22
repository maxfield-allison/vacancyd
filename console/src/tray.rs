//! Notification-area lifecycle and fixed local owner actions.
use std::{
    os::windows::io::{AsRawHandle, FromRawHandle, OwnedHandle},
    sync::Arc,
};
use tauri::{
    menu::{Menu, MenuItem, PredefinedMenuItem},
    tray::{MouseButton, MouseButtonState, TrayIconBuilder, TrayIconEvent},
    Manager,
};
use vacancyd::status_client::{ControlRequest, ControlResult, Poll, StatusClient};
use windows_sys::Win32::{
    Foundation::{GetLastError, ERROR_ALREADY_EXISTS, WAIT_OBJECT_0},
    System::Threading::{CreateEventW, CreateMutexW, SetEvent, WaitForSingleObject, INFINITE},
    UI::WindowsAndMessaging::{
        MessageBoxW, IDYES, MB_DEFBUTTON2, MB_ICONINFORMATION, MB_ICONQUESTION, MB_OK, MB_YESNO,
    },
};

const TITLE: &str = "vacancyd owner console";
fn wide(text: &str) -> Vec<u16> {
    text.encode_utf16().chain(Some(0)).collect()
}

pub struct Instance {
    _mutex: OwnedHandle,
    activation: Arc<OwnedHandle>,
}
impl Instance {
    /// Session-local objects keep each interactive user's console independent.
    /// The event carries only a request to show this console, never arguments.
    pub fn acquire(background: bool) -> std::io::Result<Option<Self>> {
        let (instance, duplicate) = Self::named("Local\\vacancyd-owner-console-v1")?;
        if duplicate {
            if !background {
                // The first instance owns its window. Let it use the same Tauri
                // show path as the tray instead of manipulating it externally.
                instance.request_show()?;
            }
            return Ok(None);
        }
        Ok(Some(instance))
    }

    fn named(name: &str) -> std::io::Result<(Self, bool)> {
        let event_name = wide(&format!("{name}-activate"));
        // Create the auto-reset event before competing for the mutex. A second
        // launch during startup can leave a signal for the owner to consume
        // after the window exists, without a title lookup or startup race.
        let activation = unsafe { CreateEventW(std::ptr::null(), 0, 0, event_name.as_ptr()) };
        if activation.is_null() {
            return Err(std::io::Error::last_os_error());
        }
        // SAFETY: ownership of a newly returned kernel handle is transferred.
        let activation = Arc::new(unsafe { OwnedHandle::from_raw_handle(activation) });
        let name = wide(name);
        let mutex = unsafe { CreateMutexW(std::ptr::null(), 0, name.as_ptr()) };
        if mutex.is_null() {
            return Err(std::io::Error::last_os_error());
        }
        let duplicate = unsafe { GetLastError() } == ERROR_ALREADY_EXISTS;
        let mutex = unsafe { OwnedHandle::from_raw_handle(mutex) };
        Ok((
            Self {
                _mutex: mutex,
                activation,
            },
            duplicate,
        ))
    }

    fn request_show(&self) -> std::io::Result<()> {
        if unsafe { SetEvent(self.activation.as_raw_handle()) } == 0 {
            return Err(std::io::Error::last_os_error());
        }
        Ok(())
    }

    pub fn listen(&self, app: tauri::AppHandle) {
        let activation = Arc::clone(&self.activation);
        // A dedicated OS thread blocked on the event costs nothing while
        // idle, unlike the previous 100ms poll on the async runtime.
        // OwnedHandle/Arc keep the thread-safe kernel object alive for the
        // life of the process; the thread exits only when the process does.
        std::thread::spawn(move || loop {
            if unsafe { WaitForSingleObject(activation.as_raw_handle(), INFINITE) } == WAIT_OBJECT_0
            {
                let owner = app.clone();
                let _ = app.run_on_main_thread(move || show(&owner));
            }
        });
    }
}

pub fn show(app: &tauri::AppHandle) {
    if let Some(window) = app.get_webview_window("main") {
        let _ = window.show();
        let _ = window.unminimize();
        let _ = window.set_focus();
    }
}

fn message(text: &str, flags: u32) -> i32 {
    let text = wide(text);
    let title = wide(TITLE);
    // Both buffers remain live and NUL-terminated through the modal call.
    unsafe { MessageBoxW(std::ptr::null_mut(), text.as_ptr(), title.as_ptr(), flags) }
}

pub fn install(app: &tauri::App) -> tauri::Result<()> {
    let state = MenuItem::with_id(
        app,
        "state",
        "Connecting to local service…",
        false,
        None::<&str>,
    )?;
    let open = MenuItem::with_id(app, "open", "Open owner console", true, None::<&str>)?;
    let pause = MenuItem::with_id(app, "pause", "Pause lending…", false, None::<&str>)?;
    let resume = MenuItem::with_id(app, "resume", "Resume lending…", false, None::<&str>)?;
    let quit = MenuItem::with_id(app, "quit", "Quit console", true, None::<&str>)?;
    let separator = PredefinedMenuItem::separator(app)?;
    let menu = Menu::with_items(app, &[&state, &open, &separator, &pause, &resume, &quit])?;
    let mut builder = TrayIconBuilder::with_id("owner")
        .tooltip("vacancyd: connecting")
        .menu(&menu).show_menu_on_left_click(false)
        .on_tray_icon_event(|tray, event| {
            if matches!(event, TrayIconEvent::Click { button: MouseButton::Left, button_state: MouseButtonState::Up, .. }) { show(tray.app_handle()); }
        })
        .on_menu_event(|app, event| match event.id.as_ref() {
            "open" => show(app),
            "quit" => app.exit(0),
            "pause" | "resume" => {
                let enabled = event.id.as_ref() == "resume";
                let consequence = if enabled {
                    "Resume lending?\n\nNew work is allowed only when owner protection, resource limits and readiness permit it. This does not force lending while you use the machine.\n\nIf another account paused lending, Windows asks for administrator approval.\n\nService restart restores configured defaults."
                } else {
                    "Pause lending?\n\nNew borrowed work will be refused. Existing work follows the service's reclaim policy; this does not guarantee immediate cancellation or freed GPU memory.\n\nService restart restores configured defaults."
                };
                if message(consequence, MB_YESNO | MB_ICONQUESTION | MB_DEFBUTTON2) != IDYES { return; }
                let app = app.clone();
                tauri::async_runtime::spawn(async move {
                    let owner = app.get_webview_window("main").and_then(|window| window.hwnd().ok()).map_or(0, |hwnd| hwnd.0 as isize);
                    let result = vacancyd::local_admin::client::set_lending(&app.state::<StatusClient>(), ControlRequest { enabled, resource: None }, owner).await;
                    let text = match result {
                        ControlResult::Applied { enabled: true } => "Lending enabled. Owner protection and resource limits still apply.",
                        ControlResult::Applied { enabled: false } => "Lending paused. New work is refused; existing work follows the service's reclaim policy.",
                        ControlResult::Failed { reason } => reason,
                    };
                    let _ = app.run_on_main_thread(move || { message(text, MB_OK | MB_ICONINFORMATION); });
                });
            },
            _ => (),
        });
    if let Some(icon) = app.default_window_icon() {
        builder = builder.icon(icon.clone());
    }
    let tray = builder.build(app)?;
    // A separate bounded reader keeps tray refreshes from contending with the
    // renderer's single-flight poll. Mutations still share the managed client.
    let reader = StatusClient::new().map_err(|error| tauri::Error::Anyhow(error.into()))?;
    tauri::async_runtime::spawn(async move {
        loop {
            let poll = reader.poll().await;
            let (label, can_pause, can_resume) = match poll {
                // Same priority the window uses: readiness and owner
                // protection outrank the lending switch, which outranks
                // vacancy. Previously this checked only the switch and
                // vacancy, so a not-ready service with no vacancy read "No
                // vacancy: borrowing withheld" while the window correctly
                // said "Service needs attention". Calibration is a normal
                // wait, not a fault, so it never reads as needing attention.
                Poll::Online { status, .. } => (
                    if status.calibrating_only() {
                        "Preparing safe lending"
                    } else if status.untrusted_evidence() {
                        "Waiting for trustworthy readings"
                    } else if !status.ready {
                        "Service needs attention"
                    } else if status.owner_veto.active {
                        // An upstream exempt from the owner veto can still be
                        // lending; do not claim the whole machine is paused.
                        if status.exempt_serving().is_empty() {
                            "Owner policy pauses new work"
                        } else {
                            "Owner policy pauses new work; exempt routes still lending"
                        }
                    } else if !status.lending_enabled {
                        "Lending paused"
                    } else if status.upstreams.iter().any(|u| u.vacancy) {
                        "Default budget fits; see Workloads"
                    } else {
                        "No vacancy: borrowing withheld"
                    },
                    status.lending_enabled,
                    !status.lending_enabled,
                ),
                _ => ("Service status unavailable", false, false),
            };
            let _ = state.set_text(label);
            let _ = pause.set_enabled(can_pause);
            let _ = resume.set_enabled(can_resume);
            let _ = tray.set_tooltip(Some(format!("vacancyd: {label}")));
            tokio::time::sleep(std::time::Duration::from_secs(3)).await;
        }
    });
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::{Instance, WaitForSingleObject, WAIT_OBJECT_0};
    use std::os::windows::io::AsRawHandle;
    use windows_sys::Win32::Foundation::WAIT_TIMEOUT;

    #[test]
    fn console_instance_is_unique_until_last_handle_closes() {
        let name = format!("Local\\vacancyd-console-test-{}", std::process::id());
        let (first, duplicate) = Instance::named(&name).unwrap();
        assert!(!duplicate);
        let (second, duplicate) = Instance::named(&name).unwrap();
        assert!(duplicate);
        assert_eq!(
            unsafe { WaitForSingleObject(first.activation.as_raw_handle(), 0) },
            WAIT_TIMEOUT
        );
        // Activation sent before the owner starts listening is retained, then
        // consumed once. Dropping the duplicate must not discard the signal.
        second.request_show().unwrap();
        drop(second);
        assert_eq!(
            unsafe { WaitForSingleObject(first.activation.as_raw_handle(), 0) },
            WAIT_OBJECT_0
        );
        assert_eq!(
            unsafe { WaitForSingleObject(first.activation.as_raw_handle(), 0) },
            WAIT_TIMEOUT
        );
        drop(first);
        let (_fresh, duplicate) = Instance::named(&name).unwrap();
        assert!(!duplicate);
    }
}
