//! Installed configuration access: Administrators and LocalSystem only.
//!
//! Use handles that deny replacement while inspected. Refuse reparse points
//! and multiply-linked files instead of changing the ACL of another object.
use std::{
    fs::File,
    io,
    os::windows::{
        ffi::OsStrExt,
        io::{AsRawHandle, FromRawHandle},
    },
    path::{Path, PathBuf},
};
use windows_sys::Win32::{
    Foundation::{LocalFree, INVALID_HANDLE_VALUE},
    Security::{
        Authorization::{
            ConvertSecurityDescriptorToStringSecurityDescriptorW,
            ConvertStringSecurityDescriptorToSecurityDescriptorW, GetSecurityInfo, SetSecurityInfo,
            SE_FILE_OBJECT,
        },
        GetSecurityDescriptorDacl, GetSecurityDescriptorOwner, DACL_SECURITY_INFORMATION,
        OWNER_SECURITY_INFORMATION, PROTECTED_DACL_SECURITY_INFORMATION, SECURITY_ATTRIBUTES,
    },
    Storage::FileSystem::{
        CreateDirectoryW, CreateFileW, GetFileInformationByHandle, BY_HANDLE_FILE_INFORMATION,
        FILE_ATTRIBUTE_REPARSE_POINT, FILE_FLAG_BACKUP_SEMANTICS, FILE_FLAG_OPEN_REPARSE_POINT,
        FILE_READ_ATTRIBUTES, FILE_SHARE_READ, OPEN_EXISTING, READ_CONTROL, WRITE_DAC, WRITE_OWNER,
    },
};

const FILE_SDDL: &str = "O:BAD:P(A;;FA;;;SY)(A;;FA;;;BA)";
const DIRECTORY_SDDL: &str = "O:BAD:P(A;OICI;FA;;;SY)(A;OICI;FA;;;BA)";

struct Descriptor(*mut core::ffi::c_void);
impl Drop for Descriptor {
    fn drop(&mut self) {
        unsafe {
            LocalFree(self.0);
        }
    }
}

pub(crate) fn open(path: &Path, write_acl: bool) -> io::Result<File> {
    let mut name: Vec<u16> = path.as_os_str().encode_wide().collect();
    if name.contains(&0) {
        return Err(io::Error::new(io::ErrorKind::InvalidInput, "NUL in path"));
    }
    name.push(0);
    let access = READ_CONTROL
        | FILE_READ_ATTRIBUTES
        | if write_acl {
            WRITE_DAC | WRITE_OWNER
        } else {
            0
        };
    // SAFETY: NUL-terminated path, no security attributes or template handle.
    // OPEN_REPARSE_POINT inspects the link itself; it never follows the final link.
    let handle = unsafe {
        CreateFileW(
            name.as_ptr(),
            access,
            FILE_SHARE_READ,
            std::ptr::null(),
            OPEN_EXISTING,
            FILE_FLAG_BACKUP_SEMANTICS | FILE_FLAG_OPEN_REPARSE_POINT,
            std::ptr::null_mut(),
        )
    };
    if handle == INVALID_HANDLE_VALUE {
        return Err(io::Error::last_os_error());
    }
    // SAFETY: successful handle is owned solely by this File.
    let file = unsafe { File::from_raw_handle(handle) };
    let mut info: BY_HANDLE_FILE_INFORMATION = unsafe { std::mem::zeroed() };
    if unsafe { GetFileInformationByHandle(handle, &mut info) } == 0 {
        return Err(io::Error::last_os_error());
    }
    if info.dwFileAttributes & FILE_ATTRIBUTE_REPARSE_POINT != 0 || info.nNumberOfLinks > 1 {
        return Err(io::Error::new(
            io::ErrorKind::PermissionDenied,
            "path must not be a reparse point or multiply-linked file",
        ));
    }
    Ok(file)
}

/// Refuse a path that already exists as a reparse point or a file with more
/// than one hard link, without changing anything about it. A path with
/// nothing at it yet is fine for the caller to create. Reusable wherever a
/// SYSTEM-run process is about to open a path an unprivileged user could
/// have planted first, such as the config this module already guards, or the
/// bounded service log.
pub(crate) fn refuse_planted(path: &Path) -> io::Result<()> {
    match open(path, false) {
        Ok(_) => Ok(()),
        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(()),
        Err(error) => Err(error),
    }
}

