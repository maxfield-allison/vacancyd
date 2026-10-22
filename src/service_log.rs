//! Bounded service log. Foreground logging continues to use stderr.
use std::fs::{File, OpenOptions};
use std::io::{self, Read, Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};

pub const FILE_BYTES: u64 = 2 * 1024 * 1024;
pub const ARCHIVES: usize = 3;

/// Refuse a `vacancyd.log`/`vacancyd.log.N` path that already exists as a
/// reparse point or a multiply hard-linked file, so a copy planted before
/// the service's first start cannot redirect what SYSTEM opens and trims.
/// A path with nothing at it yet is left for the caller to create.
#[cfg(windows)]
fn refuse_planted(path: &Path) -> io::Result<()> {
    crate::windows_acl::refuse_planted(path)
}

#[cfg(not(windows))]
fn refuse_planted(_path: &Path) -> io::Result<()> {
    Ok(())
}

pub struct ServiceLog {
    path: PathBuf,
    file: Option<File>,
    size: u64,
    limit: u64,
    archives: usize,
    failure_reported: bool,
    /// Test hook: make renaming the live file fail, as a reader without
    /// delete sharing makes it fail on Windows.
    #[cfg(test)]
    refuse_live_rename: bool,
}

impl ServiceLog {
    pub fn open(path: &Path) -> io::Result<Self> {
        Self::with_limits(path, FILE_BYTES, ARCHIVES).inspect_err(report_failure)
    }

    fn with_limits(path: &Path, limit: u64, archives: usize) -> io::Result<Self> {
        if limit == 0 || limit > FILE_BYTES || archives > ARCHIVES {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "invalid log limits",
            ));
        }
        // A bare file name has no parent of its own, so the directory to
        // check comes from the absolute path, which every later trim, open
        // and rotation then uses as well.
        let path = std::path::absolute(path)?;
        let parent = path.parent().ok_or_else(|| {
            io::Error::new(
                io::ErrorKind::InvalidInput,
                "the service log path must include a file name",
            )
        })?;
        crate::secure_file::create_missing_private_dir(parent)?;
        let mut log = Self {
            path,
            file: None,
            size: 0,
            limit,
            archives,
            failure_reported: false,
            #[cfg(test)]
            refuse_live_rename: false,
        };
        // A previous release had an unbounded file. Retain its newest bytes,
        // with a bounded buffer, before accepting new output. This also bounds
        // archives left by an interrupted rotation or an older configuration.
        log.migrate_existing(refuse_planted)?;
        log.reopen()?;
        Ok(log)
    }

    /// Trim whichever of `vacancyd.log` and its numbered archives already
    /// exist, up front, at startup only. `guard` is checked before the
    /// existence test itself (not just before the trim), because a
    /// `Path::exists` on a dangling reparse point reports `false` and would
    /// otherwise let a planted link straight through to `reopen`'s
    /// create-if-missing open. A real, unlinked file that fails its guard
    /// stops startup rather than being adopted; see `refuse_planted`.
    fn migrate_existing(&self, guard: impl Fn(&Path) -> io::Result<()>) -> io::Result<()> {
        for index in 0..=self.archives {
            let name = self.filename(index);
            guard(&name)?;
            if name.exists() {
                trim_tail(&name, self.limit)?;
            }
        }
        Ok(())
    }

    fn filename(&self, index: usize) -> PathBuf {
        if index == 0 {
            return self.path.clone();
        }
        let mut name = self.path.as_os_str().to_owned();
        name.push(format!(".{index}"));
        PathBuf::from(name)
    }

    fn reopen(&mut self) -> io::Result<()> {
        let file = OpenOptions::new()
            .create(true)
            .append(true)
            .open(&self.path)?;
        self.size = file.metadata()?.len();
        self.file = Some(file);
        Ok(())
    }

    fn rotate(&mut self) -> io::Result<()> {
        if let Some(mut file) = self.file.take() {
            file.flush()?;
        }
        if self.archives == 0 {
            truncate(&self.path)?;
            return self.reopen();
        }
        // Close before rename: Windows does not permit renaming this handle.
        let oldest = self.filename(self.archives);
        match std::fs::remove_file(oldest) {
            Ok(()) => (),
            Err(error) if error.kind() == io::ErrorKind::NotFound => (),
            Err(error) => return Err(error),
        }
        for index in (1..self.archives).rev() {
            match std::fs::rename(self.filename(index), self.filename(index + 1)) {
                Ok(()) => (),
                Err(error) if error.kind() == io::ErrorKind::NotFound => (),
                Err(error) => return Err(error),
            }
        }
        self.archive_live_file()?;
        self.reopen()
    }

    /// Move the live file to archive 1. Renaming fails on Windows while any
    /// reader holds the file without delete sharing, which common tail tools
    /// do. Copying and then truncating needs only write sharing, which every
    /// reader of a file this process has open for writing already grants.
    fn archive_live_file(&mut self) -> io::Result<()> {
        let archive = self.filename(1);
        match self.rename_live(&archive) {
            Ok(()) => return Ok(()),
            Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(()),
            Err(_) => {}
        }
        std::fs::copy(&self.path, &archive)?;
        truncate(&self.path)
    }

    fn rename_live(&self, archive: &Path) -> io::Result<()> {
        #[cfg(test)]
        if self.refuse_live_rename {
            return Err(io::Error::new(
                io::ErrorKind::PermissionDenied,
                "test: live log held by a reader",
            ));
        }
        std::fs::rename(&self.path, archive)
    }

    fn write_bounded(&mut self, bytes: &[u8]) -> io::Result<usize> {
        if bytes.is_empty() {
            return Ok(0);
        }
        if self.file.is_none() {
            self.reopen()?;
        }
        if self.size >= self.limit {
            self.rotate()?;
        }
        let count = bytes.len().min((self.limit - self.size) as usize);
        let written = self
            .file
            .as_mut()
            .expect("open file")
            .write(&bytes[..count])?;
        self.size += written as u64;
        Ok(written)
    }
}

