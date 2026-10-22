//! Run probe subprocesses with a hard wall-clock deadline.
//!
//! [`std::process::Command::output`] can wait forever, and so can draining a
//! child's pipes: any process the child starts inherits the pipes' write
//! ends and can hold them open long after the child itself has exited. These
//! programs sit on the daemon's measurement path, so a wedged vendor tool
//! must be killed and reported as an unreadable sample rather than pinning
//! the collector.
//!
//! The deadline therefore covers the whole call, the child exiting *and* its
//! output reaching end-of-file, and the call owns every process the child
//! starts. It kills them all before returning, after success as well as
//! failure:
//!
//! - On Unix the child leads a new process group. The group is killed while
//!   the child is still unreaped, so its ID cannot yet have been reused.
//! - On Windows the child starts suspended and joins a job object that kills
//!   every process in it when closed, before it can run and start anything.
//!
//! Output is read with the same deadline and capped, so a descendant that
//! escapes the group (a Unix `setsid`) can outlive the call but not delay
//! it: its pipes are closed when the call returns.

use std::io;
use std::path::{Path, PathBuf};
#[cfg(windows)]
use std::process::ExitStatus;
use std::process::{Command, Output, Stdio};
use std::time::{Duration, Instant};

/// Output beyond this on either stream fails the call. Probe output is a few
/// kilobytes; an unbounded buffer would be a memory hazard.
const OUTPUT_LIMIT: usize = 1024 * 1024;

/// After terminating the process tree, how long the call waits for its pipes
/// to close and the child to be reaped before returning regardless.
const TERMINATION_GRACE: Duration = Duration::from_millis(250);

/// How often the child's exit is checked while its pipes are quiet.
const POLL_INTERVAL: Duration = Duration::from_millis(10);

pub fn output(command: &mut Command, timeout: Duration) -> io::Result<Output> {
    run(command, timeout, OUTPUT_LIMIT)
}

fn timed_out(timeout: Duration) -> io::Error {
    io::Error::new(
        io::ErrorKind::TimedOut,
        format!("subprocess exceeded {} ms", timeout.as_millis()),
    )
}

fn output_too_large(limit: usize) -> io::Error {
    io::Error::new(
        io::ErrorKind::InvalidData,
        format!("subprocess output exceeded {limit} bytes"),
    )
}

#[cfg(unix)]
fn run(command: &mut Command, timeout: Duration, limit: usize) -> io::Result<Output> {
    use std::os::unix::process::CommandExt;

    command
        .process_group(0)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    let deadline = Instant::now() + timeout;
    let mut child = command.spawn()?;
    let stdout = child.stdout.take();
    let stderr = child.stderr.take();
    let group = unix::Group::new(child);
    let (Some(stdout), Some(stderr)) = (stdout, stderr) else {
        return Err(io::Error::other("child output was not piped"));
    };
    let mut pipes = [
        unix::Pipe::new(stdout.into())?,
        unix::Pipe::new(stderr.into())?,
    ];

    let mut exited = false;
    let finished: io::Result<()> = loop {
        if !exited {
            match group.leader_exited() {
                Ok(done) => exited = done,
                Err(error) => break Err(error),
            }
        }
        if exited && pipes.iter().all(unix::Pipe::is_closed) {
            break Ok(());
        }
        let now = Instant::now();
        if now >= deadline {
            break Err(timed_out(timeout));
        }
        let mut wait = deadline - now;
        if !exited {
            wait = wait.min(POLL_INTERVAL);
        }
        if let Err(error) = unix::read_ready(&mut pipes, wait, limit) {
            break Err(error);
        }
    };
    match finished {
        Ok(()) => {
            let status = group.finish()?;
            let [stdout, stderr] = pipes.map(unix::Pipe::into_bytes);
            Ok(Output {
                status,
                stdout,
                stderr,
            })
        }
        Err(error) => {
            // Close the pipes first, so nothing is left blocked writing into
            // them, then kill and reap the group.
            drop(pipes);
            drop(group);
            Err(error)
        }
    }
}

#[cfg(unix)]
mod unix {
    use super::{output_too_large, TERMINATION_GRACE};
    use std::fs::File;
    use std::io::{self, Read};
    use std::os::fd::{AsRawFd, OwnedFd};
    use std::process::{Child, ExitStatus};
    use std::time::{Duration, Instant};

