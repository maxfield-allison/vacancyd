//! Durable replacement of whole files.
//!
//! Writing a sibling file, syncing it and renaming it over the destination
//! makes a replacement atomic: a reader sees the old bytes or the new ones,
//! never a mixture. It does not make the replacement durable. The rename is a
//! change to the containing directory, and until that directory reaches
//! storage a power loss can bring the previous directory entry, and therefore
//! the previous file, back.
//!
//! - On Unix the directory itself is synced after the rename or link, and
//!   before the caller is told the file was saved. A newly created directory
//!   is made durable in its own parent the same way.
//! - On Windows `MoveFileExW` with `MOVEFILE_WRITE_THROUGH` does not return
//!   until the move is on disk, so no separate directory sync is needed (or
//!   available: Windows has no documented way to flush a directory). A new
//!   file's own creation is committed by `FlushFileBuffers` on that file,
//!   which is what [`std::fs::File::sync_all`] calls.
//!
//! Every function here expects the staged file to be fully written and synced
//! already, in the same directory as its destination.

use std::io::{self, Write};
use std::path::Path;

/// Why a replacement did not complete. The distinction matters to a caller
/// that reports a save: before the rename nothing changed, after it the new
/// file is already visible but may not survive a power loss.
#[derive(Debug)]
pub enum ReplaceError {
    /// The destination was not changed.
    NotReplaced(io::Error),
    /// The destination now names the new file, but the directory change could
    /// not be made durable. Readers see the new contents; after a power loss
    /// the old ones may return.
    NotDurable(io::Error),
}

impl std::fmt::Display for ReplaceError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::NotReplaced(error) => write!(formatter, "file was not replaced: {error}"),
            Self::NotDurable(error) => write!(
                formatter,
                "file was replaced, but its directory could not be synced to storage: {error}"
            ),
        }
    }
}

impl std::error::Error for ReplaceError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::NotReplaced(error) | Self::NotDurable(error) => Some(error),
        }
    }
}

impl From<ReplaceError> for io::Error {
    fn from(error: ReplaceError) -> Self {
        match error {
            ReplaceError::NotReplaced(error) => error,
            ReplaceError::NotDurable(_) => io::Error::other(error.to_string()),
        }
    }
}

/// Replace `destination` with the synced sibling `staged`. On `Ok` the new
/// contents are durable.
pub fn replace(staged: &Path, destination: &Path) -> Result<(), ReplaceError> {
    replace_with(&mut System, staged, destination)
}

/// Publish the synced sibling `staged` as `destination` only if no file of
/// that name exists; an existing destination fails with
/// [`io::ErrorKind::AlreadyExists`] and is left alone. `staged` no longer
/// exists afterwards on success. On `Ok` the new file is durable.
pub fn publish_new(staged: &Path, destination: &Path) -> Result<(), ReplaceError> {
    publish_new_with(&mut System, staged, destination)
}

/// Write `bytes` to `destination` through a synced sibling and [`replace`],
/// creating missing parent directories durably first.
///
/// The sibling is created new, under a name unique to this call, with
/// permissions private to the service from its first byte
/// ([`crate::secure_file::create_new`]). Creating it new means a file or
/// link already at that name is never opened, so a planted symbolic link
/// cannot make the service truncate the file it points to. Only the staged
/// file this call created is removed after a failure.
pub fn write(destination: &Path, bytes: &[u8]) -> io::Result<()> {
    let parent = parent_of(destination);
    crate::secure_file::create_missing_private_dir(parent)?;
    let staged = staging_path(destination)?;
    let mut file = crate::secure_file::create_new(&staged)?;
    // From here the staged file is this call's own, so a failure removes it.
    let written = file.write_all(bytes).and_then(|()| file.sync_all());
    drop(file);
    if let Err(error) = written {
        let _ = std::fs::remove_file(&staged);
        return Err(error);
    }
    replace(&staged, destination).map_err(|error| {
        if matches!(error, ReplaceError::NotReplaced(_)) {
            let _ = std::fs::remove_file(&staged);
        }
        error.into()
    })
}

