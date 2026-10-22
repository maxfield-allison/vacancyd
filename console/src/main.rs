#![cfg_attr(windows, windows_subsystem = "windows")]

#[cfg(all(windows, not(target_feature = "crt-static")))]
compile_error!("Windows console requires the static C runtime");

#[cfg(windows)]
mod save;
#[cfg(windows)]
mod tray;

#[cfg(windows)]
fn allowed_navigation(url: &tauri::Url) -> bool {
    matches!(
        (url.scheme(), url.host_str()),
        ("tauri", Some("localhost")) | ("http", Some("tauri.localhost"))
    ) && url.username().is_empty()
        && url.password().is_none()
        && url.port().is_none()
        && matches!(url.path(), "/" | "/index.html")
        && url.query().is_none()
        && url.fragment().is_none()
}

#[cfg(windows)]
#[tauri::command]
async fn read_status(
    client: tauri::State<'_, vacancyd::status_client::StatusClient>,
) -> Result<vacancyd::status_client::Poll, ()> {
    Ok(client.poll().await)
}

/// The window that owns the Windows approval prompt, so the prompt opens in
/// front of the console rather than behind it.
#[cfg(windows)]
fn owner_window(window: &tauri::WebviewWindow) -> isize {
    window.hwnd().map_or(0, |hwnd| hwnd.0 as isize)
}

/// Pause, Resume and hardware switches go over the service's admin pipe,
/// which identifies this account; a change that needs an administrator
/// raises the Windows approval prompt.
#[cfg(windows)]
#[tauri::command]
async fn set_lending(
    window: tauri::WebviewWindow,
    client: tauri::State<'_, vacancyd::status_client::StatusClient>,
    request: vacancyd::status_client::ControlRequest,
) -> Result<vacancyd::status_client::ControlResult, ()> {
    Ok(vacancyd::local_admin::client::set_lending(&client, request, owner_window(&window)).await)
}

#[cfg(windows)]
#[tauri::command]
async fn read_settings(
    client: tauri::State<'_, vacancyd::status_client::StatusClient>,
) -> Result<vacancyd::status_client::SettingsPoll, ()> {
    Ok(client.read_settings().await)
}

/// Saving settings needs an administrator, so it raises the Windows
/// approval prompt unless the console itself runs elevated.
#[cfg(windows)]
#[tauri::command]
async fn save_settings(
    window: tauri::WebviewWindow,
    client: tauri::State<'_, vacancyd::status_client::StatusClient>,
    request: vacancyd::settings::SettingsUpdate,
) -> Result<vacancyd::status_client::SettingsSaveResult, ()> {
    Ok(vacancyd::local_admin::client::save_settings(&client, request, owner_window(&window)).await)
}

/// Opening or ending a debug logging window goes over the admin pipe like
/// the lending switches. It needs an administrator, so it raises the
/// Windows approval prompt unless the console itself runs elevated.
#[cfg(windows)]
#[tauri::command]
async fn set_debug_logging(
    window: tauri::WebviewWindow,
    client: tauri::State<'_, vacancyd::status_client::StatusClient>,
    request: vacancyd::status_client::DebugLoggingRequest,
) -> Result<vacancyd::status_client::ControlResult, ()> {
    Ok(
        vacancyd::local_admin::client::set_debug_logging(&client, request, owner_window(&window))
            .await,
    )
}

/// Read the service's diagnostics export and save it where the owner
/// chooses. Nothing is written unless they pick a file.
#[cfg(windows)]
#[tauri::command]
async fn export_diagnostics(
    window: tauri::WebviewWindow,
    client: tauri::State<'_, vacancyd::status_client::StatusClient>,
) -> Result<vacancyd::status_client::ExportResult, ()> {
    use vacancyd::status_client::ExportResult;
    let document = match client.read_diagnostics().await {
        Ok(document) => document,
        Err(reason) => {
            return Ok(ExportResult::Failed {
                reason: reason.into(),
            })
        }
    };
    let owner = owner_window(&window);
    let saved = tokio::task::spawn_blocking(move || {
        let Some(path) = save::choose_path(owner, "vacancyd-diagnostics.json")? else {
            return Ok(ExportResult::Cancelled);
        };
        std::fs::write(&path, &document).map_err(|_| ())?;
        Ok(ExportResult::Saved {
            path: path.display().to_string(),
        })
    })
    .await;
    Ok(match saved {
        Ok(Ok(result)) => result,
        Ok(Err(())) | Err(_) => ExportResult::Failed {
            reason:
                "The diagnostics could not be saved there. Choose another folder and try again."
                    .into(),
        },
    })
}