    /// The child and the process group it leads. Dropping it kills the group
    /// and reaps the child.
    pub struct Group {
        child: Option<Child>,
    }

    impl Group {
        pub fn new(child: Child) -> Self {
            Self { child: Some(child) }
        }

        fn pid(&self) -> libc::pid_t {
            self.child
                .as_ref()
                .map_or(0, |child| child.id() as libc::pid_t)
        }

        /// Whether the child has exited, without reaping it: an unreaped
        /// child's ID still names its process group.
        pub fn leader_exited(&self) -> io::Result<bool> {
            // SAFETY: an all-zero siginfo_t is valid, and a zeroed si_signo
            // is how an empty WNOHANG result is recognized.
            let mut info: libc::siginfo_t = unsafe { std::mem::zeroed() };
            loop {
                // SAFETY: `info` is a valid out-parameter; WNOWAIT leaves the
                // child waitable for Child::wait.
                let result = unsafe {
                    libc::waitid(
                        libc::P_PID,
                        self.pid() as libc::id_t,
                        &mut info,
                        libc::WEXITED | libc::WNOHANG | libc::WNOWAIT,
                    )
                };
                if result == 0 {
                    return Ok(info.si_signo != 0);
                }
                let error = io::Error::last_os_error();
                if error.kind() != io::ErrorKind::Interrupted {
                    return Err(error);
                }
            }
        }

        fn kill_group(&self) {
            let pid = self.pid();
            if pid > 0 {
                // SAFETY: the leader is unreaped, so `pid` still names the
                // group this call created. A missing group is harmless.
                unsafe { libc::kill(-pid, libc::SIGKILL) };
            }
        }

        /// Kill whatever the exited child left running, then reap it.
        pub fn finish(mut self) -> io::Result<ExitStatus> {
            self.kill_group();
            let mut child = self.child.take().expect("the child is reaped once");
            child.wait()
        }
    }

    impl Drop for Group {
        fn drop(&mut self) {
            self.kill_group();
            let Some(mut child) = self.child.take() else {
                return;
            };
            let started = Instant::now();
            while started.elapsed() < TERMINATION_GRACE {
                if !matches!(child.try_wait(), Ok(None)) {
                    return;
                }
                std::thread::sleep(Duration::from_millis(5));
            }
            // SIGKILL cannot interrupt uninterruptible sleep in a driver call.
            // Reap the child when it finally dies rather than wait for it here.
            std::thread::spawn(move || {
                let _ = child.wait();
            });
        }
    }

    /// One non-blocking pipe and what has been read from it.
    pub struct Pipe {
        file: Option<File>,
        bytes: Vec<u8>,
    }

    impl Pipe {
        pub fn new(fd: OwnedFd) -> io::Result<Self> {
            // SAFETY: fcntl on a descriptor this function owns. O_NONBLOCK
            // is a flag of this open file description, the read end, and
            // does not change the child's write end.
            unsafe {
                let flags = libc::fcntl(fd.as_raw_fd(), libc::F_GETFL);
                if flags < 0
                    || libc::fcntl(fd.as_raw_fd(), libc::F_SETFL, flags | libc::O_NONBLOCK) < 0
                {
                    return Err(io::Error::last_os_error());
                }
            }
            Ok(Self {
                file: Some(File::from(fd)),
                bytes: Vec::new(),
            })
        }

        pub fn is_closed(&self) -> bool {
            self.file.is_none()
        }

        pub fn into_bytes(self) -> Vec<u8> {
            self.bytes
        }

        /// Read everything available now, closing the pipe at end-of-file.
        fn drain(&mut self, limit: usize) -> io::Result<()> {
            let Some(file) = &mut self.file else {
                return Ok(());
            };
            let mut buffer = [0u8; 8192];
            loop {
                match file.read(&mut buffer) {
                    Ok(0) => {
                        self.file = None;
                        return Ok(());
                    }
                    Ok(read) => {
                        if self.bytes.len() + read > limit {
                            return Err(output_too_large(limit));
                        }
                        self.bytes.extend_from_slice(&buffer[..read]);
                    }
                    Err(error) if error.kind() == io::ErrorKind::WouldBlock => return Ok(()),
                    Err(error) if error.kind() == io::ErrorKind::Interrupted => {}
                    Err(error) => return Err(error),
                }
            }
        }
    }