fn descriptor_string(file: &File) -> io::Result<String> {
    let mut raw = std::ptr::null_mut();
    let flags = OWNER_SECURITY_INFORMATION | DACL_SECURITY_INFORMATION;
    // SAFETY: valid open handle and output pointer; unrequested outputs are null.
    let status = unsafe {
        GetSecurityInfo(
            file.as_raw_handle(),
            SE_FILE_OBJECT,
            flags,
            std::ptr::null_mut(),
            std::ptr::null_mut(),
            std::ptr::null_mut(),
            std::ptr::null_mut(),
            &mut raw,
        )
    };
    if status != 0 {
        return Err(io::Error::from_raw_os_error(status as i32));
    }
    let descriptor = Descriptor(raw);
    let mut text = std::ptr::null_mut();
    let mut length = 0;
    if unsafe {
        ConvertSecurityDescriptorToStringSecurityDescriptorW(
            descriptor.0,
            1,
            flags,
            &mut text,
            &mut length,
        )
    } == 0
    {
        return Err(io::Error::last_os_error());
    }
    let allocation = Descriptor(text.cast());
    // SAFETY: conversion returned `length` UTF-16 units owned until drop below.
    let units = unsafe { std::slice::from_raw_parts(text, length as usize) };
    let end = units.iter().position(|v| *v == 0).unwrap_or(units.len());
    let result = String::from_utf16_lossy(&units[..end]);
    drop(allocation);
    Ok(result)
}

fn protect(file: &File, directory: bool) -> io::Result<()> {
    let sddl = if directory { DIRECTORY_SDDL } else { FILE_SDDL };
    let text: Vec<u16> = sddl.encode_utf16().chain(Some(0)).collect();
    let mut raw = std::ptr::null_mut();
    if unsafe {
        ConvertStringSecurityDescriptorToSecurityDescriptorW(
            text.as_ptr(),
            1,
            &mut raw,
            std::ptr::null_mut(),
        )
    } == 0
    {
        return Err(io::Error::last_os_error());
    }
    let descriptor = Descriptor(raw);
    let mut owner = std::ptr::null_mut();
    let mut dacl = std::ptr::null_mut();
    let mut defaulted = 0;
    let mut present = 0;
    if unsafe { GetSecurityDescriptorOwner(descriptor.0, &mut owner, &mut defaulted) } == 0
        || unsafe {
            GetSecurityDescriptorDacl(descriptor.0, &mut present, &mut dacl, &mut defaulted)
        } == 0
    {
        return Err(io::Error::last_os_error());
    }
    // SAFETY: pointers belong to the live converted descriptor. Setting the
    // owner as well as the DACL prevents a prior ordinary owner restoring access.
    let status = unsafe {
        SetSecurityInfo(
            file.as_raw_handle(),
            SE_FILE_OBJECT,
            OWNER_SECURITY_INFORMATION
                | DACL_SECURITY_INFORMATION
                | PROTECTED_DACL_SECURITY_INFORMATION,
            owner,
            std::ptr::null_mut(),
            dacl,
            std::ptr::null_mut(),
        )
    };
    if status != 0 {
        return Err(io::Error::from_raw_os_error(status as i32));
    }
    verify(file)
}

/// Judge the effective grants, not the exact text: an equally private
/// descriptor, such as the inherited one an editor's replace-save produces,
/// is accepted. See `acl_policy`.
fn verify(file: &File) -> io::Result<()> {
    crate::acl_policy::private_to_administrators(&descriptor_string(file)?).map_err(|reason| {
        io::Error::new(
            io::ErrorKind::PermissionDenied,
            format!("permissions must grant only Administrators and LocalSystem ({reason})"),
        )
    })
}

/// Tighten directory first, then the existing file; never rewrite config bytes.
/// On failure the caller must leave the service disabled and the source intact.
///
/// `require_admin_owner` is the installer's mode. Windows Installer keeps a
/// configuration that already exists (`NeverOverwrite`), and ProgramData lets
/// ordinary users create folders, so a user could plant one before the first
/// install. Refuse to adopt a file whose owner is not Administrators or
/// LocalSystem instead of taking ownership of it. The manual repair command
/// keeps taking ownership, after an administrator has reviewed the file.
pub fn protect_config(path: &Path, require_admin_owner: bool) -> io::Result<()> {
    protect_config_under(&protected_root()?, path, require_admin_owner)
}

