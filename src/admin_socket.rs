//! Linux side of the local admin channel: a Unix domain socket on which the
//! service learns from the kernel which account is asking before it changes
//! lending.
//!
//! The protocol is the Windows admin pipe's (`vacancyd::local_admin`): one
//! JSON line in, one JSON line out, one request per connection. The rules for
//! who may do what are platform-neutral and live in
//! [`vacancyd::local_admin::permits`]; this module only proves the account.
//!
//! # Identity
//!
//! The kernel records a client's credentials when it calls `connect`, and
//! reports them here: `SO_PEERCRED` gives its effective uid and gid, and
//! `SO_PEERGROUPS` its supplementary groups. Nothing the client writes can
//! change them. Root is an administrator. Any other account is one when the
//! connecting process holds the configured admin group (`server.admin_group`,
//! by default `vacancyd-admin`) *and* the group database still lists the
//! account as a member, re-read on every request that needs it. The first
//! condition is the Unix equivalent of an elevated Windows token: a process
//! that dropped its groups, such as a sandboxed service running as that
//! user, is not an administrator. The second makes removal from the group
//! take effect at once, rather than when the account's processes end.
//!
//! # The socket file
//!
//! The socket lives in a directory owned by the service's user and writable
//! by nobody else: under the packaged systemd unit that is
//! `RuntimeDirectory=vacancyd`, `/run/vacancyd`, mode 0755. The service
//! refuses a directory that is a symbolic link, owned by another account, or
//! writable by group or others, and creates only the last path component if
//! it is missing. It works through a descriptor for that verified directory
//! from then on (binding through `/proc/self/fd`), so renaming a directory
//! above it after the check cannot redirect the socket. The socket itself is
//! mode 0666 so any local account can connect: reaching the socket grants
//! nothing, and the peer credentials decide what a caller may do.
//!
//! An existing socket at the path is removed only when nothing is listening
//! on it (a stale socket from a service that did not shut down cleanly).
//! Anything else at the path, including a symbolic link, is left alone and
//! the channel does not start. On shutdown the service removes the socket
//! only if the file at the path is still the one it created.

use std::collections::HashMap;
use std::ffi::{CString, OsStr};
use std::io;
use std::os::fd::{AsRawFd, FromRawFd, OwnedFd};
use std::os::unix::ffi::OsStrExt;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use anyhow::{bail, Context, Result};
use tokio::io::{AsyncWriteExt, BufReader};
use tokio::net::UnixStream;
use tracing::{debug, info, warn};
use vacancyd::local_admin::{
    read_frame, Action, Caller, RefusalReason, Request, Response, MAX_REQUEST_BYTES,
    PROTOCOL_VERSION,
};

use crate::state::AppState;

/// Concurrent admin connections from one account. A person makes these; a
/// small bound keeps one account from occupying the channel.
const PER_ACCOUNT_CONNECTIONS: usize = 4;
/// Concurrent admin connections in total.
const TOTAL_CONNECTIONS: usize = 32;
/// Bounds reading a request and writing its answer.
const REQUEST_TIMEOUT: Duration = Duration::from_secs(5);
/// Bounds consulting the user and group databases, which may be remote
/// (LDAP, SSSD), including any wait for a free lookup. A lookup that does
/// not finish counts as "not an administrator".
const LOOKUP_TIMEOUT: Duration = Duration::from_secs(5);
/// User and group database lookups in progress at once, including ones
/// whose requests have already given up on them. See [`Lookups`].
const MAX_LOOKUPS: usize = 2;
/// Upper bound for one user or group database record, and for a group list.
const MAX_LOOKUP_BUFFER: usize = 1024 * 1024;
const MAX_GROUPS: usize = 65_536;

/// Keep the admin channel available for the life of the service.
pub async fn serve(state: Arc<AppState>) {
    crate::agent::supervise::supervise("local admin", || run(state.clone())).await
}

async fn run(state: Arc<AppState>) -> Result<()> {
    let path = state.config.server.admin_socket_path();
    let (bound, listener) = BoundSocket::bind(&path)?;
    listener.set_nonblocking(true)?;
    let listener = tokio::net::UnixListener::from_std(listener)?;
    describe_admin_group(&state.config.server.admin_group).await;
    let _listening = crate::agent::supervise::Listening::mark(&state.admin_channel_listening);
    info!(socket = %path.display(), "local admin channel listening");
    let table = Arc::new(Mutex::new(Connections::default()));
    let mut connections = tokio::task::JoinSet::new();
    let result: Result<()> = loop {
        while connections.try_join_next().is_some() {}
        let stream = match listener.accept().await {
            Ok((stream, _)) => stream,
            Err(error) => break Err(error).context("accepting an admin connection"),
        };
        let peer = match Peer::of(&stream) {
            Ok(peer) => peer,
            Err(error) => {
                debug!(error = %error, "admin connection without readable peer credentials");
                continue;
            }
        };
        let Some(slot) = Connections::admit(&table, peer.uid) else {
            debug!(uid = peer.uid, "admin connection refused: too many open");
            continue;
        };
        let st = state.clone();
        connections.spawn(async move {
            let _slot = slot;
            if let Err(e) = serve_one(&st, stream, &peer).await {
                debug!(error = %e, "admin connection ended");
            }
        });
    };
    drop(bound);
    result
}

async fn serve_one(state: &AppState, stream: UnixStream, peer: &Peer) -> Result<()> {
    let mut stream = BufReader::new(stream);
    let Some(frame) =
        tokio::time::timeout(REQUEST_TIMEOUT, read_frame(&mut stream, MAX_REQUEST_BYTES))
            .await
            .context("admin request timed out")??
    else {
        return Ok(());
    };
    let response = match serde_json::from_slice::<Request<serde_json::Value>>(&frame) {
        Ok(request) if request.protocol_version == PROTOCOL_VERSION => {
            // Anyone may pause, so a pause never waits on the user and group
            // databases.
            let administrator = request.action.action() != Action::Pause
                && is_administrator(peer, &state.config.server.admin_group).await;
            debug!(
                uid = peer.uid,
                pid = peer.pid,
                administrator,
                action = request.action.action().as_str(),
                "local admin request received"
            );
            let caller = Caller::Account {
                account: format!("uid:{}", peer.uid),
                administrator,
            };
            crate::api::perform_local_admin(state, &caller, request.action)
        }
        _ => {
            warn!(uid = peer.uid, "local admin request was not understood");
            Response::Refused {
                reason: RefusalReason::InvalidRequest,
            }
        }
    };
    let mut line = serde_json::to_vec(&response)?;
    line.push(b'\n');
    tokio::time::timeout(REQUEST_TIMEOUT, async {
        stream.write_all(&line).await?;
        stream.shutdown().await
    })
    .await
    .context("admin response timed out")??;
    Ok(())
}

/// What the kernel reports about a connected client.
#[derive(Debug, Clone)]
struct Peer {
    uid: u32,
    /// Effective gid.
    gid: u32,
    pid: Option<i32>,
    /// Supplementary groups.
    groups: Vec<u32>,
}

impl Peer {
    fn of(stream: &UnixStream) -> io::Result<Self> {
        let credentials = stream.peer_cred()?;
        Ok(Self {
            uid: credentials.uid(),
            gid: credentials.gid(),
            pid: credentials.pid(),
            groups: peer_groups(stream.as_raw_fd())?,
        })
    }

