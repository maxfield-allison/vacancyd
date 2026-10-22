//! Private file creation, checked reads and protected directory paths.
//! Used for credentials, configuration, backups and calibration state.

use std::{fs::File, io, path::Path};

/// Open an existing regular file without following its final symbolic link.
/// Unix opens are nonblocking so a substituted FIFO cannot stall a service
/// thread before the handle's actual file type can be checked.
pub fn open_regular_read(path: &Path) -> io::Result<File> {
    let mut options = std::fs::OpenOptions::new();
    options.read(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK);
    }
    #[cfg(windows)]
    {
        use std::os::windows::fs::OpenOptionsExt;
        use windows_sys::Win32::Storage::FileSystem::{
            FILE_FLAG_OPEN_REPARSE_POINT, FILE_SHARE_DELETE, FILE_SHARE_READ,
        };
        options
            .custom_flags(FILE_FLAG_OPEN_REPARSE_POINT)
            // Atomic settings replacement may proceed while this handle keeps
            // reading the original file; in-place writes are not shared.
            .share_mode(FILE_SHARE_READ | FILE_SHARE_DELETE);
    }
    let file = options.open(path)?;
    let metadata = file.metadata()?;
    #[cfg(windows)]
    {
        use std::os::windows::fs::MetadataExt;
        use windows_sys::Win32::Storage::FileSystem::FILE_ATTRIBUTE_REPARSE_POINT;
        if metadata.file_attributes() & FILE_ATTRIBUTE_REPARSE_POINT != 0 {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "reparse point refused",
            ));
        }
    }
    if !metadata.is_file() {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "regular file required",
        ));
    }
    Ok(file)
}

#[cfg(unix)]
pub fn create_new(path: &Path) -> io::Result<File> {
    use std::os::unix::fs::OpenOptionsExt;
    std::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o600)
        .open(path)
}

#[cfg(windows)]
pub fn create_new(path: &Path) -> io::Result<File> {
    use std::os::windows::{ffi::OsStrExt, io::FromRawHandle};
    use windows_sys::Win32::{
        Foundation::{LocalFree, GENERIC_WRITE, INVALID_HANDLE_VALUE},
        Security::{
            Authorization::ConvertStringSecurityDescriptorToSecurityDescriptorW,
            SECURITY_ATTRIBUTES,
        },
        Storage::FileSystem::{CreateFileW, CREATE_NEW, FILE_ATTRIBUTE_NORMAL, FILE_SHARE_READ},
    };
    // Protected DACL: no inherited Users/Everyone permissions. OW grants the
    // file's owner access; SY permits the installed LocalSystem service.
    // https://learn.microsoft.com/en-us/windows/win32/secauthz/sid-strings
    let sddl: Vec<u16> = "D:P(A;;FA;;;SY)(A;;FA;;;BA)(A;;FA;;;OW)\0"
        .encode_utf16()
        .collect();
    let mut descriptor = std::ptr::null_mut();
    // SAFETY: NUL-terminated SDDL and valid out pointer; the descriptor is
    // allocated by Windows and released after CreateFile has copied it.
    if unsafe {
        ConvertStringSecurityDescriptorToSecurityDescriptorW(
            sddl.as_ptr(),
            1,
            &mut descriptor,
            std::ptr::null_mut(),
        )
    } == 0
    {
        return Err(io::Error::last_os_error());
    }
    let name: Vec<u16> = path.as_os_str().encode_wide().chain(Some(0)).collect();
    if name[..name.len() - 1].contains(&0) {
        // SAFETY: descriptor was allocated by the successful conversion above.
        unsafe {
            LocalFree(descriptor);
        }
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "NUL in file path",
        ));
    }
    let attributes = SECURITY_ATTRIBUTES {
        nLength: std::mem::size_of::<SECURITY_ATTRIBUTES>() as u32,
        lpSecurityDescriptor: descriptor,
        bInheritHandle: 0,
    };
    // CREATE_NEW fails for existing files, including symlinks. Applying a
    // descriptor to an existing file via CreateFile would NOT change its ACL.
    // https://learn.microsoft.com/en-us/windows/win32/api/fileapi/nf-fileapi-createfilew
    // SAFETY: all pointers remain valid for the call; no handle is inherited.
    let handle = unsafe {
        CreateFileW(
            name.as_ptr(),
            GENERIC_WRITE,
            FILE_SHARE_READ,
            &attributes,
            CREATE_NEW,
            FILE_ATTRIBUTE_NORMAL,
            std::ptr::null_mut(),
        )
    };
    let error = io::Error::last_os_error();
    // SAFETY: descriptor allocation belongs to LocalFree, not Rust.
    unsafe {
        LocalFree(descriptor);
    }
    if handle == INVALID_HANDLE_VALUE {
        return Err(error);
    }
    // SAFETY: uniquely owned successful file handle; File closes it on drop.
    Ok(unsafe { File::from_raw_handle(handle) })
}