    /// Wait up to `wait` for any open pipe to become readable, then read
    /// what is there.
    pub fn read_ready(pipes: &mut [Pipe], wait: Duration, limit: usize) -> io::Result<()> {
        let mut polled: Vec<(usize, libc::pollfd)> = pipes
            .iter()
            .enumerate()
            .filter_map(|(index, pipe)| {
                pipe.file.as_ref().map(|file| {
                    (
                        index,
                        libc::pollfd {
                            fd: file.as_raw_fd(),
                            events: libc::POLLIN,
                            revents: 0,
                        },
                    )
                })
            })
            .collect();
        if polled.is_empty() {
            std::thread::sleep(wait);
            return Ok(());
        }
        // Round up, so a sub-millisecond remainder still waits.
        let milliseconds = wait.as_micros().div_ceil(1000).min(i32::MAX as u128) as libc::c_int;
        let mut fds: Vec<libc::pollfd> = polled.iter().map(|(_, fd)| *fd).collect();
        // SAFETY: `fds` is a valid array of `fds.len()` pollfd entries.
        let ready =
            unsafe { libc::poll(fds.as_mut_ptr(), fds.len() as libc::nfds_t, milliseconds) };
        if ready < 0 {
            let error = io::Error::last_os_error();
            return if error.kind() == io::ErrorKind::Interrupted {
                Ok(())
            } else {
                Err(error)
            };
        }
        for ((index, entry), fd) in polled.iter_mut().zip(&fds) {
            entry.revents = fd.revents;
            if entry.revents != 0 {
                pipes[*index].drain(limit)?;
            }
        }
        Ok(())
    }
}

#[cfg(windows)]
fn run(command: &mut Command, timeout: Duration, limit: usize) -> io::Result<Output> {
    use std::os::windows::process::CommandExt;
    use std::sync::mpsc::{self, RecvTimeoutError};

    // Probe tools such as powercfg and nvidia-smi are console applications.
    // Without CREATE_NO_WINDOW, a foreground/debug run flashes a console
    // window on every sample. A native service hides the symptom in session 0
    // but does not make the launch behavior correct. CREATE_SUSPENDED keeps
    // the child from starting anything until it is inside the job.
    command
        .creation_flags(windows::CREATE_NO_WINDOW | windows::CREATE_SUSPENDED)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    let job = windows::Job::new()?;
    let deadline = Instant::now() + timeout;
    let mut child = command.spawn()?;
    if let Err(error) = job.assign(&child).and_then(|()| windows::resume(&child)) {
        job.terminate();
        let _ = child.kill();
        return Err(error);
    }
    let (Some(stdout), Some(stderr)) = (child.stdout.take(), child.stderr.take()) else {
        job.terminate();
        return Err(io::Error::other("child output was not piped"));
    };
    let (sender, received) = mpsc::channel();
    let readers = [
        windows::spawn_reader(0, stdout, limit, sender.clone())?,
        windows::spawn_reader(1, stderr, limit, sender)?,
    ];

    let mut outputs: [Option<Vec<u8>>; 2] = [None, None];
    let mut status: Option<ExitStatus> = None;
    let finished: io::Result<ExitStatus> = loop {
        if status.is_none() {
            match child.try_wait() {
                Ok(exited) => status = exited,
                Err(error) => break Err(error),
            }
        }
        if let (Some(status), true) = (status, outputs.iter().all(Option::is_some)) {
            break Ok(status);
        }
        let now = Instant::now();
        if now >= deadline {
            break Err(timed_out(timeout));
        }
        let wait = (deadline - now).min(POLL_INTERVAL);
        match received.recv_timeout(wait) {
            Ok((index, Ok(bytes))) => outputs[index] = Some(bytes),
            Ok((_, Err(error))) => break Err(error),
            Err(RecvTimeoutError::Timeout) => {}
            // Both readers have reported; only the child's exit is left.
            Err(RecvTimeoutError::Disconnected) => std::thread::sleep(wait),
        }
    };
    match finished {
        Ok(status) => {
            // Both pipes reached end-of-file, so both readers are done.
            for reader in readers {
                let _ = reader.join();
            }
            // Closing the job kills anything the child left running.
            drop(job);
            let [stdout, stderr] = outputs.map(Option::unwrap_or_default);
            Ok(Output {
                status,
                stdout,
                stderr,
            })
        }
        Err(error) => {
            job.terminate();
            windows::stop_readers(readers);
            Err(error)
        }
    }
}