    fn holds(&self, gid: u32) -> bool {
        self.gid == gid || self.groups.contains(&gid)
    }
}

/// The client's supplementary groups as of `connect`, from `SO_PEERGROUPS`.
fn peer_groups(fd: std::os::fd::RawFd) -> io::Result<Vec<u32>> {
    let mut groups: Vec<libc::gid_t> = vec![0; 64];
    loop {
        let mut length = (groups.len() * std::mem::size_of::<libc::gid_t>()) as libc::socklen_t;
        // SAFETY: `groups` provides `length` writable bytes and outlives the
        // call; `length` is a valid in/out parameter.
        let rc = unsafe {
            libc::getsockopt(
                fd,
                libc::SOL_SOCKET,
                libc::SO_PEERGROUPS,
                groups.as_mut_ptr().cast(),
                &mut length,
            )
        };
        let count = length as usize / std::mem::size_of::<libc::gid_t>();
        if rc == 0 {
            groups.truncate(count);
            return Ok(groups);
        }
        let error = io::Error::last_os_error();
        // The kernel reports the size it needs.
        if error.raw_os_error() == Some(libc::ERANGE) && count > groups.len() && count <= MAX_GROUPS
        {
            groups.resize(count, 0);
            continue;
        }
        return Err(error);
    }
}

/// Runs user and group database lookups on their own threads, at most a
/// fixed number at once.
///
/// A started NSS call cannot be cancelled: a request that stops waiting for
/// one leaves it running. So each lookup owns its permit until the call
/// itself returns, not until its request gives up. Lookups stuck in a hung
/// backend then stop new ones from starting, instead of every timed-out
/// request leaving another thread behind. While every permit is held a
/// request waits for one within its deadline and is then answered as a
/// standard user's, so the channel fails closed. Pause never waits here.
///
/// The threads are not Tokio's blocking pool, whose shutdown waits for every
/// task it has started: a lookup that never returns cannot hold up the
/// service stopping either.
struct Lookups {
    permits: Arc<tokio::sync::Semaphore>,
    timeout: Duration,
}

/// Why a lookup produced no answer.
#[derive(Debug)]
enum LookupFailure {
    /// No permit came free, or the lookup did not return, before the deadline.
    TimedOut,
    /// The database answered with an error.
    Failed(io::Error),
    /// The lookup thread could not start, or ended without an answer.
    Worker(String),
}

impl std::fmt::Display for LookupFailure {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::TimedOut => f.write_str("did not finish in time"),
            Self::Failed(error) => write!(f, "{error}"),
            Self::Worker(detail) => f.write_str(detail),
        }
    }
}

static LOOKUPS: std::sync::LazyLock<Lookups> =
    std::sync::LazyLock::new(|| Lookups::new(MAX_LOOKUPS, LOOKUP_TIMEOUT));

impl Lookups {
    fn new(limit: usize, timeout: Duration) -> Self {
        Self {
            permits: Arc::new(tokio::sync::Semaphore::new(limit)),
            timeout,
        }
    }

    async fn run<T: Send + 'static>(
        &self,
        lookup: impl FnOnce() -> io::Result<T> + Send + 'static,
    ) -> Result<T, LookupFailure> {
        let answer = async {
            let permit = self
                .permits
                .clone()
                .acquire_owned()
                .await
                .map_err(|_| LookupFailure::Worker("lookup permits closed".into()))?;
            let (send, receive) = tokio::sync::oneshot::channel();
            std::thread::Builder::new()
                .name("vacancyd-nss".into())
                .spawn(move || {
                    let answer = lookup();
                    let _ = send.send(answer);
                    // Released only now that the call has returned.
                    drop(permit);
                })
                .map_err(|error| {
                    LookupFailure::Worker(format!("cannot start a lookup: {error}"))
                })?;
            receive
                .await
                .map_err(|_| LookupFailure::Worker("the lookup ended without an answer".into()))?
                .map_err(LookupFailure::Failed)
        };
        tokio::time::timeout(self.timeout, answer)
            .await
            .unwrap_or(Err(LookupFailure::TimedOut))
    }
}

/// The admin group's id and the groups the database lists for `uid`, or
/// `None` when the group does not exist.
type Membership = Option<(u32, Vec<u32>)>;

/// Whether `peer` is a local administrator: root, or holding the admin
/// group while the group database lists it as a member.
async fn is_administrator(peer: &Peer, group: &str) -> bool {
    let name = group.to_owned();
    let uid = peer.uid;
    administrator_via(&LOOKUPS, peer, group, move || {
        let Some(gid) = group_id(&name)? else {
            return Ok(None);
        };
        Ok(Some((gid, account_groups(uid)?.unwrap_or_default())))
    })
    .await
}

/// [`is_administrator`], with the database lookup and its limits supplied.
async fn administrator_via(
    lookups: &Lookups,
    peer: &Peer,
    group: &str,
    membership: impl FnOnce() -> io::Result<Membership> + Send + 'static,
) -> bool {
    if peer.uid == 0 {
        return true;
    }
    if group.is_empty() {
        return false;
    }
    let uid = peer.uid;
    match lookups.run(membership).await {
        Ok(Some((gid, listed))) => administrator_by_group(peer, gid, &listed),
        Ok(None) => false,
        Err(LookupFailure::TimedOut) => {
            warn!(
                uid,
                group, "admin group lookup did not finish; treating the caller as a standard user"
            );
            false
        }
        Err(error) => {
            warn!(uid, group, error = %error, "cannot read the admin group membership; treating the caller as a standard user");
            false
        }
    }
}

/// The group rule, apart from the lookups: the process must hold the group
/// and the database must still list the account in it.
fn administrator_by_group(peer: &Peer, admin_gid: u32, listed: &[u32]) -> bool {
    peer.holds(admin_gid) && listed.contains(&admin_gid)
}

/// Log once at startup whether the admin group exists, so an operator can
/// see why nobody but root is an administrator.
async fn describe_admin_group(group: &str) {
    if group.is_empty() {
        info!("local administrators: root only (server.admin_group is empty)");
        return;
    }
    let name = group.to_owned();
    match LOOKUPS.run(move || group_id(&name)).await {
        Ok(Some(gid)) => info!(group, gid, "local administrators: root and members of the admin group"),
        Ok(None) => info!(group, "local administrators: root only; the admin group does not exist (create it to delegate)"),
        Err(_) => warn!(group, "cannot resolve the admin group yet; it is checked again on each request"),
    }
}

fn cstring(value: &OsStr) -> io::Result<CString> {
    CString::new(value.as_bytes()).map_err(|_| io::Error::from(io::ErrorKind::InvalidInput))
}

/// Retry a reentrant NSS call with a growing buffer. `call` returns the
/// function's status code; 0 with no record, or a "not found" code, is
/// `Ok(None)`.
fn nss_lookup<T>(
    mut call: impl FnMut(&mut [u8]) -> (libc::c_int, Option<T>),
) -> io::Result<Option<T>> {
    let mut buffer = vec![0u8; 4096];
    loop {
        match call(&mut buffer) {
            (0, found) => return Ok(found),
            (libc::ERANGE, _) if buffer.len() < MAX_LOOKUP_BUFFER => {
                buffer.resize(buffer.len() * 2, 0);
            }
            // POSIX lets implementations report "no such entry" with these.
            (libc::ENOENT | libc::ESRCH | libc::EBADF | libc::EPERM, _) => return Ok(None),
            (code, _) => return Err(io::Error::from_raw_os_error(code)),
        }
    }
}