/// [`protect_config`] with the protected root given, so a test can repair a
/// root of its own without loosening the one other tests share.
fn protect_config_under(root: &Path, path: &Path, require_admin_owner: bool) -> io::Result<()> {
    if !path
        .parent()
        .and_then(Path::file_name)
        .and_then(|n| n.to_str())
        .is_some_and(|n| n.eq_ignore_ascii_case("vacancyd"))
    {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "protect-config requires a dedicated directory named vacancyd",
        ));
    }
    let parent = path.parent().ok_or_else(|| {
        io::Error::new(
            io::ErrorKind::InvalidInput,
            "configuration needs a parent directory",
        )
    })?;
    // Checked before any permission changes. The directory itself may still
    // grant others access, since restoring its protection is the repair.
    if let Some(reason) = path_refusal_under(root, parent, Repairing::Yes)? {
        return Err(io::Error::new(
            io::ErrorKind::PermissionDenied,
            format!(
                "refusing to change permissions on {}: {reason}",
                parent.display()
            ),
        ));
    }
    if require_admin_owner {
        let file = open(path, false)?;
        crate::acl_policy::owned_by_administrators(&descriptor_string(&file)?).map_err(
            |reason| {
                io::Error::new(
                    io::ErrorKind::PermissionDenied,
                    format!("refusing to adopt the existing configuration ({reason}); review it, then remove it or run protect-config from an elevated prompt, and install again"),
                )
            },
        )?;
    }
    let directory = open(parent, true)?;
    protect(&directory, true)?;
    let file = open(path, true)?;
    protect(&file, false)
}

pub fn verify_config(path: &Path) -> io::Result<()> {
    let directory = open(
        path.parent().ok_or_else(|| {
            io::Error::new(
                io::ErrorKind::InvalidInput,
                "configuration needs a parent directory",
            )
        })?,
        false,
    )?;
    let file = open(path, false)?;
    verify(&directory)
        .and_then(|()| verify(&file))
        .map_err(|error| {
            io::Error::new(
                error.kind(),
                format!("configuration {error}; run protect-config from an elevated prompt"),
            )
        })
}

/// The owner and DACL of `path` as SDDL, read without following a final
/// reparse point.
#[cfg(test)]
pub fn descriptor_of(path: &Path) -> io::Result<String> {
    descriptor_string(&open(path, false)?)
}

/// Check an existing directory without changing its owner or ACL.
pub fn verify_directory(path: &Path) -> io::Result<()> {
    verify(&open(path, false)?)
}

/// Why vacancyd, running as an administrator or LocalSystem, must not create
/// or change a directory at `path`, or `None`.
///
/// `path` must be [`protected_root`] or inside it as written, before any
/// link is resolved (see [`crate::protected_root::contains`]), and the root
/// must already exist. Each existing directory from the drive root down must
/// be a real directory, not a reparse point that could lead elsewhere.
/// Those above the root must pass
/// [`crate::acl_policy::others_cannot_replace`]. The root and every
/// existing directory below it must be private to Administrators and
/// LocalSystem ([`crate::acl_policy::private_to_administrators`]): no other
/// account holds any right there, so none can add, rename or remove a name
/// on the path, or turn an empty directory into a junction after this check.
pub fn replaceable_path(path: &Path) -> io::Result<Option<String>> {
    path_refusal(path, Repairing::No)
}

/// Whether the directory a check ends at is about to be repaired.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Repairing {
    /// Like every directory from the root down, it must already be private.
    No,
    /// `protect-config` is about to restore its protection, so it may still
    /// grant others access. It must be the root or a real directory inside
    /// it, reached without reparse points. Earlier directories from the
    /// protected root down must already be private; those above it must
    /// pass [`crate::acl_policy::others_cannot_replace`].
    Yes,
}

fn path_refusal(path: &Path, repairing: Repairing) -> io::Result<Option<String>> {
    path_refusal_under(&protected_root()?, path, repairing)
}