#[cfg(windows)]
mod windows {
    use super::{output_too_large, TERMINATION_GRACE};
    use std::io::{self, Read};
    use std::os::windows::io::AsRawHandle;
    use std::process::Child;
    use std::sync::mpsc::Sender;
    use std::thread::JoinHandle;
    use std::time::{Duration, Instant};
    use windows_sys::Win32::Foundation::{CloseHandle, HANDLE, INVALID_HANDLE_VALUE};

    pub const CREATE_NO_WINDOW: u32 = 0x0800_0000;
    pub const CREATE_SUSPENDED: u32 = 0x0000_0004;

    /// A job object that kills every process in it when its handle closes.
    pub struct Job(HANDLE);

    impl Job {
        pub fn new() -> io::Result<Self> {
            use windows_sys::Win32::System::JobObjects::{
                CreateJobObjectW, JobObjectExtendedLimitInformation, SetInformationJobObject,
                JOBOBJECT_EXTENDED_LIMIT_INFORMATION, JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE,
            };
            // SAFETY: default security and no name are both valid.
            let handle = unsafe { CreateJobObjectW(std::ptr::null(), std::ptr::null()) };
            if handle.is_null() {
                return Err(io::Error::last_os_error());
            }
            let job = Self(handle);
            // SAFETY: an all-zero limit structure is valid; only the flag
            // below is set. Breakaway is not permitted, so the child's
            // descendants stay in the job.
            let mut limits: JOBOBJECT_EXTENDED_LIMIT_INFORMATION = unsafe { std::mem::zeroed() };
            limits.BasicLimitInformation.LimitFlags = JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE;
            // SAFETY: `limits` is the structure this information class
            // names, with its exact size.
            let set = unsafe {
                SetInformationJobObject(
                    job.0,
                    JobObjectExtendedLimitInformation,
                    std::ptr::from_ref(&limits).cast(),
                    std::mem::size_of::<JOBOBJECT_EXTENDED_LIMIT_INFORMATION>() as u32,
                )
            };
            if set == 0 {
                return Err(io::Error::last_os_error());
            }
            Ok(job)
        }

        pub fn assign(&self, child: &Child) -> io::Result<()> {
            use windows_sys::Win32::System::JobObjects::AssignProcessToJobObject;
            // SAFETY: both handles are live for the call.
            if unsafe { AssignProcessToJobObject(self.0, child.as_raw_handle() as HANDLE) } == 0 {
                return Err(io::Error::last_os_error());
            }
            Ok(())
        }

        /// Kill every process in the job now.
        pub fn terminate(&self) {
            use windows_sys::Win32::System::JobObjects::TerminateJobObject;
            // SAFETY: a live job handle. Failure leaves KILL_ON_JOB_CLOSE,
            // which still applies when the handle is dropped.
            unsafe { TerminateJobObject(self.0, 1) };
        }
    }

    impl Drop for Job {
        fn drop(&mut self) {
            // SAFETY: this struct owns the handle and closes it once.
            unsafe { CloseHandle(self.0) };
        }
    }

    struct OwnedHandle(HANDLE);

    impl Drop for OwnedHandle {
        fn drop(&mut self) {
            // SAFETY: the wrapped handle is owned and closed once.
            unsafe { CloseHandle(self.0) };
        }
    }