/// A group's id by name, from the group database.
fn group_id(name: &str) -> io::Result<Option<u32>> {
    let name = cstring(OsStr::new(name))?;
    nss_lookup(|buffer| {
        // SAFETY: an all-zero `group` is a valid out-parameter.
        let mut record: libc::group = unsafe { std::mem::zeroed() };
        let mut result = std::ptr::null_mut();
        // SAFETY: every pointer is valid for the call; `buffer` provides
        // `buffer.len()` bytes for the record's strings.
        let rc = unsafe {
            libc::getgrnam_r(
                name.as_ptr(),
                &mut record,
                buffer.as_mut_ptr().cast(),
                buffer.len(),
                &mut result,
            )
        };
        (rc, (!result.is_null()).then_some(record.gr_gid))
    })
}

/// Every group the database lists for `uid`, including its primary group,
/// or `None` for a uid without an account record.
fn account_groups(uid: u32) -> io::Result<Option<Vec<u32>>> {
    let Some((name, primary)) = nss_lookup(|buffer| {
        // SAFETY: an all-zero `passwd` is a valid out-parameter.
        let mut record: libc::passwd = unsafe { std::mem::zeroed() };
        let mut result = std::ptr::null_mut();
        // SAFETY: as for getgrnam_r above.
        let rc = unsafe {
            libc::getpwuid_r(
                uid,
                &mut record,
                buffer.as_mut_ptr().cast(),
                buffer.len(),
                &mut result,
            )
        };
        let found = (!result.is_null()).then(|| {
            // SAFETY: a successful lookup points pw_name at a NUL-terminated
            // string inside `buffer`, which is still borrowed here.
            let name = unsafe { std::ffi::CStr::from_ptr(record.pw_name) }.to_owned();
            (name, record.pw_gid)
        });
        (rc, found)
    })?
    else {
        return Ok(None);
    };
    let mut groups: Vec<libc::gid_t> = vec![0; 64];
    loop {
        let mut count = groups.len() as libc::c_int;
        // SAFETY: `groups` has room for `count` ids and `count` is a valid
        // in/out parameter.
        let rc =
            unsafe { libc::getgrouplist(name.as_ptr(), primary, groups.as_mut_ptr(), &mut count) };
        let count = usize::try_from(count).unwrap_or(0);
        if rc >= 0 {
            groups.truncate(count.min(groups.len()));
            return Ok(Some(groups));
        }
        // -1: the list did not fit; `count` is the size it needs.
        let wanted = count.max(groups.len() * 2);
        if wanted > MAX_GROUPS {
            return Err(io::Error::other("account belongs to too many groups"));
        }
        groups.resize(wanted, 0);
    }
}

/// Counts open admin connections, overall and per account.
#[derive(Default)]
struct Connections {
    total: usize,
    by_uid: HashMap<u32, usize>,
}

/// One admitted connection; releases its place when dropped.
struct Slot {
    table: Arc<Mutex<Connections>>,
    uid: u32,
}

impl Connections {
    fn admit(table: &Arc<Mutex<Self>>, uid: u32) -> Option<Slot> {
        let mut connections = table.lock().unwrap();
        let open = connections.by_uid.get(&uid).copied().unwrap_or(0);
        if connections.total >= TOTAL_CONNECTIONS || open >= PER_ACCOUNT_CONNECTIONS {
            return None;
        }
        connections.total += 1;
        connections.by_uid.insert(uid, open + 1);
        Some(Slot {
            table: table.clone(),
            uid,
        })
    }
}

impl Drop for Slot {
    fn drop(&mut self) {
        let mut connections = self.table.lock().unwrap();
        connections.total -= 1;
        if let Some(open) = connections.by_uid.get_mut(&self.uid) {
            *open -= 1;
            if *open == 0 {
                connections.by_uid.remove(&self.uid);
            }
        }
    }
}

/// The socket file this service created, removed on drop only while the
/// file at its path is still that socket.
struct BoundSocket {
    directory: OwnedFd,
    name: CString,
    identity: FileIdentity,
}

/// What distinguishes the socket this service created from a later file at
/// the same path. An inode number can be reused as soon as a file is gone,
/// and file timestamps come from a coarse clock, so after binding the
/// service stamps its socket's modification time with a random nanosecond
/// value that a later file will not carry by chance.
#[derive(Debug, PartialEq, Eq)]
struct FileIdentity {
    kind: libc::mode_t,
    device: u64,
    inode: u64,
    modified: (i64, i64),
}

impl FileIdentity {
    fn of(status: &libc::stat) -> Self {
        Self {
            kind: status.st_mode & libc::S_IFMT,
            device: status.st_dev,
            inode: status.st_ino,
            modified: (status.st_mtime, status.st_mtime_nsec),
        }
    }
}

/// Set `name`'s modification time to now with random nanoseconds.
fn stamp(directory: &OwnedFd, name: &CString) -> io::Result<()> {
    let mut random = [0u8; 4];
    getrandom::getrandom(&mut random).map_err(|error| io::Error::other(error.to_string()))?;
    let nanoseconds = i64::from(u32::from_ne_bytes(random) % 1_000_000_000);
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_err(io::Error::other)?;
    let times = [
        libc::timespec {
            tv_sec: 0,
            tv_nsec: libc::UTIME_OMIT,
        },
        libc::timespec {
            tv_sec: now.as_secs() as libc::time_t,
            tv_nsec: nanoseconds as _,
        },
    ];
    // SAFETY: a valid directory descriptor, NUL-terminated name and a
    // two-element timespec array.
    if unsafe {
        libc::utimensat(
            directory.as_raw_fd(),
            name.as_ptr(),
            times.as_ptr(),
            libc::AT_SYMLINK_NOFOLLOW,
        )
    } != 0
    {
        return Err(io::Error::last_os_error());
    }
    Ok(())
}

impl BoundSocket {
    fn bind(path: &Path) -> Result<(Self, std::os::unix::net::UnixListener)> {
        let (Some(parent), Some(name)) = (path.parent(), path.file_name()) else {
            bail!("admin socket path {} names no file", path.display());
        };
        let directory = open_private_directory(parent)
            .with_context(|| format!("admin socket directory {}", parent.display()))?;
        let name = cstring(name).context("admin socket name")?;
        let through = through_directory(&directory, &name);
        remove_stale_socket(&directory, &name, &through)
            .with_context(|| format!("admin socket {}", path.display()))?;
        let listener = std::os::unix::net::UnixListener::bind(&through)
            .with_context(|| format!("binding admin socket {}", path.display()))?;
        // Connecting needs write permission on the socket. Reaching it
        // grants nothing by itself; the peer credentials decide.
        // SAFETY: a valid directory descriptor and NUL-terminated name.
        if unsafe { libc::fchmodat(directory.as_raw_fd(), name.as_ptr(), 0o666, 0) } != 0 {
            return Err(io::Error::last_os_error())
                .with_context(|| format!("setting permissions on {}", path.display()));
        }
        stamp(&directory, &name).with_context(|| format!("marking {}", path.display()))?;
        let status = stat_at(&directory, &name)?.context("admin socket vanished after binding")?;
        // SAFETY: geteuid has no preconditions.
        if status.st_mode & libc::S_IFMT != libc::S_IFSOCK
            || status.st_uid != unsafe { libc::geteuid() }
        {
            bail!(
                "{} is not the socket this service just created",
                path.display()
            );
        }
        Ok((
            Self {
                directory,
                name,
                identity: FileIdentity::of(&status),
            },
            listener,
        ))
    }
}