/// Why another account could change what `directory` holds, or `None` when
/// only this service can. On Unix the directory must be owned by the
/// service's account or root and writable by neither group nor others; a
/// sticky directory such as `/tmp` is still refused, because another account
/// could create a name in it first. On Windows it must grant access only to
/// Administrators and LocalSystem. A missing directory is an error of kind
/// [`io::ErrorKind::NotFound`].
#[cfg(unix)]
pub fn shared_directory(directory: &Path) -> io::Result<Option<String>> {
    use std::os::unix::fs::MetadataExt;
    let metadata = std::fs::metadata(directory)?;
    if !metadata.is_dir() {
        return Ok(Some("it is not a directory".into()));
    }
    // SAFETY: geteuid has no preconditions.
    let me = unsafe { libc::geteuid() };
    if metadata.uid() != me && metadata.uid() != 0 {
        return Ok(Some(format!(
            "it is owned by uid {}, not by this service (uid {me}) or root",
            metadata.uid()
        )));
    }
    if metadata.mode() & 0o022 != 0 {
        return Ok(Some(format!(
            "it is writable by group or others (mode {:o})",
            metadata.mode() & 0o7777
        )));
    }
    Ok(None)
}

#[cfg(windows)]
pub fn shared_directory(directory: &Path) -> io::Result<Option<String>> {
    match crate::windows_acl::verify_directory(directory) {
        Ok(()) => Ok(None),
        // Read, and not private. An error from Windows itself keeps its code.
        Err(error) if error.raw_os_error().is_none() => Ok(Some(error.to_string())),
        Err(error) => Err(error),
    }
}

/// Why another account could replace `directory`, or a directory on the way
/// to it, with one of its own choosing, or `None` when none can. A service
/// that runs as root and changes or uses a directory through a path another
/// account can redirect would act on whatever that account points it at.
///
/// Every directory the path is looked up in, following symbolic links, must
/// be owned by this service's account or root and be writable by neither
/// group nor others, or else sticky: a sticky directory such as `/tmp` lets
/// others add names but not replace this service's, so everything this path
/// finds in one must belong to this service's account or root, symbolic
/// links included. The walk stops at the first missing name, which
/// [`create_private_dir_all`] then creates inside a directory that passed.
/// Whether `directory` itself is private is [`shared_directory`]'s question.
#[cfg(unix)]
pub fn replaceable_path(directory: &Path) -> io::Result<Option<String>> {
    use std::os::unix::fs::MetadataExt;
    use std::path::PathBuf;
    // The kernel's own limit on links followed in one lookup.
    const MAX_LINKS: u32 = 40;
    // SAFETY: geteuid has no preconditions.
    let me = unsafe { libc::geteuid() };
    let owner_reason = |path: &Path, uid: u32| {
        (uid != me && uid != 0).then(|| {
            format!(
                "{}: it is owned by uid {uid}, not by this service (uid {me}) or root",
                path.display()
            )
        })
    };
    let components = |path: &Path| {
        path.components()
            .rev()
            .map(|part| PathBuf::from(part.as_os_str()))
            .collect::<Vec<_>>()
    };
    let root = Path::new("/");
    if let Some(reason) = owner_reason(root, std::fs::metadata(root)?.uid()) {
        return Ok(Some(reason));
    }
    let mut pending = components(&std::path::absolute(directory)?);
    let mut resolved = root.to_path_buf();
    let mut links = 0;
    while let Some(part) = pending.pop() {
        if part == root {
            resolved = root.to_path_buf();
            continue;
        }
        if part == Path::new(".") {
            continue;
        }
        if part == Path::new("..") {
            // `resolved` holds no links, so its parent is the real one.
            resolved.pop();
            continue;
        }
        let next = resolved.join(&part);
        let meta = match std::fs::symlink_metadata(&next) {
            Ok(meta) => meta,
            Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(None),
            Err(error) => return Err(error),
        };
        if let Some(reason) = owner_reason(&next, meta.uid()) {
            return Ok(Some(reason));
        }
        if meta.file_type().is_symlink() {
            links += 1;
            if links > MAX_LINKS {
                return Ok(Some(format!(
                    "{}: it resolves through too many symbolic links",
                    next.display()
                )));
            }
            // A relative target continues from the link's own directory.
            pending.extend(components(&std::fs::read_link(&next)?));
            continue;
        }
        if !meta.is_dir() {
            return Ok(Some(format!("{}: it is not a directory", next.display())));
        }
        let sticky = meta.mode() & 0o1000 != 0;
        if meta.mode() & 0o022 != 0 && !sticky {
            return Ok(Some(format!(
                "{}: it is writable by group or others (mode {:o})",
                next.display(),
                meta.mode() & 0o7777
            )));
        }
        resolved = next;
    }
    Ok(None)
}