#[cfg(windows)]
fn main() {
    use tauri::Manager;
    let background = std::env::args().any(|arg| arg == "--tray");
    let instance = match tray::Instance::acquire(background) {
        Ok(Some(instance)) => instance,
        Ok(None) => return,
        Err(_) => {
            std::process::exit(1);
        }
    };
    let result = tauri::Builder::default()
        .invoke_handler(tauri::generate_handler![
            read_status,
            set_lending,
            read_settings,
            save_settings,
            set_debug_logging,
            export_diagnostics
        ])
        .setup(move |app| {
            app.manage(vacancyd::status_client::StatusClient::new()?);
            let window = tauri::WebviewWindowBuilder::new(
                app,
                "main",
                tauri::WebviewUrl::App("index.html".into()),
            )
            .title("vacancyd owner console")
            .inner_size(1180.0, 820.0)
            .min_inner_size(480.0, 320.0)
            .visible(false)
            .zoom_hotkeys_enabled(true)
            .disable_drag_drop_handler()
            .devtools(false)
            .on_navigation(allowed_navigation)
            .on_new_window(|_, _| tauri::webview::NewWindowResponse::Deny)
            .on_download(|_, _| false)
            .build()?;
            // Logical dimensions grow with Windows scaling. Compare the actual
            // decorated window with its monitor's physical work area, excluding
            // the taskbar, before showing it. Otherwise the bottom of the page
            // can be unreachable even after scrolling on a small display.
            let fits = if let Some(monitor) = window.current_monitor()? {
                let size = window.outer_size()?;
                let available = monitor.work_area().size;
                size.width <= available.width && size.height <= available.height
            } else {
                false
            };
            if !fits && !background {
                window.maximize()?;
            }
            tray::install(app)?;
            instance.listen(app.handle().clone());
            app.manage(instance);
            let hide_window = window.clone();
            window.on_window_event(move |event| {
                // Hidden logon startup must never maximize/show the window.
                // Size it to its current monitor only when the owner opens it.
                if matches!(event, tauri::WindowEvent::Focused(true)) {
                    if let (Ok(Some(monitor)), Ok(size)) =
                        (hide_window.current_monitor(), hide_window.outer_size())
                    {
                        let area = monitor.work_area().size;
                        if size.width > area.width || size.height > area.height {
                            let _ = hide_window.maximize();
                        }
                    }
                }
                if let tauri::WindowEvent::CloseRequested { api, .. } = event {
                    if hide_window.hide().is_ok() {
                        api.prevent_close();
                    }
                }
            });
            if !background {
                window.show()?;
            }
            Ok(())
        })
        .run(tauri::generate_context!());
    if result.is_err() {
        // The GUI subsystem has no terminal for a panic message. Keep the
        // recovery path visible even when the WebView itself cannot start.
        use windows_sys::Win32::UI::WindowsAndMessaging::{MessageBoxW, MB_ICONERROR, MB_OK};
        let message: Vec<u16> = "The owner console could not start. Check that the Microsoft Edge WebView2 Runtime is installed, then try again. See the installed setup guide. The service and session reporter continue independently."
            .encode_utf16().chain(Some(0)).collect();
        let title: Vec<u16> = "vacancyd owner console"
            .encode_utf16()
            .chain(Some(0))
            .collect();
        // SAFETY: both strings are NUL-terminated and live for this modal call.
        unsafe {
            MessageBoxW(
                std::ptr::null_mut(),
                message.as_ptr(),
                title.as_ptr(),
                MB_OK | MB_ICONERROR,
            );
        }
        std::process::exit(1);
    }
}

#[cfg(not(windows))]
fn main() {
    eprintln!("The owner console is supported on Windows only.");
    std::process::exit(1);
}

#[cfg(all(test, windows))]
mod tests {
    use super::allowed_navigation;

    #[test]
    fn navigation_stays_on_the_packaged_document() {
        for target in [
            "tauri://localhost/index.html",
            "http://tauri.localhost/",
            "http://tauri.localhost/index.html",
        ] {
            assert!(allowed_navigation(&target.parse().unwrap()), "{target}");
        }
        for target in [
            "http://tauri.localhost:9811/",
            "tauri://localhost:9811/",
            "http://user@tauri.localhost/",
            "http://user:password@tauri.localhost/",
            "http://tauri.localhost/other.html",
            "http://tauri.localhost/?next=outside",
            "http://tauri.localhost/#outside",
            "https://tauri.localhost/",
            "http://localhost/",
            "https://example.com/",
        ] {
            assert!(!allowed_navigation(&target.parse().unwrap()), "{target}");
        }
    }
}