impl Write for ServiceLog {
    fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
        let result = self.write_bounded(bytes);
        match &result {
            Err(error) if !self.failure_reported => {
                report_failure(error);
                self.failure_reported = true;
            }
            Ok(_) => self.failure_reported = false,
            _ => (),
        }
        result
    }
    fn flush(&mut self) -> io::Result<()> {
        let result = match self.file.as_mut() {
            Some(file) => file.flush(),
            None => Ok(()),
        };
        if let Err(error) = &result {
            if !self.failure_reported {
                report_failure(error);
                self.failure_reported = true;
            }
        }
        result
    }
}

fn truncate(path: &Path) -> io::Result<()> {
    OpenOptions::new()
        .write(true)
        .create(true)
        .truncate(true)
        .open(path)
        .map(drop)
}

fn trim_tail(path: &Path, limit: u64) -> io::Result<()> {
    let mut file = OpenOptions::new().read(true).write(true).open(path)?;
    let length = file.metadata()?.len();
    if length > limit {
        file.seek(SeekFrom::Start(length - limit))?;
        let mut tail = vec![0; limit as usize];
        file.read_exact(&mut tail)?;
        file.seek(SeekFrom::Start(0))?;
        file.write_all(&tail)?;
        file.set_len(limit)?;
    }
    Ok(())
}