/// On Windows the question is narrower: `directory` must be inside the
/// protected `ProgramData\vacancyd` folder as written, through no reparse
/// point, and private to Administrators and LocalSystem from that folder
/// down. See [`crate::windows_acl::replaceable_path`].
#[cfg(windows)]
pub fn replaceable_path(directory: &Path) -> io::Result<Option<String>> {
    crate::windows_acl::replaceable_path(directory)
}

/// Create `directory` and any missing directories above it. Unix creates each
/// with mode 0700 and syncs its parent. Windows protects each new directory
/// for Administrators and LocalSystem before creating anything inside it.
/// Nothing that already exists is changed. The function checks the path with
/// [`replaceable_path`] after scanning for missing directories and before
/// creating them. A name found missing by the scan must still be missing
/// when its creation is attempted.
pub fn create_private_dir_all(directory: &Path) -> io::Result<()> {
    let mut missing = Vec::new();
    let mut cursor = Some(directory);
    while let Some(next) = cursor.filter(|next| !next.as_os_str().is_empty()) {
        match std::fs::symlink_metadata(next) {
            Ok(_) => break,
            Err(error) if error.kind() == io::ErrorKind::NotFound => {
                missing.push(next.to_path_buf());
                cursor = next.parent();
            }
            Err(error) => return Err(error),
        }
    }
    // The path is judged again after the scan, so a directory another
    // account created since the caller's own check is judged too rather
    // than built on. One created after this point takes a name the scan
    // found missing, and its creation below fails instead of adopting it.
    if let Some(reason) = replaceable_path(directory)? {
        return Err(io::Error::new(io::ErrorKind::PermissionDenied, reason));
    }
    // Outermost first: each new directory's entry lives in its parent.
    for next in missing.iter().rev() {
        create_private_dir(next)?;
        let parent = next
            .parent()
            .filter(|parent| !parent.as_os_str().is_empty())
            .unwrap_or_else(|| Path::new("."));
        crate::durable::sync_dir(parent)?;
    }
    Ok(())
}

/// Create `directory` for a caller running as root, LocalSystem or an
/// administrator when it is missing, through [`create_private_dir_all`]: the
/// path is checked first and every missing directory is private from
/// creation. On Unix, each new entry is synced in its parent, and a
/// directory that already exists is left to the caller's own checks. On
/// Windows an existing directory is checked too, so it must be inside the
/// protected `ProgramData\vacancyd` folder like a new one. Setup,
/// migration, durable file writes and the service log use this wrapper;
/// pressure-cache and admission-recovery creation call
/// [`create_private_dir_all`] directly.
pub fn create_missing_private_dir(directory: &Path) -> io::Result<()> {
    if cfg!(windows) {
        // Finds nothing missing when the directory exists, and checks it.
        return create_private_dir_all(directory);
    }
    match std::fs::symlink_metadata(directory) {
        Ok(_) => Ok(()),
        Err(error) if error.kind() == io::ErrorKind::NotFound => create_private_dir_all(directory),
        Err(error) => Err(error),
    }
}