/// A hidden sibling of `destination` no other call uses: the process id and
/// the time keep concurrent writers and earlier runs apart.
fn staging_path(destination: &Path) -> io::Result<std::path::PathBuf> {
    let name = destination
        .file_name()
        .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidInput, "path has no file name"))?;
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_err(io::Error::other)?
        .as_nanos();
    let mut staged = std::ffi::OsString::from(".");
    staged.push(name);
    staged.push(format!(".{}-{nanos}.tmp", std::process::id()));
    Ok(destination.with_file_name(staged))
}

/// Make the entries of `directory` (files created, renamed or removed in it)
/// durable. A no-op on Windows; see the module documentation.
pub fn sync_dir(directory: &Path) -> io::Result<()> {
    System.sync_dir(directory)
}

fn parent_of(path: &Path) -> &Path {
    path.parent()
        .filter(|parent| !parent.as_os_str().is_empty())
        .unwrap_or_else(|| Path::new("."))
}

/// The filesystem operations whose order makes a replacement durable,
/// separated so tests can record that order and inject failures.
trait Operations {
    /// Rename `from` over `to`, replacing any existing file.
    fn rename_replacing(&mut self, from: &Path, to: &Path) -> io::Result<()>;
    /// Give `from` the name `to` only if `to` does not exist, then remove the
    /// name `from`.
    fn rename_new(&mut self, from: &Path, to: &Path) -> io::Result<()>;
    fn sync_dir(&mut self, directory: &Path) -> io::Result<()>;
}

fn replace_with(
    operations: &mut impl Operations,
    staged: &Path,
    destination: &Path,
) -> Result<(), ReplaceError> {
    debug_assert_eq!(parent_of(staged), parent_of(destination));
    operations
        .rename_replacing(staged, destination)
        .map_err(ReplaceError::NotReplaced)?;
    operations
        .sync_dir(parent_of(destination))
        .map_err(ReplaceError::NotDurable)
}

fn publish_new_with(
    operations: &mut impl Operations,
    staged: &Path,
    destination: &Path,
) -> Result<(), ReplaceError> {
    debug_assert_eq!(parent_of(staged), parent_of(destination));
    operations
        .rename_new(staged, destination)
        .map_err(ReplaceError::NotReplaced)?;
    operations
        .sync_dir(parent_of(destination))
        .map_err(ReplaceError::NotDurable)
}

struct System;

#[cfg(unix)]
impl Operations for System {
    fn rename_replacing(&mut self, from: &Path, to: &Path) -> io::Result<()> {
        std::fs::rename(from, to)
    }

    fn rename_new(&mut self, from: &Path, to: &Path) -> io::Result<()> {
        // Unlike rename, link never replaces a file a racing writer created.
        std::fs::hard_link(from, to)?;
        // The destination is published either way; a staged name that could
        // not be removed is left for the caller's own cleanup.
        let _ = std::fs::remove_file(from);
        Ok(())
    }

    fn sync_dir(&mut self, directory: &Path) -> io::Result<()> {
        std::fs::File::open(directory)?.sync_all()
    }
}

#[cfg(windows)]
impl Operations for System {
    fn rename_replacing(&mut self, from: &Path, to: &Path) -> io::Result<()> {
        use windows_sys::Win32::Storage::FileSystem::{
            MOVEFILE_REPLACE_EXISTING, MOVEFILE_WRITE_THROUGH,
        };
        move_file(from, to, MOVEFILE_REPLACE_EXISTING | MOVEFILE_WRITE_THROUGH)
    }

    fn rename_new(&mut self, from: &Path, to: &Path) -> io::Result<()> {
        use windows_sys::Win32::Storage::FileSystem::MOVEFILE_WRITE_THROUGH;
        // Without MOVEFILE_REPLACE_EXISTING the move fails if `to` exists.
        move_file(from, to, MOVEFILE_WRITE_THROUGH)
    }

    fn sync_dir(&mut self, _directory: &Path) -> io::Result<()> {
        Ok(())
    }
}