    /// Resume the only thread of the child, created suspended.
    pub fn resume(child: &Child) -> io::Result<()> {
        use windows_sys::Win32::System::Diagnostics::ToolHelp::{
            CreateToolhelp32Snapshot, Thread32First, Thread32Next, TH32CS_SNAPTHREAD, THREADENTRY32,
        };
        use windows_sys::Win32::System::Threading::{
            OpenThread, ResumeThread, THREAD_SUSPEND_RESUME,
        };

        let pid = child.id();
        // SAFETY: a system-wide thread snapshot takes no process handle.
        let snapshot = unsafe { CreateToolhelp32Snapshot(TH32CS_SNAPTHREAD, 0) };
        if snapshot == INVALID_HANDLE_VALUE {
            return Err(io::Error::last_os_error());
        }
        let snapshot = OwnedHandle(snapshot);
        // SAFETY: an all-zero entry is valid once its size is set.
        let mut entry: THREADENTRY32 = unsafe { std::mem::zeroed() };
        entry.dwSize = std::mem::size_of::<THREADENTRY32>() as u32;
        let mut resumed = 0;
        // SAFETY: a live snapshot and a sized entry.
        let mut more = unsafe { Thread32First(snapshot.0, &mut entry) } != 0;
        while more {
            if entry.th32OwnerProcessID == pid {
                // SAFETY: opening a thread by ID has no preconditions.
                let thread = unsafe { OpenThread(THREAD_SUSPEND_RESUME, 0, entry.th32ThreadID) };
                if thread.is_null() {
                    return Err(io::Error::last_os_error());
                }
                let thread = OwnedHandle(thread);
                // SAFETY: a live thread handle with suspend/resume access.
                if unsafe { ResumeThread(thread.0) } == u32::MAX {
                    return Err(io::Error::last_os_error());
                }
                resumed += 1;
            }
            // SAFETY: as for Thread32First.
            more = unsafe { Thread32Next(snapshot.0, &mut entry) } != 0;
        }
        if resumed == 0 {
            return Err(io::Error::other(
                "the suspended subprocess has no thread to resume",
            ));
        }
        Ok(())
    }

    type Report = (usize, io::Result<Vec<u8>>);

    /// Read `pipe` to end-of-file on its own thread, reporting at most
    /// `limit` bytes, or the overflow, as `index`.
    pub fn spawn_reader(
        index: usize,
        pipe: impl Read + Send + 'static,
        limit: usize,
        report: Sender<Report>,
    ) -> io::Result<JoinHandle<()>> {
        std::thread::Builder::new()
            .name("subprocess-output".into())
            .spawn(move || {
                let mut bytes = Vec::new();
                let result = pipe
                    .take(limit as u64 + 1)
                    .read_to_end(&mut bytes)
                    .and_then(|_| {
                        if bytes.len() > limit {
                            Err(output_too_large(limit))
                        } else {
                            Ok(bytes)
                        }
                    });
                let _ = report.send((index, result));
            })
    }

    /// After the job is terminated, the pipes' writers are gone and the
    /// readers finish at end-of-file. A writer outside the job cannot hold a
    /// reader past the grace period: its blocking read is cancelled, and a
    /// reader that still has not finished is left to finish on its own.
    pub fn stop_readers(readers: [JoinHandle<()>; 2]) {
        use windows_sys::Win32::System::IO::CancelSynchronousIo;
        let started = Instant::now();
        while readers.iter().any(|reader| !reader.is_finished())
            && started.elapsed() < TERMINATION_GRACE
        {
            std::thread::sleep(Duration::from_millis(5));
            if started.elapsed() >= TERMINATION_GRACE / 2 {
                for reader in readers.iter().filter(|reader| !reader.is_finished()) {
                    // SAFETY: the thread handle stays valid while its
                    // JoinHandle exists; cancelling a thread with no pending
                    // synchronous I/O fails harmlessly.
                    unsafe { CancelSynchronousIo(reader.as_raw_handle() as HANDLE) };
                }
            }
        }
    }
}

/// Absolute path of a program that ships with Windows, such as `powercfg.exe`.
///
/// The service runs as LocalSystem. A bare program name makes Windows search
/// the application directory, the system directories and then every PATH
/// entry, and a user-writable PATH directory would let someone plant a
/// program that runs as SYSTEM. Tools are therefore named by full path from
/// the system directory the kernel reports, never searched for.
#[cfg(windows)]
pub fn system_program(file_name: &str) -> io::Result<PathBuf> {
    Ok(system_directory()?.join(file_name))
}

#[cfg(windows)]
fn system_directory() -> io::Result<PathBuf> {
    use std::os::windows::ffi::OsStringExt;
    use windows_sys::Win32::System::SystemInformation::GetSystemDirectoryW;

    let mut buffer = vec![0u16; 1024];
    // SAFETY: the buffer is valid for its stated length in UTF-16 units.
    let length = unsafe { GetSystemDirectoryW(buffer.as_mut_ptr(), buffer.len() as u32) } as usize;
    if length == 0 {
        return Err(io::Error::last_os_error());
    }
    if length >= buffer.len() {
        return Err(io::Error::other("system directory path is too long"));
    }
    Ok(PathBuf::from(std::ffi::OsString::from_wide(
        &buffer[..length],
    )))
}