fn path_refusal_under(
    root: &Path,
    path: &Path,
    repairing: Repairing,
) -> io::Result<Option<String>> {
    let absolute = std::path::absolute(path)?;
    let inside = match (root.to_str(), absolute.to_str()) {
        (Some(root), Some(path)) => crate::protected_root::contains(root, path),
        _ => false,
    };
    if !inside {
        return Ok(Some(format!(
            "{} is not inside {}, the protected folder required for this destination",
            absolute.display(),
            root.display()
        )));
    }
    // Inside the root as written, so the chain passes through the root at
    // this depth.
    let root_depth = root.ancestors().count();
    let mut chain: Vec<&Path> = absolute.ancestors().collect();
    chain.reverse();
    let mut existing = Vec::new();
    for directory in &chain {
        match open(directory, false) {
            Ok(file) => existing.push((*directory, file)),
            Err(error) if error.kind() == io::ErrorKind::NotFound => break,
            // Read, and refused. An error from Windows itself keeps its code.
            Err(error) if error.raw_os_error().is_none() => {
                return Ok(Some(format!("{}: {error}", directory.display())))
            }
            Err(error) => return Err(error),
        }
    }
    if existing.len() < root_depth {
        return Ok(Some(format!(
            "{} does not exist; the vacancyd installer creates it, protected for Administrators and LocalSystem",
            root.display()
        )));
    }
    let repaired =
        (repairing == Repairing::Yes && existing.len() == chain.len()).then(|| existing.len() - 1);
    for (index, (directory, file)) in existing.iter().enumerate() {
        if Some(index) == repaired {
            continue;
        }
        let descriptor = descriptor_string(file)?;
        let judged = if index + 1 < root_depth {
            crate::acl_policy::others_cannot_replace(&descriptor, false)
        } else {
            crate::acl_policy::private_to_administrators(&descriptor)
        };
        if let Err(reason) = judged {
            return Ok(Some(format!("{}: {reason}", directory.display())));
        }
    }
    Ok(None)
}

/// The root accepted by [`replaceable_path`]: `vacancyd` in the machine's
/// ProgramData folder, which the MSI creates protected for Administrators
/// and LocalSystem.
#[cfg(not(test))]
pub fn protected_root() -> io::Result<PathBuf> {
    Ok(program_data()?.join("vacancyd"))
}

/// Tests use a protected folder of their own, so they never create or change
/// an installed `ProgramData\vacancyd`.
#[cfg(test)]
pub fn protected_root() -> io::Result<PathBuf> {
    Ok(crate::secure_file::test_root())
}

/// The machine's ProgramData folder, as Windows records it. Not the
/// `ProgramData` environment variable: the environment of an elevated
/// program comes partly from the same user's settings, which that user's
/// unelevated programs can change.
fn program_data() -> io::Result<PathBuf> {
    use std::os::windows::ffi::OsStringExt;
    use windows_sys::Win32::{
        System::Com::CoTaskMemFree,
        UI::Shell::{FOLDERID_ProgramData, SHGetKnownFolderPath, KF_FLAG_DEFAULT},
    };
    let mut raw: windows_sys::core::PWSTR = std::ptr::null_mut();
    // SAFETY: a valid folder ID and out pointer; a null token asks for the
    // calling process's own view.
    let status = unsafe {
        SHGetKnownFolderPath(
            &FOLDERID_ProgramData,
            KF_FLAG_DEFAULT as u32,
            std::ptr::null_mut(),
            &mut raw,
        )
    };
    let path = if status >= 0 && !raw.is_null() {
        // SAFETY: on success Windows returns a NUL-terminated string, valid
        // until it is freed below.
        let units = unsafe {
            let length = (0..).take_while(|&index| *raw.add(index) != 0).count();
            std::slice::from_raw_parts(raw, length)
        };
        Ok(PathBuf::from(std::ffi::OsString::from_wide(units)))
    } else {
        Err(io::Error::from_raw_os_error(status))
    };
    // SAFETY: the caller frees the string whether or not the call succeeded;
    // freeing null is allowed.
    unsafe { CoTaskMemFree(raw.cast()) };
    path
}

/// Create the directory `path` with the owner and protected DACL that
/// [`protect_directory`] applies, given to `CreateDirectoryW` itself. It
/// never inherits its parent's entries, even for a moment, so no other
/// account can open it with rights it would keep once the DACL changed.
/// Fails with [`io::ErrorKind::AlreadyExists`] when anything has the name.
pub fn create_protected_directory(path: &Path) -> io::Result<()> {
    create_directory_with(path, DIRECTORY_SDDL)?;
    // What was created must be the real, private directory just made.
    verify(&open(path, false)?)
}