impl Drop for BoundSocket {
    fn drop(&mut self) {
        if let Ok(Some(status)) = stat_at(&self.directory, &self.name) {
            if FileIdentity::of(&status) == self.identity {
                // SAFETY: a valid directory descriptor and NUL-terminated name.
                unsafe { libc::unlinkat(self.directory.as_raw_fd(), self.name.as_ptr(), 0) };
            }
        }
    }
}

/// The path of `name` inside the directory open as `directory`, resolved by
/// the kernel through the descriptor rather than by name.
fn through_directory(directory: &OwnedFd, name: &CString) -> PathBuf {
    let mut path = PathBuf::from(format!("/proc/self/fd/{}", directory.as_raw_fd()));
    path.push(OsStr::from_bytes(name.as_bytes()));
    path
}

/// Open the socket's directory, creating only its last component if
/// missing, and check that no other account can change what is inside.
fn open_private_directory(path: &Path) -> Result<OwnedFd> {
    let c_path = cstring(path.as_os_str())?;
    let flags = libc::O_RDONLY | libc::O_DIRECTORY | libc::O_NOFOLLOW | libc::O_CLOEXEC;
    let open = || {
        // SAFETY: a NUL-terminated path; the descriptor is owned below.
        let fd = unsafe { libc::open(c_path.as_ptr(), flags) };
        if fd < 0 {
            Err(io::Error::last_os_error())
        } else {
            // SAFETY: a freshly opened descriptor nothing else owns.
            Ok(unsafe { OwnedFd::from_raw_fd(fd) })
        }
    };
    let mut created = false;
    let directory = match open() {
        Ok(directory) => directory,
        Err(error) if error.kind() == io::ErrorKind::NotFound => {
            // Private until checked; opened for others below.
            // SAFETY: a NUL-terminated path.
            if unsafe { libc::mkdir(c_path.as_ptr(), 0o700) } == 0 {
                created = true;
            } else {
                let error = io::Error::last_os_error();
                if error.kind() != io::ErrorKind::AlreadyExists {
                    return Err(error).context("creating the directory");
                }
            }
            open().context("opening the directory")?
        }
        Err(error) if matches!(error.raw_os_error(), Some(libc::ELOOP | libc::ENOTDIR)) => {
            bail!("it is a symbolic link or not a directory");
        }
        Err(error) => return Err(error).context("opening the directory"),
    };
    let status = fstat(&directory)?;
    // SAFETY: geteuid has no preconditions.
    let me = unsafe { libc::geteuid() };
    if status.st_uid != me {
        bail!(
            "it is owned by uid {}, not by this service (uid {me})",
            status.st_uid
        );
    }
    if status.st_mode & 0o022 != 0 {
        bail!(
            "it is writable by group or others (mode {:o})",
            status.st_mode & 0o7777
        );
    }
    if created {
        // Let local accounts reach the socket inside.
        // SAFETY: a valid directory descriptor.
        if unsafe { libc::fchmod(directory.as_raw_fd(), 0o755) } != 0 {
            return Err(io::Error::last_os_error()).context("opening the directory to local users");
        }
    } else if status.st_mode & 0o001 == 0 {
        warn!(
            directory = %path.display(),
            mode = format!("{:o}", status.st_mode & 0o7777),
            "other local accounts cannot reach the admin socket in this directory"
        );
    }
    Ok(directory)
}

/// Remove a socket left by a service that did not shut down cleanly. A
/// socket something still listens on, and anything that is not a socket,
/// are left alone.
fn remove_stale_socket(directory: &OwnedFd, name: &CString, through: &Path) -> Result<()> {
    let Some(status) = stat_at(directory, name)? else {
        return Ok(());
    };
    if status.st_mode & libc::S_IFMT != libc::S_IFSOCK {
        bail!("something other than a socket already exists there; refusing to replace it");
    }
    match std::os::unix::net::UnixStream::connect(through) {
        Ok(_) => bail!("another process is already serving it"),
        Err(error) if error.kind() == io::ErrorKind::ConnectionRefused => {
            // SAFETY: a valid directory descriptor and NUL-terminated name.
            if unsafe { libc::unlinkat(directory.as_raw_fd(), name.as_ptr(), 0) } != 0 {
                return Err(io::Error::last_os_error()).context("removing the stale socket");
            }
            info!("removed a stale admin socket left by an earlier run");
            Ok(())
        }
        Err(error) => Err(error).context("checking whether the existing socket is in use"),
    }
}

fn fstat(fd: &OwnedFd) -> io::Result<libc::stat> {
    // SAFETY: an all-zero stat is a valid out-parameter.
    let mut status: libc::stat = unsafe { std::mem::zeroed() };
    // SAFETY: a valid descriptor and out-parameter.
    if unsafe { libc::fstat(fd.as_raw_fd(), &mut status) } != 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(status)
}