/// The `nvidia-smi` to run. On Windows it is looked for only where the NVIDIA
/// driver installs it: the system directory (current drivers) and
/// `NVIDIA Corporation\NVSMI` under Program Files (older drivers). PATH is
/// never searched; see [`system_program`]. Elsewhere the name is resolved as
/// before.
#[cfg(windows)]
pub fn nvidia_smi() -> io::Result<PathBuf> {
    let program_files = ["ProgramW6432", "ProgramFiles"]
        .into_iter()
        .filter_map(std::env::var_os)
        .map(PathBuf::from);
    nvidia_smi_candidates(&system_directory()?, program_files)
        .into_iter()
        .find(|candidate| candidate.is_file())
        .ok_or_else(|| {
            io::Error::new(
                io::ErrorKind::NotFound,
                "nvidia-smi.exe is not in the system directory or NVIDIA's NVSMI directory",
            )
        })
}

#[cfg(not(windows))]
pub fn nvidia_smi() -> io::Result<PathBuf> {
    Ok(PathBuf::from("nvidia-smi"))
}

/// Where the NVIDIA driver installs `nvidia-smi.exe`, in search order.
#[cfg_attr(not(any(windows, test)), allow(dead_code))]
fn nvidia_smi_candidates(
    system_directory: &Path,
    program_files: impl IntoIterator<Item = PathBuf>,
) -> Vec<PathBuf> {
    std::iter::once(system_directory.join("nvidia-smi.exe"))
        .chain(program_files.into_iter().map(|directory| {
            directory
                .join("NVIDIA Corporation")
                .join("NVSMI")
                .join("nvidia-smi.exe")
        }))
        .filter(|candidate| candidate.is_absolute())
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn nvidia_smi_is_looked_for_only_in_absolute_driver_locations() {
        let root = PathBuf::from(if cfg!(windows) { r"C:\" } else { "/" });
        let candidates = nvidia_smi_candidates(
            &root.join("Windows").join("System32"),
            [
                root.join("Program Files"),
                // A relative Program Files value must never become a search.
                PathBuf::from("relative"),
            ],
        );
        assert_eq!(
            candidates,
            [
                root.join("Windows").join("System32").join("nvidia-smi.exe"),
                root.join("Program Files")
                    .join("NVIDIA Corporation")
                    .join("NVSMI")
                    .join("nvidia-smi.exe"),
            ]
        );
    }

    #[cfg(windows)]
    #[test]
    fn windows_tools_resolve_to_absolute_system_paths() {
        let path = system_program("powercfg.exe").unwrap();
        assert!(path.is_absolute(), "{}", path.display());
        assert!(path.is_file(), "{}", path.display());
        if let Ok(path) = nvidia_smi() {
            assert!(path.is_absolute());
        }
    }

    #[test]
    fn captures_output_and_exit_status() {
        let mut command = echo_command();
        let output = super::output(&mut command, Duration::from_secs(5)).unwrap();
        assert!(output.status.success());
        assert!(String::from_utf8_lossy(&output.stdout).contains("vacancyd"));
    }

    #[test]
    fn kills_a_child_that_exceeds_its_deadline() {
        let mut command = sleep_command();
        let started = Instant::now();
        let error = super::output(&mut command, Duration::from_millis(100)).unwrap_err();
        assert_eq!(error.kind(), io::ErrorKind::TimedOut);
        assert!(started.elapsed() < Duration::from_secs(2));
    }

    #[test]
    fn output_beyond_the_limit_fails_instead_of_growing() {
        let mut command = echo_command();
        let started = Instant::now();
        let error = run(&mut command, Duration::from_secs(5), 3).unwrap_err();
        assert_eq!(error.kind(), io::ErrorKind::InvalidData, "{error}");
        assert!(started.elapsed() < Duration::from_secs(5));
        // Within the limit the output is complete.
        let mut command = echo_command();
        let output = run(&mut command, Duration::from_secs(5), 64).unwrap();
        assert!(output.stdout.len() <= 64);
    }

    // The process-tree tests below run this test binary as the child. The
    // child starts a grandchild, which inherits the child's stdout and
    // stderr (the pipes being read) and records its process ID.

    /// Environment variable selecting what a re-run of this binary does.
    const ROLE: &str = "VACANCYD_CHILD_PROCESS_TEST_ROLE";
    /// Where a grandchild records its process ID.
    const PID_FILE: &str = "VACANCYD_CHILD_PROCESS_TEST_PID_FILE";

    /// Not a test: the process body for the tests below. It does nothing
    /// unless one of them selected a role.
    #[test]
    #[ignore = "helper process for the process-tree tests"]
    fn process_tree_helper() {
        let Ok(role) = std::env::var(ROLE) else {
            return;
        };
        // Start a grandchild and wait until it has recorded its ID, so the
        // tests can always find it.
        let grandchild = |role: &str, holds_output: bool| {
            let mut command = helper_command(role);
            if !holds_output {
                command.stdout(Stdio::null()).stderr(Stdio::null());
                // Windows passes every inheritable handle to a child, not only
                // its standard ones: keep the pipes out of this grandchild.
                #[cfg(windows)]
                {
                    use std::os::windows::io::AsRawHandle;
                    use windows_sys::Win32::Foundation::{
                        SetHandleInformation, HANDLE_FLAG_INHERIT,
                    };
                    for handle in [
                        std::io::stdout().as_raw_handle(),
                        std::io::stderr().as_raw_handle(),
                    ] {
                        // SAFETY: this process's own standard handles.
                        assert_ne!(
                            unsafe { SetHandleInformation(handle as _, HANDLE_FLAG_INHERIT, 0) },
                            0
                        );
                    }
                }
            }
            // The grandchild is meant to outlive this helper, which exits
            // without waiting for it; the call under test must kill it.
            #[allow(clippy::zombie_processes)]
            let _grandchild = command.spawn().unwrap();
            let pid_file = std::env::var(PID_FILE).unwrap();
            while !Path::new(&pid_file).exists() {
                std::thread::sleep(Duration::from_millis(10));
            }
        };
        match role.as_str() {
            // Exit at once, leaving a grandchild that holds the pipes open.
            "leaves-pipe-holder" => grandchild("grandchild", true),
            // Keep running, and start a grandchild that holds the pipes.
            "hangs-with-pipe-holder" => {
                grandchild("grandchild", true);
                std::thread::sleep(Duration::from_secs(60));
            }
            // Exit at once, leaving a grandchild that does not hold them.
            "leaves-stray" => grandchild("grandchild", false),
            #[cfg(unix)]
            "leaves-escaped-pipe-holder" => grandchild("escaped-grandchild", true),
            "grandchild" | "escaped-grandchild" => {
                #[cfg(unix)]
                if role == "escaped-grandchild" {
                    // SAFETY: setsid has no preconditions.
                    assert!(unsafe { libc::setsid() } > 0);
                }
                let path = std::env::var(PID_FILE).unwrap();
                std::fs::write(format!("{path}.partial"), std::process::id().to_string()).unwrap();
                std::fs::rename(format!("{path}.partial"), path).unwrap();
                std::thread::sleep(Duration::from_secs(60));
            }
            other => panic!("unknown helper role {other}"),
        }
    }

    fn helper_command(role: &str) -> Command {
        let mut command = Command::new(std::env::current_exe().unwrap());
        command
            .args([
                "--exact",
                "child_process::tests::process_tree_helper",
                "--ignored",
                "--test-threads=1",
            ])
            .env(ROLE, role);
        command
    }

    struct Tree {
        pid_file: PathBuf,
    }

    impl Tree {
        fn new(name: &str) -> Self {
            let pid_file = std::env::temp_dir().join(format!(
                "vacancyd-child-process-{name}-{}-{}.pid",
                std::process::id(),
                std::time::SystemTime::now()
                    .duration_since(std::time::UNIX_EPOCH)
                    .unwrap()
                    .as_nanos()
            ));
            Self { pid_file }
        }

        fn command(&self, role: &str) -> Command {
            let mut command = helper_command(role);
            command.env(PID_FILE, &self.pid_file);
            command
        }

        /// The grandchild's process ID. The child waits for it to be
        /// recorded before going on, so it exists once the call returns.
        fn grandchild(&self) -> u32 {
            std::fs::read_to_string(&self.pid_file)
                .expect("the grandchild started within the deadline")
                .parse()
                .unwrap()
        }

        fn assert_grandchild_killed(&self) {
            use sysinfo::{Pid, ProcessStatus, ProcessesToUpdate, System};
            let pid = Pid::from_u32(self.grandchild());
            let started = Instant::now();
            let mut system = System::new();
            loop {
                system.refresh_processes(ProcessesToUpdate::All, true);
                // A killed process that no one has reaped yet is a zombie.
                if system
                    .process(pid)
                    .is_none_or(|process| process.status() == ProcessStatus::Zombie)
                {
                    return;
                }
                assert!(
                    started.elapsed() < Duration::from_secs(5),
                    "grandchild {pid} outlived the call"
                );
                std::thread::sleep(Duration::from_millis(50));
            }
        }
    }

    impl Drop for Tree {
        fn drop(&mut self) {
            let _ = std::fs::remove_file(&self.pid_file);
        }
    }

    /// A child that exits successfully but leaves a descendant holding its
    /// output open used to turn a missed deadline into a success.
    #[test]
    fn a_descendant_holding_the_pipes_cannot_extend_the_deadline() {
        let tree = Tree::new("holder");
        let deadline = Duration::from_secs(5);
        let started = Instant::now();
        let error = output(&mut tree.command("leaves-pipe-holder"), deadline).unwrap_err();
        let elapsed = started.elapsed();
        assert_eq!(error.kind(), io::ErrorKind::TimedOut, "{error}");
        assert!(
            elapsed < deadline + Duration::from_secs(1),
            "took {elapsed:?}"
        );
        tree.assert_grandchild_killed();
    }

    #[test]
    fn a_timed_out_child_and_its_pipe_holding_descendant_are_both_killed() {
        let tree = Tree::new("hang");
        let deadline = Duration::from_secs(5);
        let started = Instant::now();
        let error = output(&mut tree.command("hangs-with-pipe-holder"), deadline).unwrap_err();
        let elapsed = started.elapsed();
        assert_eq!(error.kind(), io::ErrorKind::TimedOut, "{error}");
        assert!(
            elapsed < deadline + Duration::from_secs(1),
            "took {elapsed:?}"
        );
        tree.assert_grandchild_killed();
    }

    #[test]
    fn a_successful_call_kills_what_the_child_left_running() {
        let tree = Tree::new("stray");
        let output = output(&mut tree.command("leaves-stray"), Duration::from_secs(20)).unwrap();
        assert!(output.status.success(), "{output:?}");
        tree.assert_grandchild_killed();
    }

    /// A descendant that leaves the process group cannot be killed with it,
    /// but it still cannot hold the call past its deadline.
    #[cfg(unix)]
    #[test]
    fn an_escaped_descendant_cannot_extend_the_deadline() {
        let tree = Tree::new("escaped");
        let deadline = Duration::from_secs(5);
        let started = Instant::now();
        let error = output(&mut tree.command("leaves-escaped-pipe-holder"), deadline).unwrap_err();
        let elapsed = started.elapsed();
        let escaped = tree.grandchild();
        // SAFETY: signalling a process ID this test's grandchild recorded.
        unsafe { libc::kill(escaped as libc::pid_t, libc::SIGKILL) };
        assert_eq!(error.kind(), io::ErrorKind::TimedOut, "{error}");
        assert!(
            elapsed < deadline + Duration::from_secs(1),
            "took {elapsed:?}"
        );
    }

    #[cfg(windows)]
    fn echo_command() -> Command {
        let mut command = Command::new("cmd.exe");
        command.args(["/d", "/c", "echo vacancyd"]);
        command
    }

    #[cfg(not(windows))]
    fn echo_command() -> Command {
        let mut command = Command::new("sh");
        command.args(["-c", "printf vacancyd"]);
        command
    }

    #[cfg(windows)]
    fn sleep_command() -> Command {
        let mut command = Command::new("powershell.exe");
        command.args(["-NoProfile", "-Command", "Start-Sleep -Seconds 5"]);
        command
    }

    #[cfg(not(windows))]
    fn sleep_command() -> Command {
        let mut command = Command::new("sleep");
        command.arg("5");
        command
    }
}