fn create_directory_with(path: &Path, sddl: &str) -> io::Result<()> {
    let mut name: Vec<u16> = path.as_os_str().encode_wide().collect();
    if name.contains(&0) {
        return Err(io::Error::new(io::ErrorKind::InvalidInput, "NUL in path"));
    }
    name.push(0);
    let text: Vec<u16> = sddl.encode_utf16().chain(Some(0)).collect();
    let mut raw = std::ptr::null_mut();
    // SAFETY: NUL-terminated SDDL and a valid out pointer; Windows allocates
    // the descriptor, which `Descriptor` frees.
    if unsafe {
        ConvertStringSecurityDescriptorToSecurityDescriptorW(
            text.as_ptr(),
            1,
            &mut raw,
            std::ptr::null_mut(),
        )
    } == 0
    {
        return Err(io::Error::last_os_error());
    }
    let descriptor = Descriptor(raw);
    let attributes = SECURITY_ATTRIBUTES {
        nLength: std::mem::size_of::<SECURITY_ATTRIBUTES>() as u32,
        lpSecurityDescriptor: descriptor.0,
        bInheritHandle: 0,
    };
    // SAFETY: NUL-terminated path and attributes that outlive the call.
    if unsafe { CreateDirectoryW(name.as_ptr(), &attributes) } == 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(())
}

/// Restrict a dedicated staging directory before writing any secret bytes.
pub fn protect_directory(path: &Path) -> io::Result<()> {
    let directory = open(path, true)?;
    protect(&directory, true)
}