/// `lstat` of `name` inside `directory`; `None` when it does not exist.
fn stat_at(directory: &OwnedFd, name: &CString) -> io::Result<Option<libc::stat>> {
    // SAFETY: an all-zero stat is a valid out-parameter.
    let mut status: libc::stat = unsafe { std::mem::zeroed() };
    // SAFETY: a valid directory descriptor, NUL-terminated name and
    // out-parameter.
    let rc = unsafe {
        libc::fstatat(
            directory.as_raw_fd(),
            name.as_ptr(),
            &mut status,
            libc::AT_SYMLINK_NOFOLLOW,
        )
    };
    if rc == 0 {
        return Ok(Some(status));
    }
    let error = io::Error::last_os_error();
    if error.kind() == io::ErrorKind::NotFound {
        Ok(None)
    } else {
        Err(error)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::fs::{MetadataExt, PermissionsExt};
    use vacancyd::local_admin::{socket_client, PausedBy, RequestedAction};

    /// A private scratch directory, removed when dropped.
    struct Scratch(PathBuf);

    impl Scratch {
        fn new(label: &str) -> Self {
            let path = std::env::temp_dir().join(format!(
                "vacancyd-{label}-{}-{}",
                std::process::id(),
                std::time::SystemTime::now()
                    .duration_since(std::time::UNIX_EPOCH)
                    .unwrap()
                    .as_nanos()
            ));
            std::fs::create_dir(&path).unwrap();
            std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755)).unwrap();
            Self(path)
        }
    }

    impl Drop for Scratch {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    fn euid() -> u32 {
        // SAFETY: geteuid has no preconditions.
        unsafe { libc::geteuid() }
    }

    fn state_for(directory: &Path, group: &str) -> (Arc<AppState>, PathBuf, PathBuf) {
        let socket = directory.join("admin.sock");
        let config = directory.join("vacancyd.toml");
        std::fs::write(
            &config,
            format!(
                "[server]\nlisten = \"127.0.0.1:9811\"\nadmin_socket = \"{}\"\nadmin_group = \"{group}\"\n\n[owner.gpu_pressure]\nenabled = false\n\n[[resource]]\nid = \"memory\"\nkind = \"system-memory\"\nreserve = 0\n",
                socket.display()
            ),
        )
        .unwrap();
        let state =
            AppState::new_with_config_path(crate::config::Config::load(&config).unwrap(), &config)
                .unwrap();
        // The daemon's own default: loopback does not administer on Linux.
        assert_eq!(
            state.local_admin_policy,
            vacancyd::local_admin::Policy::for_this_platform()
        );
        assert!(!state.local_admin_policy.loopback_is_administrator);
        (Arc::new(state), socket, config)
    }

    async fn start(state: &Arc<AppState>, socket: &Path) -> tokio::task::JoinHandle<Result<()>> {
        let server = tokio::spawn(run(state.clone()));
        for _ in 0..200 {
            if std::fs::symlink_metadata(socket).is_ok() {
                break;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        // Bound, but give the task a moment to reach `accept`.
        tokio::time::sleep(Duration::from_millis(20)).await;
        server
    }

    async fn stop(server: tokio::task::JoinHandle<Result<()>>) {
        server.abort();
        let _ = server.await;
    }

    fn save_request(config: &Path) -> Request {
        let snapshot = crate::api::current_settings(config).unwrap();
        Request::new(RequestedAction::SaveSettings {
            settings: snapshot.into(),
        })
    }

    async fn send(socket: &Path, request: Request) -> Response {
        socket_client::send(socket, &request).await.unwrap()
    }

    fn refused() -> Response {
        Response::Refused {
            reason: RefusalReason::AdministratorRequired,
        }
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn the_socket_identifies_this_account_and_applies_the_rule() {
        let scratch = Scratch::new("admin-socket");
        let (state, socket, config) = state_for(&scratch.0, "vacancyd-admin");
        let server = start(&state, &socket).await;

        let metadata = std::fs::symlink_metadata(&socket).unwrap();
        assert!(std::os::unix::fs::FileTypeExt::is_socket(
            &metadata.file_type()
        ));
        assert_eq!(metadata.mode() & 0o7777, 0o666);
        assert_eq!(metadata.uid(), euid());

        let me = PausedBy::Account(format!("uid:{}", euid()));
        assert_eq!(
            send(&socket, Request::new(RequestedAction::Pause {})).await,
            Response::Lending {
                lending_enabled: false
            }
        );
        assert_eq!(*state.paused_by.lock().unwrap(), Some(me.clone()));

        let original = std::fs::read(&config).unwrap();
        let save = send(&socket, save_request(&config)).await;
        let switch = send(
            &socket,
            Request::new(RequestedAction::SetResource {
                resource: "memory".into(),
                enabled: false,
            }),
        )
        .await;
        if euid() == 0 {
            assert!(
                matches!(
                    save,
                    Response::Saved {
                        restart_required: false,
                        ..
                    }
                ),
                "{save:?}"
            );
            assert_ne!(std::fs::read(&config).unwrap(), original);
            assert_eq!(
                switch,
                Response::Resource {
                    resource: "memory".into(),
                    enabled: false
                }
            );
        } else {
            // A standard account may pause and resume its own pause, and
            // nothing else.
            assert_eq!(save, refused());
            assert_eq!(switch, refused());
            assert_eq!(std::fs::read(&config).unwrap(), original);
            assert!(state.resource_enabled("memory"));
            assert_eq!(
                send(
                    &socket,
                    Request::new(RequestedAction::ReleaseWork {
                        id: 1,
                        upstream_stopped: true
                    })
                )
                .await,
                refused()
            );
        }
        assert_eq!(
            send(&socket, Request::new(RequestedAction::Resume {})).await,
            Response::Lending {
                lending_enabled: true
            }
        );
        assert_eq!(*state.paused_by.lock().unwrap(), None);

        stop(server).await;
        assert!(
            std::fs::symlink_metadata(&socket).is_err(),
            "stopping removes the socket"
        );
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn malformed_and_oversized_requests_change_nothing() {
        use tokio::io::AsyncReadExt;
        let scratch = Scratch::new("admin-socket-bad");
        let (state, socket, _config) = state_for(&scratch.0, "");
        let server = start(&state, &socket).await;

        for bad in [
            &b"not json\n"[..],
            br#"{"protocol_version":2,"action":{"kind":"pause"}}
"#,
            br#"{"protocol_version":1,"action":{"kind":"pause","uid":0}}
"#,
        ] {
            let mut stream = UnixStream::connect(&socket).await.unwrap();
            stream.write_all(bad).await.unwrap();
            let mut answer = String::new();
            stream.read_to_string(&mut answer).await.unwrap();
            assert_eq!(
                serde_json::from_str::<Response>(answer.trim_end()).unwrap(),
                Response::Refused {
                    reason: RefusalReason::InvalidRequest
                }
            );
        }
        // Too large: the connection is closed without an answer.
        let mut stream = UnixStream::connect(&socket).await.unwrap();
        let _ = stream.write_all(&vec![b' '; MAX_REQUEST_BYTES + 2]).await;
        let mut answer = Vec::new();
        let _ = stream.read_to_end(&mut answer).await;
        assert!(answer.is_empty());
        // Connecting and closing without a request is harmless.
        drop(UnixStream::connect(&socket).await.unwrap());
        assert!(state
            .lending_enabled
            .load(std::sync::atomic::Ordering::Relaxed));
        // The channel still answers.
        assert_eq!(
            send(&socket, Request::new(RequestedAction::Pause {})).await,
            Response::Lending {
                lending_enabled: false
            }
        );
        stop(server).await;
    }

    #[test]
    fn a_stale_socket_is_replaced_but_a_live_one_is_not() {
        let scratch = Scratch::new("admin-socket-stale");
        let path = scratch.0.join("admin.sock");
        // A socket nobody listens on any more, as a crash leaves it.
        drop(std::os::unix::net::UnixListener::bind(&path).unwrap());
        assert!(std::fs::symlink_metadata(&path).is_ok());
        let (bound, listener) = BoundSocket::bind(&path).unwrap();

        // While it is served, a second service refuses to take it over.
        let error = BoundSocket::bind(&path).err().unwrap();
        assert!(
            format!("{error:#}").contains("already serving"),
            "{error:#}"
        );
        std::os::unix::net::UnixStream::connect(&path).unwrap();
        drop(listener);
        drop(bound);
        assert!(std::fs::symlink_metadata(&path).is_err());
    }

    #[test]
    fn nothing_but_a_stale_socket_is_ever_replaced() {
        let scratch = Scratch::new("admin-socket-links");
        let path = scratch.0.join("admin.sock");
        let victim = scratch.0.join("victim");
        std::fs::write(&victim, b"keep").unwrap();

        std::os::unix::fs::symlink(&victim, &path).unwrap();
        let error = BoundSocket::bind(&path).err().unwrap();
        assert!(
            format!("{error:#}").contains("other than a socket"),
            "{error:#}"
        );
        assert!(std::fs::symlink_metadata(&path)
            .unwrap()
            .file_type()
            .is_symlink());
        let victim_mode = std::fs::metadata(&victim).unwrap().mode();
        assert_eq!(std::fs::read(&victim).unwrap(), b"keep");
        assert_ne!(victim_mode & 0o777, 0o666);
        std::fs::remove_file(&path).unwrap();

        // A dangling link, which bind() would otherwise create a socket
        // through, is refused the same way.
        std::os::unix::fs::symlink(scratch.0.join("elsewhere.sock"), &path).unwrap();
        assert!(BoundSocket::bind(&path).is_err());
        assert!(std::fs::symlink_metadata(scratch.0.join("elsewhere.sock")).is_err());
        std::fs::remove_file(&path).unwrap();

        // A symbolic link to a socket someone else serves.
        let other = scratch.0.join("other.sock");
        let _other = std::os::unix::net::UnixListener::bind(&other).unwrap();
        std::os::unix::fs::symlink(&other, &path).unwrap();
        assert!(BoundSocket::bind(&path).is_err());
        std::fs::remove_file(&path).unwrap();

        std::fs::write(&path, b"a regular file").unwrap();
        assert!(BoundSocket::bind(&path).is_err());
        assert_eq!(std::fs::read(&path).unwrap(), b"a regular file");
        std::fs::remove_file(&path).unwrap();

        std::fs::create_dir(&path).unwrap();
        assert!(BoundSocket::bind(&path).is_err());
        assert!(std::fs::metadata(&path).unwrap().is_dir());
    }

    #[test]
    fn the_socket_directory_must_be_private_to_the_service() {
        let scratch = Scratch::new("admin-socket-dirs");

        // A missing directory is created, open to local users for traversal.
        let created = scratch.0.join("run");
        let (bound, _listener) = BoundSocket::bind(&created.join("admin.sock")).unwrap();
        assert_eq!(std::fs::metadata(&created).unwrap().mode() & 0o7777, 0o755);
        drop(bound);

        // Only the last component is created.
        assert!(BoundSocket::bind(&scratch.0.join("a/b/admin.sock")).is_err());
        assert!(!scratch.0.join("a").exists());

        // A symbolic link in place of the directory.
        let real = scratch.0.join("real");
        std::fs::create_dir(&real).unwrap();
        let link = scratch.0.join("link");
        std::os::unix::fs::symlink(&real, &link).unwrap();
        let error = BoundSocket::bind(&link.join("admin.sock")).err().unwrap();
        assert!(format!("{error:#}").contains("symbolic link"), "{error:#}");
        assert!(std::fs::read_dir(&real).unwrap().next().is_none());

        // A directory others could write to.
        for mode in [0o777, 0o775, 0o757, 0o1777] {
            let open = scratch.0.join(format!("open-{mode:o}"));
            std::fs::create_dir(&open).unwrap();
            std::fs::set_permissions(&open, std::fs::Permissions::from_mode(mode)).unwrap();
            let error = BoundSocket::bind(&open.join("admin.sock")).err().unwrap();
            assert!(
                format!("{error:#}").contains("writable"),
                "{mode:o}: {error:#}"
            );
            assert!(std::fs::read_dir(&open).unwrap().next().is_none());
        }

        // A file in place of the directory.
        let file = scratch.0.join("file");
        std::fs::write(&file, b"").unwrap();
        assert!(BoundSocket::bind(&file.join("admin.sock")).is_err());

        // Owned by another account (only root can arrange this).
        if euid() == 0 {
            let foreign = scratch.0.join("foreign");
            std::fs::create_dir(&foreign).unwrap();
            std::os::unix::fs::chown(&foreign, Some(65534), Some(65534)).unwrap();
            let error = BoundSocket::bind(&foreign.join("admin.sock"))
                .err()
                .unwrap();
            assert!(
                format!("{error:#}").contains("owned by uid 65534"),
                "{error:#}"
            );
            assert!(std::fs::read_dir(&foreign).unwrap().next().is_none());
        }
    }

    #[test]
    fn dropping_removes_only_the_socket_this_service_created() {
        let scratch = Scratch::new("admin-socket-drop");
        let path = scratch.0.join("admin.sock");
        let (bound, listener) = BoundSocket::bind(&path).unwrap();
        drop(listener);
        // Something replaced it after binding; shutdown must leave that.
        std::fs::remove_file(&path).unwrap();
        std::fs::write(&path, b"replacement").unwrap();
        drop(bound);
        assert_eq!(std::fs::read(&path).unwrap(), b"replacement");

        // The same with a new socket, which may reuse the inode number.
        std::fs::remove_file(&path).unwrap();
        let (bound, listener) = BoundSocket::bind(&path).unwrap();
        drop(listener);
        std::fs::remove_file(&path).unwrap();
        let _successor = std::os::unix::net::UnixListener::bind(&path).unwrap();
        drop(bound);
        assert!(std::fs::symlink_metadata(&path).is_ok());
        // Repeated, without pauses, so the coarse clock cannot tell them
        // apart and inode numbers get reused.
        for _ in 0..200 {
            std::fs::remove_file(&path).unwrap();
            let (bound, listener) = BoundSocket::bind(&path).unwrap();
            drop(listener);
            std::fs::remove_file(&path).unwrap();
            let _successor = std::os::unix::net::UnixListener::bind(&path).unwrap();
            drop(bound);
            assert!(std::fs::symlink_metadata(&path).is_ok());
        }
    }

    #[test]
    fn the_group_rule_needs_both_the_process_and_the_database() {
        let peer = |gid, groups: &[u32]| Peer {
            uid: 1000,
            gid,
            pid: None,
            groups: groups.to_vec(),
        };
        const ADMIN: u32 = 990;
        // Held as the effective group or a supplementary group, and listed.
        assert!(administrator_by_group(
            &peer(ADMIN, &[]),
            ADMIN,
            &[1000, ADMIN]
        ));
        assert!(administrator_by_group(
            &peer(1000, &[27, ADMIN]),
            ADMIN,
            &[ADMIN]
        ));
        // Listed in the database, but this process does not hold the group
        // (it dropped it, or the account was added after this login).
        assert!(!administrator_by_group(
            &peer(1000, &[27]),
            ADMIN,
            &[1000, ADMIN]
        ));
        // Holds the group, but the database no longer lists the account.
        assert!(!administrator_by_group(
            &peer(1000, &[ADMIN]),
            ADMIN,
            &[1000]
        ));
        assert!(!administrator_by_group(&peer(ADMIN, &[]), ADMIN, &[]));
    }

    #[test]
    fn database_lookups_resolve_real_records_and_miss_cleanly() {
        assert_eq!(group_id("root").unwrap(), Some(0));
        assert_eq!(group_id("vacancyd-no-such-group-for-tests").unwrap(), None);
        let root = account_groups(0).unwrap().unwrap();
        assert!(root.contains(&0), "{root:?}");
        assert_eq!(account_groups(3_999_999_999).unwrap(), None);
        assert!(group_id("bad\0name").is_err());
    }

    #[tokio::test]
    async fn root_is_an_administrator_and_an_unknown_group_makes_nobody_else_one() {
        let peer = |uid| Peer {
            uid,
            gid: uid,
            pid: None,
            groups: vec![],
        };
        assert!(is_administrator(&peer(0), "").await);
        assert!(is_administrator(&peer(0), "vacancyd-no-such-group-for-tests").await);
        assert!(!is_administrator(&peer(3_999_999_999), "").await);
        assert!(!is_administrator(&peer(3_999_999_999), "vacancyd-no-such-group-for-tests").await);
        // A uid with no account record is never a group member.
        assert!(!is_administrator(&peer(3_999_999_999), "root").await);
    }

    /// A lookup that blocks until its gate opens, counting how many have
    /// started and how many are still inside the call.
    #[derive(Clone, Default)]
    struct HungLookup {
        gate: Arc<(Mutex<bool>, std::sync::Condvar)>,
        started: Arc<std::sync::atomic::AtomicUsize>,
        running: Arc<std::sync::atomic::AtomicUsize>,
    }

    impl HungLookup {
        fn call(&self) -> impl FnOnce() -> io::Result<Membership> + Send + 'static {
            let this = self.clone();
            move || {
                use std::sync::atomic::Ordering::SeqCst;
                this.started.fetch_add(1, SeqCst);
                this.running.fetch_add(1, SeqCst);
                let (open, opened) = &*this.gate;
                let mut open = open.lock().unwrap();
                while !*open {
                    open = opened.wait(open).unwrap();
                }
                this.running.fetch_sub(1, SeqCst);
                Ok(Some((990, vec![990])))
            }
        }

        fn release(&self) {
            let (open, opened) = &*self.gate;
            *open.lock().unwrap() = true;
            opened.notify_all();
        }

        fn started(&self) -> usize {
            self.started.load(std::sync::atomic::Ordering::SeqCst)
        }

        fn running(&self) -> usize {
            self.running.load(std::sync::atomic::Ordering::SeqCst)
        }
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn hung_lookups_cannot_accumulate_beyond_the_limit() {
        use futures_util::future::join_all;
        let lookups = Lookups::new(2, Duration::from_millis(100));
        let hung = HungLookup::default();
        let member = Peer {
            uid: 1000,
            gid: 990,
            pid: None,
            groups: vec![],
        };
        // Wave after wave of requests, each giving up on its lookup, as a
        // caller reconnecting to a socket whose backend hangs would.
        for _ in 0..5 {
            let answers = join_all(
                (0..20)
                    .map(|_| administrator_via(&lookups, &member, "vacancyd-admin", hung.call())),
            )
            .await;
            assert!(answers.iter().all(|administrator| !administrator));
            // The two admitted lookups start on their own threads; allow a
            // loaded machine time to schedule them, then check none beyond.
            let deadline = std::time::Instant::now() + Duration::from_secs(5);
            while hung.started() < 2 && std::time::Instant::now() < deadline {
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
            assert_eq!(hung.started(), 2, "a lookup started past the limit");
            assert_eq!(hung.running(), 2);
        }
        // A lookup that cannot start is refused without ever running.
        assert!(matches!(
            lookups.run(|| Ok(())).await,
            Err(LookupFailure::TimedOut)
        ));
        // Once the backend answers, the abandoned calls return their
        // permits and lookups work again.
        hung.release();
        let deadline = std::time::Instant::now() + Duration::from_secs(5);
        while hung.running() > 0 {
            assert!(
                std::time::Instant::now() < deadline,
                "hung lookups never returned"
            );
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        let fresh = HungLookup::default();
        fresh.release();
        assert!(administrator_via(&lookups, &member, "vacancyd-admin", fresh.call()).await);
        assert_eq!(hung.started(), 2);
    }

    #[tokio::test]
    async fn lookup_errors_and_panics_make_a_standard_user_and_free_the_permit() {
        let lookups = Lookups::new(1, Duration::from_secs(5));
        let member = Peer {
            uid: 1000,
            gid: 990,
            pid: None,
            groups: vec![],
        };
        assert!(
            !administrator_via(&lookups, &member, "g", || Err(io::Error::other(
                "ldap down"
            )))
            .await
        );
        assert!(!administrator_via(&lookups, &member, "g", || panic!("nss module crashed")).await);
        assert!(administrator_via(&lookups, &member, "g", || Ok(Some((990, vec![990])))).await);
        // Root and an empty group never consult the database.
        let root = Peer {
            uid: 0,
            ..member.clone()
        };
        assert!(administrator_via(&lookups, &root, "g", || panic!("not called")).await);
        assert!(!administrator_via(&lookups, &member, "", || panic!("not called")).await);
    }

    #[test]
    fn connections_are_bounded_per_account_and_overall() {
        let table = Arc::new(Mutex::new(Connections::default()));
        let alice: Vec<Slot> = (0..PER_ACCOUNT_CONNECTIONS)
            .map(|_| Connections::admit(&table, 1000).unwrap())
            .collect();
        assert!(Connections::admit(&table, 1000).is_none());
        // Another account, and root, still get in.
        let bob = Connections::admit(&table, 1001).unwrap();
        let root = Connections::admit(&table, 0).unwrap();
        drop(alice);
        assert!(Connections::admit(&table, 1000).is_some());
        drop((bob, root));
        assert_eq!(table.lock().unwrap().total, 0);
        assert!(table.lock().unwrap().by_uid.is_empty());

        let many: Vec<Slot> = (0..TOTAL_CONNECTIONS as u32)
            .map(|uid| Connections::admit(&table, 2000 + uid).unwrap())
            .collect();
        assert!(Connections::admit(&table, 0).is_none());
        drop(many);
        assert!(Connections::admit(&table, 0).is_some());
    }

    /// Send `request` as another account from a forked child and return the
    /// answer. The child runs only async-signal-safe system calls: the test
    /// harness is multi-threaded.
    fn request_as(
        socket: &Path,
        uid: u32,
        gid: u32,
        groups: &[u32],
        request: &Request,
    ) -> Option<Response> {
        use std::io::Read;
        let mut line = serde_json::to_vec(request).unwrap();
        line.push(b'\n');
        // SAFETY: an all-zero sockaddr_un is valid; the path fits (checked).
        let mut address: libc::sockaddr_un = unsafe { std::mem::zeroed() };
        address.sun_family = libc::AF_UNIX as libc::sa_family_t;
        let bytes = socket.as_os_str().as_bytes();
        assert!(bytes.len() < address.sun_path.len());
        for (slot, byte) in address.sun_path.iter_mut().zip(bytes) {
            *slot = *byte as libc::c_char;
        }
        let groups: Vec<libc::gid_t> = groups.to_vec();
        let mut buffer = vec![0u8; 256 * 1024];
        let mut pipe = [0; 2];
        // SAFETY: a valid two-element out array.
        assert_eq!(unsafe { libc::pipe(pipe.as_mut_ptr()) }, 0);
        // SAFETY: the child below calls only async-signal-safe functions on
        // memory allocated before the fork, then _exit.
        let pid = unsafe { libc::fork() };
        assert!(pid >= 0);
        if pid == 0 {
            unsafe {
                // Keep only the pipe's write end, as descriptor 3. The child
                // must not hold copies of other tests' descriptors, such as
                // an admission journal's lock, while it runs.
                if libc::dup2(pipe[1], 3) != 3 {
                    libc::_exit(9);
                }
                if libc::syscall(libc::SYS_close_range, 4u32, u32::MAX, 0u32) != 0 {
                    for fd in 4..65_536 {
                        libc::close(fd);
                    }
                }
                let pipe = [-1, 3];
                if libc::setgroups(groups.len(), groups.as_ptr()) != 0
                    || libc::setgid(gid) != 0
                    || libc::setuid(uid) != 0
                {
                    libc::_exit(10);
                }
                let fd = libc::socket(libc::AF_UNIX, libc::SOCK_STREAM, 0);
                if fd < 0 {
                    libc::_exit(11);
                }
                if libc::connect(
                    fd,
                    (&address as *const libc::sockaddr_un).cast(),
                    std::mem::size_of::<libc::sockaddr_un>() as libc::socklen_t,
                ) != 0
                {
                    libc::_exit(12);
                }
                let mut sent = 0;
                while sent < line.len() {
                    let n = libc::write(fd, line.as_ptr().add(sent).cast(), line.len() - sent);
                    if n <= 0 {
                        libc::_exit(13);
                    }
                    sent += n as usize;
                }
                let mut received = 0;
                loop {
                    let n = libc::read(
                        fd,
                        buffer.as_mut_ptr().add(received).cast(),
                        buffer.len() - received,
                    );
                    if n < 0 {
                        libc::_exit(14);
                    }
                    if n == 0 || received + n as usize == buffer.len() {
                        received += n as usize;
                        break;
                    }
                    received += n as usize;
                }
                let mut written = 0;
                while written < received {
                    let n = libc::write(
                        pipe[1],
                        buffer.as_ptr().add(written).cast(),
                        received - written,
                    );
                    if n <= 0 {
                        libc::_exit(15);
                    }
                    written += n as usize;
                }
                libc::_exit(0);
            }
        }
        // SAFETY: the parent's copy of the write end, closed once.
        unsafe { libc::close(pipe[1]) };
        // SAFETY: the read end, owned from here on.
        let mut reader = unsafe { std::fs::File::from_raw_fd(pipe[0]) };
        let mut answer = Vec::new();
        reader.read_to_end(&mut answer).unwrap();
        let mut status = 0;
        // SAFETY: our own child and a valid out-parameter.
        assert_eq!(unsafe { libc::waitpid(pid, &mut status, 0) }, pid);
        assert!(libc::WIFEXITED(status), "child status {status}");
        assert_eq!(
            libc::WEXITSTATUS(status),
            0,
            "child failed at step {}",
            libc::WEXITSTATUS(status)
        );
        let text = std::str::from_utf8(&answer).unwrap().trim_end();
        (!text.is_empty()).then(|| serde_json::from_str(text).unwrap())
    }

    /// `nobody` and its primary group, as most distributions define them.
    fn nobody() -> Option<(u32, u32, String)> {
        // SAFETY: test-only lookups; the returned records are copied at once.
        unsafe {
            let account = libc::getpwnam(c"nobody".as_ptr());
            if account.is_null() {
                return None;
            }
            let (uid, gid) = ((*account).pw_uid, (*account).pw_gid);
            let group = libc::getgrgid(gid);
            if group.is_null() {
                return None;
            }
            let name = std::ffi::CStr::from_ptr((*group).gr_name)
                .to_str()
                .ok()?
                .to_owned();
            Some((uid, gid, name))
        }
    }

    fn no_account(start: u32) -> u32 {
        (start..start + 1000)
            .find(|uid| account_groups(*uid).unwrap().is_none())
            .unwrap()
    }

    /// As root: other accounts connect from forked children with exactly the
    /// credentials each case needs. Skipped without root; GitLab CI runs it.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn accounts_are_judged_by_kernel_credentials_and_the_group_database() {
        if euid() != 0 {
            eprintln!("skipped: needs root to connect as other accounts");
            return;
        }
        let Some((nobody_uid, group_gid, group)) = nobody() else {
            eprintln!("skipped: this system defines no nobody account and group");
            return;
        };
        let stranger = no_account(4200);
        let unrelated_gid = no_account(5200);
        let scratch = Scratch::new("admin-socket-root");
        let (state, socket, config) = state_for(&scratch.0, &group);
        let server = start(&state, &socket).await;
        let ask = |uid: u32, gid: u32, groups: Vec<u32>, request: Request| {
            let socket = socket.clone();
            tokio::task::spawn_blocking(move || request_as(&socket, uid, gid, &groups, &request))
        };
        let switch = |enabled| {
            Request::new(RequestedAction::SetResource {
                resource: "memory".into(),
                enabled,
            })
        };
        let lending = |on| {
            Some(Response::Lending {
                lending_enabled: on,
            })
        };

        // Holds the group as its effective gid, and the database lists it
        // (its primary group): an administrator.
        assert_eq!(
            ask(nobody_uid, group_gid, vec![], switch(false))
                .await
                .unwrap(),
            Some(Response::Resource {
                resource: "memory".into(),
                enabled: false
            })
        );
        // The same account, from a process that does not hold the group.
        let standard = |request| ask(nobody_uid, unrelated_gid, vec![], request);
        assert_eq!(standard(switch(true)).await.unwrap(), Some(refused()));
        assert_eq!(
            standard(save_request(&config)).await.unwrap(),
            Some(refused())
        );
        assert!(!state.resource_enabled("memory"));
        // It may pause, and resume its own pause.
        assert_eq!(
            standard(Request::new(RequestedAction::Pause {}))
                .await
                .unwrap(),
            lending(false)
        );
        assert_eq!(
            *state.paused_by.lock().unwrap(),
            Some(PausedBy::Account(format!("uid:{nobody_uid}")))
        );
        // A uid with no account holds the group but is not listed: not an
        // administrator, and not the account that paused.
        assert_eq!(
            ask(
                stranger,
                group_gid,
                vec![group_gid],
                Request::new(RequestedAction::Resume {})
            )
            .await
            .unwrap(),
            Some(refused())
        );
        // Pausing again does not take the pause over.
        assert_eq!(
            ask(
                stranger,
                stranger,
                vec![],
                Request::new(RequestedAction::Pause {})
            )
            .await
            .unwrap(),
            lending(false)
        );
        assert_eq!(
            ask(
                stranger,
                stranger,
                vec![],
                Request::new(RequestedAction::Resume {})
            )
            .await
            .unwrap(),
            Some(refused())
        );
        assert_eq!(
            standard(Request::new(RequestedAction::Resume {}))
                .await
                .unwrap(),
            lending(true)
        );

        // Holding the group as a supplementary group counts too.
        let saved = ask(
            nobody_uid,
            unrelated_gid,
            vec![group_gid],
            save_request(&config),
        )
        .await
        .unwrap();
        assert!(
            matches!(
                saved,
                Some(Response::Saved {
                    restart_required: false,
                    ..
                })
            ),
            "{saved:?}"
        );

        // Root resumes anyone's pause and saves settings.
        assert_eq!(
            standard(Request::new(RequestedAction::Pause {}))
                .await
                .unwrap(),
            lending(false)
        );
        assert_eq!(
            send(&socket, Request::new(RequestedAction::Resume {})).await,
            Response::Lending {
                lending_enabled: true
            }
        );
        assert!(matches!(
            send(&socket, save_request(&config)).await,
            Response::Saved { .. }
        ));
        stop(server).await;
    }
}
