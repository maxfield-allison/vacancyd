//! The native Save As dialog, for writing a diagnostics export where the
//! owner chooses.

use std::ffi::OsString;
use std::os::windows::ffi::{OsStrExt, OsStringExt};
use std::path::PathBuf;

/// Longest path the dialog may return, in UTF-16 units including the NUL.
const MAX_PATH_UNITS: usize = 32_768;

/// Ask the owner where to save a JSON file, offering `suggested_name`.
/// `Ok(None)` means they cancelled. Blocks until the dialog closes; call it
/// from a blocking thread.
pub fn choose_path(owner_window: isize, suggested_name: &str) -> Result<Option<PathBuf>, ()> {
    use windows_sys::Win32::System::Com::{
        CoInitializeEx, CoUninitialize, COINIT_APARTMENTTHREADED, COINIT_DISABLE_OLE1DDE,
    };
    use windows_sys::Win32::UI::Controls::Dialogs::{
        CommDlgExtendedError, GetSaveFileNameW, OFN_DONTADDTORECENT, OFN_EXPLORER, OFN_NOCHANGEDIR,
        OFN_OVERWRITEPROMPT, OFN_PATHMUSTEXIST, OPENFILENAMEW,
    };

    let mut file: Vec<u16> = suggested_name.encode_utf16().collect();
    file.resize(MAX_PATH_UNITS, 0);
    let filter: Vec<u16> = "JSON files (*.json)\0*.json\0All files (*.*)\0*.*\0\0"
        .encode_utf16()
        .collect();
    let extension = wide("json");
    let title = wide("Save vacancyd diagnostics");
    let directory = initial_directory().ok_or(())?;
    // SAFETY: an all-zero OPENFILENAMEW is valid: null pointers and handles,
    // zero sizes and flags.
    let mut dialog: OPENFILENAMEW = unsafe { std::mem::zeroed() };
    dialog.lStructSize = std::mem::size_of::<OPENFILENAMEW>() as u32;
    dialog.hwndOwner = owner_window as _;
    dialog.lpstrFilter = filter.as_ptr();
    dialog.nFilterIndex = 1;
    dialog.lpstrFile = file.as_mut_ptr();
    dialog.nMaxFile = file.len() as u32;
    dialog.lpstrTitle = title.as_ptr();
    dialog.lpstrInitialDir = directory.as_ptr();
    dialog.lpstrDefExt = extension.as_ptr();
    dialog.Flags = OFN_EXPLORER
        | OFN_OVERWRITEPROMPT
        | OFN_PATHMUSTEXIST
        | OFN_NOCHANGEDIR
        | OFN_DONTADDTORECENT;
    // The Explorer-style dialog hosts shell components, which expect COM on
    // the calling thread.
    // SAFETY: no reserved pointer; balanced by CoUninitialize below when it
    // succeeds, on this same thread.
    let com = unsafe {
        CoInitializeEx(
            std::ptr::null(),
            (COINIT_APARTMENTTHREADED | COINIT_DISABLE_OLE1DDE) as u32,
        )
    };
    // SAFETY: every string is NUL-terminated and outlives the call, and
    // `file` holds `nMaxFile` units for the answer.
    let chosen = unsafe { GetSaveFileNameW(&mut dialog) };
    // SAFETY: reads this thread's last dialog error; no arguments.
    let error = if chosen == 0 {
        unsafe { CommDlgExtendedError() }
    } else {
        0
    };
    if com >= 0 {
        // SAFETY: balances the successful CoInitializeEx above.
        unsafe { CoUninitialize() };
    }
    if chosen == 0 {
        // Zero extended error means the owner cancelled.
        return if error == 0 { Ok(None) } else { Err(()) };
    }
    let length = file
        .iter()
        .position(|unit| *unit == 0)
        .unwrap_or(file.len());
    Ok(Some(PathBuf::from(OsString::from_wide(&file[..length]))))
}

fn wide(text: &str) -> Vec<u16> {
    text.encode_utf16().chain(std::iter::once(0)).collect()
}

fn initial_directory() -> Option<Vec<u16>> {
    use windows_sys::Win32::UI::Shell::{FOLDERID_Documents, FOLDERID_Profile};

    // Known folders follow Windows redirection, including a moved Documents
    // folder. Do not inherit the installed console's Program Files directory.
    [FOLDERID_Documents, FOLDERID_Profile]
        .iter()
        .filter_map(known_folder)
        .find(|path| path.is_dir())
        .map(|path| path.as_os_str().encode_wide().chain(Some(0)).collect())
}

fn known_folder(id: &windows_sys::core::GUID) -> Option<PathBuf> {
    use windows_sys::Win32::{System::Com::CoTaskMemFree, UI::Shell::SHGetKnownFolderPath};

    let mut raw = std::ptr::null_mut();
    // SAFETY: id is a valid known-folder GUID and raw is an out-parameter.
    // A null token selects the current interactive user's folder.
    let status = unsafe { SHGetKnownFolderPath(id, 0, std::ptr::null_mut(), &mut raw) };
    let path = if status >= 0 && !raw.is_null() {
        // SAFETY: a successful call returns a NUL-terminated UTF-16 allocation
        // that remains live until CoTaskMemFree below.
        unsafe {
            let mut length = 0;
            while *raw.add(length) != 0 {
                length += 1;
            }
            Some(PathBuf::from(OsString::from_wide(
                std::slice::from_raw_parts(raw, length),
            )))
        }
    } else {
        None
    };
    // SAFETY: the API requires freeing its output even on failure; null is valid.
    unsafe { CoTaskMemFree(raw.cast()) };
    path
}