pub fn protect_file(path: &Path) -> io::Result<()> {
    let file = open(path, true)?;
    protect(&file, false)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A directory named vacancyd with a configuration in it, whose
    /// permissions also let ordinary users read it.
    fn readable_vacancyd_directory(parent: &Path) -> PathBuf {
        let directory = parent.join("vacancyd");
        std::fs::create_dir_all(&directory).unwrap();
        std::fs::write(directory.join("vacancyd.toml"), b"# test\n").unwrap();
        let granted = std::process::Command::new("icacls")
            .arg(&directory)
            .args(["/grant", "*S-1-5-32-545:(RX)"])
            .output()
            .unwrap();
        assert!(granted.status.success(), "{granted:?}");
        directory
    }

    #[test]
    fn protect_config_refuses_a_vacancyd_directory_outside_the_protected_folder() {
        let outside = crate::secure_file::protected_directory_outside_test_root();
        let directory = readable_vacancyd_directory(&outside);

        let refused = protect_config(&directory.join("vacancyd.toml"), false).unwrap_err();

        assert!(refused.to_string().contains("is not inside"), "{refused}");
        let sddl = descriptor_of(&directory).unwrap();
        assert!(sddl.contains(";BU)"), "permissions were changed: {sddl}");
        std::fs::remove_dir_all(outside).unwrap();
    }

    #[test]
    fn protect_config_refuses_a_path_through_a_junction() {
        // The final directory is real; a folder above it is a junction, which
        // the old check of the final name alone did not see.
        let base = crate::secure_file::test_root().join(format!(
            "protect-junction-{}",
            crate::setup::nonce().unwrap()
        ));
        create_protected_directory(&base).unwrap();
        let real = base.join("real");
        create_protected_directory(&real).unwrap();
        let directory = readable_vacancyd_directory(&real);
        let link = base.join("link");
        crate::secure_file::junction(&link, &real);

        let refused =
            protect_config(&link.join("vacancyd").join("vacancyd.toml"), false).unwrap_err();

        assert!(refused.to_string().contains("reparse point"), "{refused}");
        let sddl = descriptor_of(&directory).unwrap();
        assert!(sddl.contains(";BU)"), "permissions were changed: {sddl}");
        std::fs::remove_dir(&link).unwrap();
        std::fs::remove_dir_all(base).unwrap();
    }

    #[test]
    fn protect_config_still_repairs_a_directory_inside_the_protected_folder() {
        let base = crate::secure_file::test_root()
            .join(format!("protect-repair-{}", crate::setup::nonce().unwrap()));
        create_protected_directory(&base).unwrap();
        let directory = readable_vacancyd_directory(&base);
        assert!(verify_directory(&directory).is_err());

        protect_config(&directory.join("vacancyd.toml"), false).unwrap();

        verify_config(&directory.join("vacancyd.toml")).unwrap();
        std::fs::remove_dir_all(base).unwrap();
    }

    /// The installed case: the directory being repaired is the protected
    /// root itself. A root of this test's own stands in for it, named
    /// vacancyd as protect-config requires, so loosening it cannot affect
    /// other tests.
    #[test]
    fn protect_config_repairs_the_protected_root_itself() {
        let base = crate::secure_file::protected_directory_outside_test_root();
        let root = readable_vacancyd_directory(&base);
        assert!(verify_directory(&root).is_err());
        let config = root.join("vacancyd.toml");

        protect_config_under(&root, &config, false).unwrap();

        verify_config(&config).unwrap();
        std::fs::remove_dir_all(base).unwrap();
    }

    #[test]
    fn program_data_is_read_from_the_known_folder_record() {
        // On an ordinary machine the record and the variable agree; the
        // service's root is built from the record alone.
        let folder = program_data().unwrap();
        assert!(folder.is_absolute(), "{}", folder.display());
        let variable = std::env::var_os("ProgramData").unwrap();
        assert!(
            folder.as_os_str().eq_ignore_ascii_case(&variable),
            "{} and {}",
            folder.display(),
            variable.display()
        );
    }

    /// A scratch parent with `sddl`, under the temporary directory: these
    /// tests call the creation primitive directly, without the path check.
    fn parent_with(name: &str, sddl: &str) -> PathBuf {
        let parent = std::env::temp_dir().join(format!(
            "vacancyd-{name}-{}",
            crate::setup::nonce().unwrap()
        ));
        create_directory_with(&parent, sddl).unwrap();
        parent
    }

    #[test]
    fn a_new_directory_never_carries_rights_its_parent_would_pass_on() {
        // The parent passes on to every new subdirectory write access for
        // Users, which a handle opened before a later protection would keep.
        let parent = parent_with(
            "inherit",
            "D:P(A;OICI;FA;;;SY)(A;OICI;FA;;;BA)(A;CIIO;FW;;;BU)",
        );
        let child = parent.join("cache");

        create_protected_directory(&child).expect("the descriptor is applied at creation");

        let sddl = descriptor_of(&child).unwrap();
        assert!(!sddl.contains(";BU)"), "{sddl}");
        assert!(!sddl.contains("ID;"), "nothing is inherited: {sddl}");
        assert_eq!(crate::acl_policy::private_to_administrators(&sddl), Ok(()));
        assert_eq!(
            create_protected_directory(&child).unwrap_err().kind(),
            io::ErrorKind::AlreadyExists
        );
        std::fs::remove_dir(&child).unwrap();
        std::fs::remove_dir(&parent).unwrap();
    }

    /// Where the permissions a new directory would inherit forbid changing
    /// them, Windows refuses to create it with a descriptor of its own, so
    /// protected creation fails and leaves nothing behind. The route it
    /// replaced, creating the directory and protecting it afterwards, would
    /// instead leave a directory carrying the parent's rights, because its
    /// protection fails there. (An enabled restore privilege would bypass
    /// the denial; this test expects it to be disabled.)
    #[test]
    fn protected_creation_fails_closed_where_inherited_rights_forbid_changing_them() {
        // Every new subdirectory inherits a denial of WRITE_DAC and
        // WRITE_OWNER to everyone, and OWNER RIGHTS limited to READ_CONTROL.
        let parent = parent_with(
            "forbid",
            "D:P(D;CIIO;WDWO;;;WD)(A;OICI;FA;;;SY)(A;OICI;FA;;;BA)(A;;FA;;;OW)(A;CIIO;RC;;;OW)(A;CIIO;FW;;;BU)",
        );
        let child = parent.join("cache");

        let refused = create_protected_directory(&child).unwrap_err();

        assert_eq!(refused.kind(), io::ErrorKind::PermissionDenied, "{refused}");
        assert!(!child.exists());
        let inherited = parent.join("inherited");
        std::fs::create_dir(&inherited).unwrap();
        assert!(protect_directory(&inherited).is_err());
        let sddl = descriptor_of(&inherited).unwrap();
        assert!(sddl.contains(";BU)"), "{sddl}");
        std::fs::remove_dir(&inherited).unwrap();
        std::fs::remove_dir(&parent).unwrap();
    }
}