/// A base directory for tests that create directories through the checked
/// route. On Unix it is the temporary directory. On Windows it stands in for
/// the protected `ProgramData\vacancyd` folder, which the path check
/// requires (see `windows_acl::protected_root`): one directory per test
/// process under the `ProgramData` environment variable's directory, created
/// protected for Administrators and LocalSystem. Tests remove what they
/// create in it; the empty base is left behind.
#[cfg(test)]
pub fn test_root() -> std::path::PathBuf {
    #[cfg(unix)]
    {
        std::env::temp_dir()
    }
    #[cfg(windows)]
    {
        static ROOT: std::sync::OnceLock<std::path::PathBuf> = std::sync::OnceLock::new();
        ROOT.get_or_init(|| {
            let program_data = std::env::var_os("ProgramData").expect("ProgramData is set");
            let root = Path::new(&program_data)
                .join(format!("vacancyd-tests-{}", crate::setup::nonce().unwrap()));
            crate::windows_acl::create_protected_directory(&root).unwrap();
            root
        })
        .clone()
    }
}

/// A directory outside [`test_root`] that would pass every check except
/// being inside the protected folder: a new directory under ProgramData,
/// protected for Administrators and LocalSystem. The caller removes it.
#[cfg(all(test, windows))]
pub fn protected_directory_outside_test_root() -> std::path::PathBuf {
    let program_data = std::env::var_os("ProgramData").expect("ProgramData is set");
    let directory = Path::new(&program_data).join(format!(
        "vacancyd-outside-{}",
        crate::setup::nonce().unwrap()
    ));
    crate::windows_acl::create_protected_directory(&directory).unwrap();
    directory
}

/// Make `link` a directory junction to `target`, as an unelevated program
/// could where it may create names.
#[cfg(all(test, windows))]
pub fn junction(link: &Path, target: &Path) {
    let made = std::process::Command::new("cmd")
        .args(["/C", "mklink", "/J"])
        .arg(link)
        .arg(target)
        .output()
        .unwrap();
    assert!(made.status.success(), "{made:?}");
}

/// `mkdir` with mode 0700. A umask only removes permissions, so nothing has
/// to be changed afterwards through a path that could have been swapped.
#[cfg(unix)]
fn create_private_dir(directory: &Path) -> io::Result<()> {
    use std::os::unix::fs::DirBuilderExt;
    std::fs::DirBuilder::new().mode(0o700).create(directory)
}