#[cfg(windows)]
fn move_file(from: &Path, to: &Path, flags: u32) -> io::Result<()> {
    use std::os::windows::ffi::OsStrExt;
    use windows_sys::Win32::Storage::FileSystem::MoveFileExW;

    fn wide(path: &Path) -> io::Result<Vec<u16>> {
        let mut value: Vec<u16> = path.as_os_str().encode_wide().collect();
        if value.contains(&0) {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "NUL in file path",
            ));
        }
        value.push(0);
        Ok(value)
    }

    let from = wide(from)?;
    let to = wide(to)?;
    // SAFETY: both paths are NUL-terminated and live for the call.
    if unsafe { MoveFileExW(from.as_ptr(), to.as_ptr(), flags) } != 0 {
        Ok(())
    } else {
        // An existing destination (ERROR_ALREADY_EXISTS or ERROR_FILE_EXISTS)
        // decodes as io::ErrorKind::AlreadyExists.
        Err(io::Error::last_os_error())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    #[derive(Default)]
    struct Recorder {
        calls: Vec<String>,
        fail: Option<&'static str>,
    }

    impl Recorder {
        fn step(&mut self, name: &str, detail: String) -> io::Result<()> {
            self.calls.push(format!("{name} {detail}"));
            if self.fail == Some(name) {
                return Err(io::Error::other(format!("injected {name} failure")));
            }
            Ok(())
        }
    }

    impl Operations for Recorder {
        fn rename_replacing(&mut self, from: &Path, to: &Path) -> io::Result<()> {
            self.step(
                "rename_replacing",
                format!("{} -> {}", from.display(), to.display()),
            )
        }
        fn rename_new(&mut self, from: &Path, to: &Path) -> io::Result<()> {
            self.step(
                "rename_new",
                format!("{} -> {}", from.display(), to.display()),
            )
        }
        fn sync_dir(&mut self, directory: &Path) -> io::Result<()> {
            self.step("sync_dir", directory.display().to_string())
        }
    }

    fn paths() -> (PathBuf, PathBuf) {
        let directory = Path::new("state");
        (directory.join("a.toml.tmp"), directory.join("a.toml"))
    }

    #[test]
    fn replacement_syncs_the_directory_after_the_rename() {
        let (staged, destination) = paths();
        let mut recorder = Recorder::default();
        replace_with(&mut recorder, &staged, &destination).unwrap();
        assert_eq!(
            recorder.calls,
            [
                format!(
                    "rename_replacing {} -> {}",
                    staged.display(),
                    destination.display()
                ),
                format!("sync_dir {}", Path::new("state").display()),
            ]
        );

        let mut recorder = Recorder::default();
        publish_new_with(&mut recorder, &staged, &destination).unwrap();
        assert_eq!(recorder.calls.len(), 2);
        assert!(recorder.calls[0].starts_with("rename_new "));
        assert!(recorder.calls[1].starts_with("sync_dir "));
    }

    #[test]
    fn a_failed_rename_is_not_replaced_and_is_never_synced() {
        let (staged, destination) = paths();
        for fail in ["rename_replacing", "rename_new"] {
            let mut recorder = Recorder {
                fail: Some(fail),
                ..Recorder::default()
            };
            let result = if fail == "rename_new" {
                publish_new_with(&mut recorder, &staged, &destination)
            } else {
                replace_with(&mut recorder, &staged, &destination)
            };
            assert!(
                matches!(result, Err(ReplaceError::NotReplaced(_))),
                "{result:?}"
            );
            assert_eq!(recorder.calls.len(), 1, "{:?}", recorder.calls);
        }
    }

    #[test]
    fn a_failed_directory_sync_is_reported_as_replaced_but_not_durable() {
        let (staged, destination) = paths();
        let mut recorder = Recorder {
            fail: Some("sync_dir"),
            ..Recorder::default()
        };
        let result = replace_with(&mut recorder, &staged, &destination);
        assert!(
            matches!(result, Err(ReplaceError::NotDurable(_))),
            "{result:?}"
        );
        assert_eq!(recorder.calls.len(), 2);
        let mut recorder = Recorder {
            fail: Some("sync_dir"),
            ..Recorder::default()
        };
        assert!(matches!(
            publish_new_with(&mut recorder, &staged, &destination),
            Err(ReplaceError::NotDurable(_))
        ));
    }

    #[test]
    fn a_relative_file_name_syncs_the_current_directory() {
        let mut recorder = Recorder::default();
        replace_with(&mut recorder, Path::new("a.tmp"), Path::new("a")).unwrap();
        assert_eq!(recorder.calls[1], "sync_dir .");
    }

    fn scratch(name: &str) -> PathBuf {
        let directory = crate::secure_file::test_root().join(format!(
            "vacancyd-durable-{name}-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        // Private, whatever the umask: directories are created only under a
        // path no other account can change.
        let builder = std::fs::DirBuilder::new();
        #[cfg(unix)]
        let builder = {
            use std::os::unix::fs::DirBuilderExt;
            let mut builder = builder;
            builder.mode(0o700);
            builder
        };
        builder.create(&directory).unwrap();
        directory
    }

    fn staged_file(path: &Path, bytes: &[u8]) {
        let mut file = std::fs::File::create(path).unwrap();
        file.write_all(bytes).unwrap();
        file.sync_all().unwrap();
    }

    #[test]
    fn real_replacement_publishes_new_bytes_and_consumes_the_staged_file() {
        let directory = scratch("replace");
        let destination = directory.join("config.toml");
        let staged = directory.join(".config.toml.tmp");
        std::fs::write(&destination, b"old").unwrap();
        staged_file(&staged, b"new");
        replace(&staged, &destination).unwrap();
        assert_eq!(std::fs::read(&destination).unwrap(), b"new");
        assert!(!staged.exists());
        std::fs::remove_dir_all(directory).unwrap();
    }

    #[test]
    fn real_publication_never_clobbers_an_existing_file() {
        let directory = scratch("publish");
        let destination = directory.join("config.toml");
        let staged = directory.join(".config.toml.tmp");
        staged_file(&staged, b"first");
        publish_new(&staged, &destination).unwrap();
        assert_eq!(std::fs::read(&destination).unwrap(), b"first");
        assert!(!staged.exists());

        staged_file(&staged, b"second");
        match publish_new(&staged, &destination) {
            Err(ReplaceError::NotReplaced(error)) => {
                assert_eq!(error.kind(), io::ErrorKind::AlreadyExists, "{error}")
            }
            other => panic!("existing destination was not refused: {other:?}"),
        }
        assert_eq!(std::fs::read(&destination).unwrap(), b"first");
        std::fs::remove_dir_all(directory).unwrap();
    }

    #[cfg(unix)]
    #[test]
    fn write_never_follows_a_link_planted_at_a_staging_name() {
        let directory = scratch("planted");
        let victim = directory.join("victim");
        std::fs::write(&victim, b"keep").unwrap();
        let destination = directory.join("state.json");
        // The fixed staging name earlier versions opened with File::create.
        std::os::unix::fs::symlink(&victim, directory.join("state.json.tmp")).unwrap();

        write(&destination, b"new").unwrap();

        assert_eq!(std::fs::read(&victim).unwrap(), b"keep");
        assert_eq!(std::fs::read(&destination).unwrap(), b"new");
        std::fs::remove_dir_all(directory).unwrap();
    }

    #[cfg(unix)]
    #[test]
    fn a_written_file_is_private_to_the_service_account() {
        use std::os::unix::fs::PermissionsExt;
        let directory = scratch("private");
        let destination = directory.join("state.json");

        write(&destination, b"{}").unwrap();

        let mode = std::fs::metadata(&destination)
            .unwrap()
            .permissions()
            .mode();
        assert_eq!(mode & 0o777, 0o600);
        std::fs::remove_dir_all(directory).unwrap();
    }

    #[test]
    fn write_creates_missing_directories_and_leaves_no_staged_file() {
        let directory = scratch("write");
        let destination = directory.join("nested").join("deeper").join("state.json");
        write(&destination, b"{\"a\":1}").unwrap();
        write(&destination, b"{\"a\":2}").unwrap();
        assert_eq!(std::fs::read(&destination).unwrap(), b"{\"a\":2}");
        let names: Vec<_> = std::fs::read_dir(destination.parent().unwrap())
            .unwrap()
            .map(|entry| entry.unwrap().file_name())
            .collect();
        assert_eq!(names, [std::ffi::OsString::from("state.json")]);
        std::fs::remove_dir_all(directory).unwrap();
    }
}