fn report_failure(error: &io::Error) {
    // Never call tracing from its own writer. Limit reporting to one event per
    // failure episode; a successful write permits a subsequent failure report.
    eprintln!("vacancyd service log unavailable: {error}");
    #[cfg(windows)]
    {
        use windows_sys::Win32::System::EventLog::{
            DeregisterEventSource, RegisterEventSourceW, ReportEventW, EVENTLOG_ERROR_TYPE,
        };
        let source: Vec<u16> = "vacancyd\0".encode_utf16().collect();
        let message: Vec<u16> = format!("Service log write/rotation failed: {error}\0")
            .encode_utf16()
            .collect();
        // SAFETY: NUL-terminated strings remain live for both synchronous calls;
        // the source handle is released once, and no binary data is supplied.
        unsafe {
            let handle = RegisterEventSourceW(std::ptr::null(), source.as_ptr());
            if !handle.is_null() {
                let strings = [message.as_ptr()];
                ReportEventW(
                    handle,
                    EVENTLOG_ERROR_TYPE,
                    0,
                    1,
                    std::ptr::null_mut(),
                    1,
                    0,
                    strings.as_ptr(),
                    std::ptr::null(),
                );
                DeregisterEventSource(handle);
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_debug_log_budget_matches_the_real_bound() {
        assert_eq!(
            FILE_BYTES * (ARCHIVES as u64 + 1),
            crate::log_control::SERVICE_LOG_TOTAL_BYTES
        );
    }
    use std::cell::RefCell;

    fn bare_log(dir: &Path, limit: u64, archives: usize) -> ServiceLog {
        ServiceLog {
            path: dir.join("service.log"),
            file: None,
            size: 0,
            limit,
            archives,
            failure_reported: false,
            refuse_live_rename: false,
        }
    }

    #[test]
    fn migrate_existing_guards_every_candidate_even_when_none_exist() {
        // A dangling reparse point reports `false` from `Path::exists`, so
        // the guard has to run before that check, not only before trimming,
        // or a planted link at an absent path would slip straight through.
        let dir = directory();
        let log = bare_log(&dir, 32, 2);
        let seen = RefCell::new(Vec::new());
        log.migrate_existing(|p: &Path| {
            seen.borrow_mut().push(p.to_path_buf());
            Ok(())
        })
        .unwrap();
        assert_eq!(
            seen.into_inner(),
            vec![
                dir.join("service.log"),
                dir.join("service.log.1"),
                dir.join("service.log.2"),
            ]
        );
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn migrate_existing_stops_at_the_first_refused_candidate() {
        let dir = directory();
        std::fs::write(dir.join("service.log.2"), b"0123456789").unwrap();
        let log = bare_log(&dir, 4, 2);
        let seen = RefCell::new(Vec::new());
        let err = log
            .migrate_existing(|p: &Path| {
                seen.borrow_mut().push(p.to_path_buf());
                if p.ends_with("service.log.1") {
                    Err(io::Error::new(io::ErrorKind::PermissionDenied, "planted"))
                } else {
                    Ok(())
                }
            })
            .unwrap_err();
        assert_eq!(err.kind(), io::ErrorKind::PermissionDenied);
        // The refused candidate stopped the walk before service.log.2 was
        // even checked, and before its oversized content was trimmed.
        assert_eq!(
            seen.into_inner(),
            vec![dir.join("service.log"), dir.join("service.log.1")]
        );
        assert_eq!(
            std::fs::read(dir.join("service.log.2")).unwrap(),
            b"0123456789"
        );
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn migrate_existing_trims_only_files_that_are_actually_there() {
        let dir = directory();
        std::fs::write(dir.join("service.log"), b"0123456789").unwrap();
        let log = bare_log(&dir, 4, 2);
        log.migrate_existing(|_: &Path| Ok(())).unwrap();
        assert_eq!(std::fs::read(dir.join("service.log")).unwrap(), b"6789");
        assert!(!dir.join("service.log.1").exists());
        assert!(!dir.join("service.log.2").exists());
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[cfg(windows)]
    #[test]
    fn a_log_outside_the_protected_folder_is_refused_before_anything_is_written() {
        let outside = crate::secure_file::protected_directory_outside_test_root();
        for path in [
            outside.join("vacancyd.log"),
            outside.join("logs").join("vacancyd.log"),
        ] {
            // `open` would also report the failure to the event log.
            let refused = ServiceLog::with_limits(&path, FILE_BYTES, ARCHIVES)
                .err()
                .unwrap();
            assert!(refused.to_string().contains("is not inside"), "{refused}");
        }
        assert_eq!(std::fs::read_dir(&outside).unwrap().count(), 0);
        std::fs::remove_dir(outside).unwrap();
    }

    /// A file name without a directory has no parent to check, so it used
    /// to be opened, trimmed and rotated wherever the process happened to
    /// run. The test process runs in the crate directory, outside the
    /// protected folder.
    #[cfg(windows)]
    #[test]
    fn a_bare_log_file_name_is_checked_where_it_resolves() {
        let name = format!("vacancyd-bare-{}.log", crate::setup::nonce().unwrap());
        let refused = ServiceLog::with_limits(Path::new(&name), FILE_BYTES, ARCHIVES)
            .err()
            .unwrap();
        assert!(refused.to_string().contains("is not inside"), "{refused}");
        assert!(!Path::new(&name).exists());
    }

    #[test]
    fn refuse_planted_permits_ordinary_and_absent_paths() {
        // Off Windows this exercises the `cfg(not(windows))` no-op; on
        // Windows it exercises the real check, which must still pass an
        // ordinary single-link file and a path with nothing at it yet.
        let dir = directory();
        std::fs::write(dir.join("service.log"), b"hi").unwrap();
        refuse_planted(&dir.join("service.log")).unwrap();
        refuse_planted(&dir.join("does-not-exist")).unwrap();
        std::fs::remove_dir_all(dir).unwrap();
    }

    /// On Windows the service log must be inside the protected folder,
    /// which tests stand in for with their own.
    fn directory() -> PathBuf {
        let p = crate::secure_file::test_root()
            .join(format!("vacancyd-log-{}", crate::setup::nonce().unwrap()));
        std::fs::create_dir(&p).unwrap();
        p
    }
    #[test]
    fn production_defaults_open_and_empty_writes_do_not_rotate() {
        let dir = directory();
        let path = dir.join("service.log");
        let mut log = ServiceLog::open(&path).unwrap();
        assert_eq!(log.limit, FILE_BYTES);
        assert_eq!(log.archives, ARCHIVES);
        assert_eq!(log.write(b"").unwrap(), 0);
        drop(log);
        assert_eq!(std::fs::read_dir(&dir).unwrap().count(), 1);
        std::fs::remove_dir_all(dir).unwrap();
    }
    #[test]
    fn continuous_output_and_restart_remain_bounded() {
        let dir = directory();
        let path = dir.join("service.log");
        let bytes = (0..=255u8).cycle().take(1000).collect::<Vec<_>>();
        let mut log = ServiceLog::with_limits(&path, 32, 3).unwrap();
        log.write_all(&bytes[..600]).unwrap();
        drop(log);
        let mut log = ServiceLog::with_limits(&path, 32, 3).unwrap();
        log.write_all(&bytes[600..]).unwrap();
        log.flush().unwrap();
        drop(log);
        let mut retained = Vec::new();
        let mut total = 0;
        for index in (0..=3).rev() {
            let file = if index == 0 {
                path.clone()
            } else {
                dir.join(format!("service.log.{index}"))
            };
            let data = std::fs::read(file).unwrap();
            assert!(data.len() <= 32);
            total += data.len();
            retained.extend(data);
        }
        assert!(total <= 128);
        assert!(bytes.ends_with(&retained));
        assert_eq!(std::fs::read_dir(&dir).unwrap().count(), 4);
        std::fs::remove_dir_all(dir).unwrap();
    }
    #[test]
    fn migration_keeps_only_the_tail_of_unbounded_files() {
        let dir = directory();
        let path = dir.join("service.log");
        std::fs::write(&path, b"old bytes and newest").unwrap();
        let log = ServiceLog::with_limits(&path, 6, 3).unwrap();
        drop(log);
        assert_eq!(std::fs::read(&path).unwrap(), b"newest");
        std::fs::remove_dir_all(dir).unwrap();
    }
    #[test]
    fn rotation_continues_while_a_reader_blocks_renaming_the_live_file() {
        let dir = directory();
        let path = dir.join("service.log");
        let mut log = ServiceLog::with_limits(&path, 8, 2).unwrap();
        log.refuse_live_rename = true;
        log.write_all(b"first...").unwrap();
        log.write_all(b"second..").unwrap();
        log.write_all(b"third").unwrap();
        drop(log);
        assert_eq!(std::fs::read(&path).unwrap(), b"third");
        assert_eq!(
            std::fs::read(dir.join("service.log.1")).unwrap(),
            b"second.."
        );
        assert_eq!(
            std::fs::read(dir.join("service.log.2")).unwrap(),
            b"first..."
        );
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn failed_rotation_never_appends_past_the_bound_and_recovers() {
        let dir = directory();
        let path = dir.join("service.log");
        let mut log = ServiceLog::with_limits(&path, 8, 1).unwrap();
        log.write_all(b"12345678").unwrap();
        std::fs::create_dir(dir.join("service.log.1")).unwrap();
        assert!(log.write_all(b"must not append").is_err());
        assert_eq!(std::fs::metadata(&path).unwrap().len(), 8);
        assert!(log.failure_reported);
        std::fs::remove_dir(dir.join("service.log.1")).unwrap();
        log.write_all(b"recovery").unwrap();
        assert!(!log.failure_reported);
        drop(log);
        assert_eq!(std::fs::read(&path).unwrap(), b"recovery");
        std::fs::remove_dir_all(dir).unwrap();
    }
}