/// Created with its protected owner and DACL in one call, so it never holds
/// rights inherited from its parent that a handle opened in the meantime
/// could keep after a later permission change.
#[cfg(windows)]
fn create_private_dir(directory: &Path) -> io::Result<()> {
    crate::windows_acl::create_protected_directory(directory)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;

    #[test]
    fn private_creation_is_readable_by_owner_and_never_clobbers() {
        let path = std::env::temp_dir().join(format!(
            "vacancyd-private-{}",
            crate::setup::nonce().unwrap()
        ));
        let mut file = create_new(&path).unwrap();
        file.write_all(b"test-only-data").unwrap();
        drop(file);
        assert_eq!(std::fs::read(&path).unwrap(), b"test-only-data");
        assert_eq!(
            create_new(&path).unwrap_err().kind(),
            io::ErrorKind::AlreadyExists
        );
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            assert_eq!(
                std::fs::metadata(&path).unwrap().permissions().mode() & 0o777,
                0o600
            );
        }
        #[cfg(windows)]
        assert_private_dacl(&path);
        std::fs::remove_file(path).unwrap();
    }

    /// An empty scratch directory with `mode`, under the sticky temp root.
    #[cfg(unix)]
    fn scratch(name: &str, mode: u32) -> std::path::PathBuf {
        use std::os::unix::fs::PermissionsExt;
        let directory = std::env::temp_dir().join(format!(
            "vacancyd-path-{name}-{}",
            crate::setup::nonce().unwrap()
        ));
        std::fs::create_dir(&directory).unwrap();
        std::fs::set_permissions(&directory, std::fs::Permissions::from_mode(mode)).unwrap();
        directory
    }

    #[cfg(unix)]
    #[test]
    fn a_path_through_a_directory_others_can_write_is_refused_before_anything_is_created() {
        // Another account controls the parent and could swap the new
        // directory for a link to one of the service's own between its
        // creation and any permission change.
        let parent = scratch("open-parent", 0o777);
        let cache = parent.join("cache");
        let reason = replaceable_path(&cache).unwrap().unwrap();
        assert!(reason.contains("writable by group or others"), "{reason}");
        assert!(
            reason.starts_with(&parent.display().to_string()),
            "{reason}"
        );
        assert!(!cache.exists());
        std::fs::remove_dir(parent).unwrap();
    }

    #[cfg(unix)]
    #[test]
    fn a_link_into_a_directory_others_can_write_is_followed_and_refused() {
        let private = scratch("link-private", 0o700);
        let open = scratch("link-open", 0o777);
        let link = private.join("link");
        std::os::unix::fs::symlink(&open, &link).unwrap();
        let reason = replaceable_path(&link.join("cache")).unwrap().unwrap();
        assert!(reason.contains("writable by group or others"), "{reason}");
        // A relative link resolves from its own directory.
        std::fs::remove_file(&link).unwrap();
        let open_name = open.file_name().unwrap();
        std::os::unix::fs::symlink(std::path::Path::new("..").join(open_name), &link).unwrap();
        assert!(replaceable_path(&link.join("cache")).unwrap().is_some());
        std::fs::remove_file(&link).unwrap();
        std::fs::remove_dir(private).unwrap();
        std::fs::remove_dir(open).unwrap();
    }

    #[cfg(unix)]
    #[test]
    fn a_private_path_below_a_sticky_directory_can_be_created_and_is_private() {
        use std::os::unix::fs::PermissionsExt;
        // The temp root is sticky: others may add names but not replace these.
        let parent = scratch("sticky-child", 0o700);
        let cache = parent.join("a").join("b");
        assert_eq!(replaceable_path(&cache).unwrap(), None);
        create_private_dir_all(&cache).unwrap();
        for directory in [parent.join("a"), cache.clone()] {
            let mode = std::fs::metadata(&directory).unwrap().permissions().mode();
            assert_eq!(mode & 0o777, 0o700, "{}: {mode:o}", directory.display());
        }
        assert_eq!(replaceable_path(&cache).unwrap(), None);
        assert_eq!(shared_directory(&cache).unwrap(), None);
        // A directory that is already there is refused, not adopted.
        assert_eq!(
            create_private_dir(&cache).unwrap_err().kind(),
            io::ErrorKind::AlreadyExists
        );
        std::fs::remove_dir_all(parent).unwrap();
    }

    #[cfg(unix)]
    #[test]
    fn privileged_creation_leaves_existing_directories_and_refuses_open_parents() {
        use std::os::unix::fs::PermissionsExt;
        // An existing directory is the caller's to judge, unchanged.
        let existing = scratch("existing", 0o755);
        create_missing_private_dir(&existing).unwrap();
        let mode = std::fs::metadata(&existing).unwrap().permissions().mode();
        assert_eq!(mode & 0o777, 0o755);
        // A missing one under a parent another account can change is refused
        // before anything is created.
        let open = scratch("open", 0o777);
        let target = open.join("vacancyd").join("admission");
        let refused = create_missing_private_dir(&target).unwrap_err();
        assert!(
            refused.to_string().contains("writable by group or others"),
            "{refused}"
        );
        assert!(!open.join("vacancyd").exists());
        std::fs::remove_dir(existing).unwrap();
        std::fs::remove_dir(open).unwrap();
    }

    #[cfg(unix)]
    #[test]
    fn a_directory_planted_on_the_path_after_its_check_is_not_built_on() {
        use std::os::unix::fs::PermissionsExt;
        // The caller's check passes while `new` is still missing.
        let parent = scratch("planted", 0o700);
        let planted = parent.join("new");
        let cache = planted.join("cache");
        assert_eq!(replaceable_path(&cache).unwrap(), None);
        // Then another account creates `new` and keeps the right to replace
        // what the service puts in it.
        std::fs::create_dir(&planted).unwrap();
        std::fs::set_permissions(&planted, std::fs::Permissions::from_mode(0o777)).unwrap();

        let refused = create_private_dir_all(&cache).unwrap_err();
        assert_eq!(refused.kind(), io::ErrorKind::PermissionDenied);
        assert!(
            refused.to_string().contains("writable by group or others"),
            "{refused}"
        );
        assert!(!cache.exists());
        std::fs::remove_dir_all(parent).unwrap();
    }

    #[cfg(windows)]
    #[test]
    fn program_data_is_accepted_above_the_deepest_existing_directory() {
        // The real descriptor: a path through ProgramData into a directory
        // only Administrators and LocalSystem can change must stay usable.
        let program_data = std::env::var_os("ProgramData").unwrap();
        let sddl = crate::windows_acl::descriptor_of(std::path::Path::new(&program_data)).unwrap();
        assert_eq!(
            crate::acl_policy::others_cannot_replace(&sddl, false),
            Ok(()),
            "{sddl}"
        );
    }

    #[cfg(windows)]
    #[test]
    fn a_private_directory_outside_the_protected_folder_is_refused_existing_or_not() {
        let outside = protected_directory_outside_test_root();
        let reason = replaceable_path(&outside).unwrap().unwrap();
        assert!(reason.contains("is not inside"), "{reason}");
        assert!(
            reason.contains(&test_root().display().to_string()),
            "{reason}"
        );
        // It exists and is private, so before the limit it was left alone.
        let refused = create_missing_private_dir(&outside).unwrap_err();
        assert_eq!(refused.kind(), io::ErrorKind::PermissionDenied);
        let missing = outside.join("cache");
        assert!(create_private_dir_all(&missing).is_err());
        assert!(!missing.exists());
        std::fs::remove_dir(outside).unwrap();
    }

    /// The containment check compares the path after Windows has made it
    /// absolute, so a raw spelling must be judged by where it leads.
    #[cfg(windows)]
    #[test]
    fn raw_spellings_that_lead_outside_the_protected_folder_are_refused() {
        let root = test_root().display().to_string();
        let parent = test_root().parent().unwrap().display().to_string();
        let name = format!("vacancyd-elsewhere-{}", crate::setup::nonce().unwrap());
        for spelling in [
            // Mixed separators and dot components.
            format!("{root}/../{name}"),
            format!("{root}\\.\\..\\{name}"),
            format!("{root}\\sub/../../{name}"),
            // Relative: the test process runs outside the protected folder.
            format!("{name}\\cache"),
            format!(".\\{name}"),
            // Trailing dots and spaces, which Windows removes.
            format!("{parent}\\{name}."),
            format!("{parent}\\{name} "),
            format!("{parent}\\{name}. \\cache"),
            // Verbatim paths keep their spelling and are compared literally.
            format!("\\\\?\\{root}\\..\\{name}"),
            format!("\\\\?\\{root}/cache"),
            format!("\\\\?\\{root}.\\cache"),
        ] {
            let reason = replaceable_path(Path::new(&spelling)).unwrap();
            assert!(reason.is_some(), "{spelling} was accepted");
        }
        // Spellings that lead inside it are still accepted.
        for spelling in [format!("{root}/cache"), format!("{root}\\cache\\..")] {
            assert_eq!(
                replaceable_path(Path::new(&spelling)).unwrap(),
                None,
                "{spelling}"
            );
        }
        assert!(!Path::new(&parent).join(&name).exists());
    }

    #[cfg(windows)]
    #[test]
    fn a_path_through_a_junction_inside_the_protected_folder_is_refused() {
        let base = test_root().join(format!("junction-{}", crate::setup::nonce().unwrap()));
        crate::windows_acl::create_protected_directory(&base).unwrap();
        let target = base.join("target");
        crate::windows_acl::create_protected_directory(&target).unwrap();
        let link = base.join("link");
        junction(&link, &target);

        let reason = replaceable_path(&link).unwrap().unwrap();
        assert!(reason.contains("reparse point"), "{reason}");
        assert!(replaceable_path(&link.join("cache")).unwrap().is_some());
        // An existing directory used to be left to the caller unchecked.
        assert!(create_missing_private_dir(&link).is_err());
        assert!(create_missing_private_dir(&link.join("cache")).is_err());
        assert!(!target.join("cache").exists());
        std::fs::remove_dir(&link).unwrap();
        std::fs::remove_dir_all(base).unwrap();
    }

    #[cfg(windows)]
    #[test]
    fn a_directory_another_account_holds_any_right_on_is_refused_inside_the_folder() {
        // Write-attributes access is enough to turn an empty directory into
        // a junction after it has been checked.
        let directory = test_root().join(format!("granted-{}", crate::setup::nonce().unwrap()));
        crate::windows_acl::create_protected_directory(&directory).unwrap();
        assert_eq!(replaceable_path(&directory).unwrap(), None);
        let granted = std::process::Command::new("icacls")
            .arg(&directory)
            .args(["/grant", "*S-1-5-32-545:(WA)"])
            .output()
            .unwrap();
        assert!(granted.status.success(), "{granted:?}");

        let reason = replaceable_path(&directory).unwrap().unwrap();
        assert!(reason.contains("access is granted to"), "{reason}");
        assert!(create_missing_private_dir(&directory).is_err());
        assert!(create_private_dir_all(&directory.join("cache")).is_err());
        assert!(!directory.join("cache").exists());
        std::fs::remove_dir(directory).unwrap();
    }

    #[cfg(unix)]
    #[test]
    fn a_directory_others_can_write_is_shared() {
        use std::os::unix::fs::PermissionsExt;
        let directory = std::env::temp_dir().join(format!(
            "vacancyd-shared-{}",
            crate::setup::nonce().unwrap()
        ));
        std::fs::create_dir(&directory).unwrap();
        let set = |mode| {
            std::fs::set_permissions(&directory, std::fs::Permissions::from_mode(mode)).unwrap()
        };
        set(0o700);
        assert_eq!(shared_directory(&directory).unwrap(), None);
        set(0o755);
        assert_eq!(shared_directory(&directory).unwrap(), None);
        for mode in [0o775, 0o757, 0o1777] {
            set(mode);
            let reason = shared_directory(&directory).unwrap().unwrap();
            assert!(reason.contains("writable by group or others"), "{reason}");
        }
        assert_eq!(
            shared_directory(&directory.join("missing"))
                .unwrap_err()
                .kind(),
            io::ErrorKind::NotFound
        );
        std::fs::remove_dir(directory).unwrap();
    }

    #[cfg(windows)]
    fn assert_private_dacl(path: &Path) {
        use std::os::windows::ffi::OsStrExt;
        use windows_sys::Win32::{
            Foundation::LocalFree,
            Security::{
                Authorization::{
                    ConvertSecurityDescriptorToStringSecurityDescriptorW, GetNamedSecurityInfoW,
                    SE_FILE_OBJECT,
                },
                DACL_SECURITY_INFORMATION,
            },
        };
        let name: Vec<u16> = path.as_os_str().encode_wide().chain(Some(0)).collect();
        let mut descriptor = std::ptr::null_mut();
        // SAFETY: NUL-terminated path, valid out pointer; unrequested outputs
        // are null. Windows owns the returned allocation until LocalFree.
        let status = unsafe {
            GetNamedSecurityInfoW(
                name.as_ptr(),
                SE_FILE_OBJECT,
                DACL_SECURITY_INFORMATION,
                std::ptr::null_mut(),
                std::ptr::null_mut(),
                std::ptr::null_mut(),
                std::ptr::null_mut(),
                &mut descriptor,
            )
        };
        assert_eq!(status, 0);
        let mut sddl = std::ptr::null_mut();
        let mut length = 0;
        // SAFETY: valid descriptor and output pointers; conversion copies it.
        let converted = unsafe {
            ConvertSecurityDescriptorToStringSecurityDescriptorW(
                descriptor,
                1,
                DACL_SECURITY_INFORMATION,
                &mut sddl,
                &mut length,
            )
        };
        // SAFETY: allocation from successful GetNamedSecurityInfoW.
        unsafe {
            LocalFree(descriptor);
        }
        assert_ne!(converted, 0);
        // SAFETY: successful conversion returned a buffer of `length` UTF-16
        // units, valid until LocalFree below. Stop at the first terminal NUL;
        // the reported allocation can include additional NUL padding.
        let actual = unsafe {
            let units = std::slice::from_raw_parts(sddl, length as usize);
            let end = units.iter().position(|unit| *unit == 0).unwrap();
            String::from_utf16_lossy(&units[..end])
        };
        // SAFETY: allocation from successful conversion.
        unsafe {
            LocalFree(sddl.cast());
        }
        // Exact comparison proves no inherited or additional broad grants,
        // not just that the creating account can read its own test file.
        assert_eq!(actual, "D:P(A;;FA;;;SY)(A;;FA;;;BA)(A;;FA;;;OW)");
    }
}
