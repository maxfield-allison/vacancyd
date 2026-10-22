//! Linux client of the admin socket: `vacancyd admin-request`.
//!
//! The service identifies this process from the socket's peer credentials,
//! so the request carries no claim about who is asking. Before sending
//! anything, the client checks the other end in the same way. The server
//! must run as root, like the packaged service; or as this same account, like
//! a foreground `vacancyd run` started by the person now asking; or as the
//! account that owns the socket and its directory, when no other account can
//! write to that directory, like the container image's unprivileged service.

use std::path::Path;
use std::time::Duration;

use tokio::io::{AsyncWriteExt, BufReader};
use tokio::net::UnixStream;

use super::{read_frame, HelperExit, Request, Response, MAX_RESPONSE_BYTES};

/// Bounds one round trip, including connecting. A settings save validates
/// and writes the configuration, and the service may consult the user and
/// group databases, so this is longer than the service's own read timeout.
const SOCKET_TIMEOUT: Duration = Duration::from_secs(15);

/// Why a round trip failed before the service answered.
#[derive(Debug, PartialEq, Eq)]
pub enum SendError {
    /// The socket does not exist, is not a socket, or refused the connection.
    Unreachable,
    /// The process serving the socket is not root, not this account, and
    /// does not own the socket's private directory.
    NotService,
    /// The connection broke or the answer was unreadable.
    Unconfirmed,
}

impl SendError {
    pub fn message(&self) -> &'static str {
        match self {
            Self::Unreachable => "Cannot reach the local service's admin socket. Check that vacancyd is running, and pass --socket if it uses a non-default path.",
            Self::NotService => {
                "The admin socket is served by a process that is neither root, this account, nor the owner of the socket's private directory. Nothing was sent."
            }
            Self::Unconfirmed => {
                "Cannot confirm the change. Check current service status before trying again."
            }
        }
    }
}

/// Send one request on the admin socket at `socket` as the calling account.
pub async fn send(socket: &Path, request: &Request) -> Result<Response, SendError> {
    let stream = UnixStream::connect(socket)
        .await
        .map_err(|_| SendError::Unreachable)?;
    verify_server(socket, &stream)?;
    let mut line = serde_json::to_vec(request).map_err(|_| SendError::Unconfirmed)?;
    line.push(b'\n');
    let mut stream = BufReader::new(stream);
    stream
        .write_all(&line)
        .await
        .map_err(|_| SendError::Unconfirmed)?;
    let frame = read_frame(&mut stream, MAX_RESPONSE_BYTES)
        .await
        .map_err(|_| SendError::Unconfirmed)?
        .ok_or(SendError::Unconfirmed)?;
    serde_json::from_slice(&frame).map_err(|_| SendError::Unconfirmed)
}

/// The kernel records the listening process's credentials when it calls
/// `listen`, and reports them to every client that connects.
fn verify_server(socket: &Path, stream: &UnixStream) -> Result<(), SendError> {
    use std::os::unix::fs::{FileTypeExt, MetadataExt};
    let server = stream.peer_cred().map_err(|_| SendError::NotService)?;
    // SAFETY: geteuid has no preconditions and cannot fail.
    let me = unsafe { libc::geteuid() };
    // Only consulted when the server is neither root nor this account.
    let placement = || {
        let file = std::fs::symlink_metadata(socket).ok()?;
        let directory = std::fs::symlink_metadata(socket.parent()?).ok()?;
        (file.file_type().is_socket() && directory.is_dir()).then_some(Placement {
            socket_owner: file.uid(),
            directory_owner: directory.uid(),
            directory_mode: directory.mode(),
        })
    };
    if trusted_server(server.uid(), me, placement) {
        Ok(())
    } else {
        Err(SendError::NotService)
    }
}

/// Who owns the socket file and the directory holding it.
struct Placement {
    socket_owner: u32,
    directory_owner: u32,
    directory_mode: u32,
}

fn trusted_server(
    server_uid: u32,
    my_uid: u32,
    placement: impl FnOnce() -> Option<Placement>,
) -> bool {
    if server_uid == 0 || server_uid == my_uid {
        return true;
    }
    // An unprivileged service: trusted when the socket and its directory are
    // its own and nobody else can write there, so no other account could
    // have put a socket at this path.
    placement().is_some_and(|placement| {
        placement.socket_owner == server_uid
            && placement.directory_owner == server_uid
            && placement.directory_mode & 0o022 == 0
    })
}

/// Read the request from `path`, or from standard input when it is `-`
/// (for `docker exec -i`, where the request file would otherwise have to be
/// copied into the container).
fn read_request_from(path: &Path) -> std::io::Result<Request> {
    if path == Path::new("-") {
        super::read_request(std::io::stdin().lock())
    } else {
        super::read_request_file(path)
    }
}

/// `vacancyd admin-request` on Linux: send a request read from a file and
/// exit with its [`HelperExit`] code. The service's answer is printed to
/// standard output when it confirms the change, and a plain explanation to
/// standard error otherwise.
pub async fn run_helper(request_file: &Path, socket: &Path) -> i32 {
    let request = match read_request_from(request_file) {
        Ok(request) => request,
        Err(error) => {
            eprintln!(
                "vacancyd admin-request: cannot use {}: {error}",
                request_file.display()
            );
            return HelperExit::BadRequestFile.code();
        }
    };
    let response = match tokio::time::timeout(SOCKET_TIMEOUT, send(socket, &request)).await {
        Ok(Ok(response)) => response,
        Ok(Err(error)) => {
            eprintln!("vacancyd admin-request: {}", error.message());
            return HelperExit::Unconfirmed.code();
        }
        Err(_) => {
            eprintln!(
                "vacancyd admin-request: {}",
                SendError::Unconfirmed.message()
            );
            return HelperExit::Unconfirmed.code();
        }
    };
    let exit = HelperExit::from_response(&request.action, &response);
    match exit {
        HelperExit::Applied => {
            if let Ok(text) = serde_json::to_string(&response) {
                println!("{text}");
            }
        }
        HelperExit::Refused(reason) => {
            eprintln!(
                "vacancyd admin-request: refused ({}): {}",
                reason.as_str(),
                reason.message()
            );
        }
        HelperExit::Unconfirmed | HelperExit::BadRequestFile => {
            eprintln!(
                "vacancyd admin-request: {}",
                SendError::Unconfirmed.message()
            );
        }
    }
    exit.code()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn placed(
        socket_owner: u32,
        directory_owner: u32,
        directory_mode: u32,
    ) -> impl FnOnce() -> Option<Placement> {
        move || {
            Some(Placement {
                socket_owner,
                directory_owner,
                directory_mode,
            })
        }
    }

    #[test]
    fn root_this_account_or_the_owner_of_a_private_directory_may_serve() {
        let unknown = || None;
        assert!(trusted_server(0, 1000, unknown));
        assert!(trusted_server(1000, 1000, unknown));
        assert!(trusted_server(0, 0, unknown));
        assert!(!trusted_server(1001, 1000, unknown));
        assert!(!trusted_server(65532, 0, unknown));
        // The container image: an unprivileged service in its own directory.
        assert!(trusted_server(65532, 0, placed(65532, 65532, 0o40755)));
        assert!(trusted_server(65532, 1000, placed(65532, 65532, 0o40700)));
        // Someone else's directory, a shared directory, or a socket owned by
        // another account: an impostor could have put it there.
        assert!(!trusted_server(65532, 0, placed(65532, 0, 0o40755)));
        assert!(!trusted_server(65532, 0, placed(65532, 65532, 0o41777)));
        assert!(!trusted_server(65532, 0, placed(65532, 65532, 0o40775)));
        assert!(!trusted_server(65532, 0, placed(1000, 65532, 0o40755)));
    }

    #[test]
    fn placement_is_read_from_the_socket_and_its_directory() {
        // Read a real socket's placement, then judge it as if this process
        // were an unprivileged service (uid 4242) and the client another
        // account: the root and same-account shortcuts must not decide.
        use std::os::unix::fs::{MetadataExt, PermissionsExt};
        let directory = std::env::temp_dir().join(format!(
            "vacancyd-client-placement-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        std::fs::create_dir(&directory).unwrap();
        let socket = directory.join("admin.sock");
        let _listener = std::os::unix::net::UnixListener::bind(&socket).unwrap();
        // SAFETY: geteuid has no preconditions.
        let me = unsafe { libc::geteuid() };
        const SERVICE: u32 = 4242;
        let as_service = |uid: u32| if uid == me { SERVICE } else { uid };
        let placement = || {
            let file = std::fs::symlink_metadata(&socket).unwrap();
            let parent = std::fs::symlink_metadata(&directory).unwrap();
            Placement {
                socket_owner: as_service(file.uid()),
                directory_owner: as_service(parent.uid()),
                directory_mode: parent.mode(),
            }
        };
        std::fs::set_permissions(&directory, std::fs::Permissions::from_mode(0o755)).unwrap();
        let private = placement();
        assert!(trusted_server(SERVICE, SERVICE + 1, || Some(private)));
        std::fs::set_permissions(&directory, std::fs::Permissions::from_mode(0o777)).unwrap();
        let shared = placement();
        assert!(!trusted_server(SERVICE, SERVICE + 1, || Some(shared)));
        std::fs::remove_dir_all(&directory).unwrap();
    }

    #[test]
    fn requests_are_read_from_a_file_or_standard_input_alike() {
        let request = br#"{"protocol_version": 1, "action": {"kind": "pause"}}"#;
        assert_eq!(
            super::super::read_request(&request[..]).unwrap(),
            Request::new(super::super::RequestedAction::Pause {})
        );
        let oversized = vec![b' '; super::super::MAX_REQUEST_BYTES + 1];
        assert!(super::super::read_request(&oversized[..]).is_err());
        assert!(super::super::read_request(
            &br#"{"protocol_version": 2, "action": {"kind": "pause"}}"#[..]
        )
        .is_err());
    }

    #[tokio::test]
    async fn a_missing_socket_is_unreachable() {
        let missing = std::env::temp_dir().join(format!(
            "vacancyd-missing-admin-{}.sock",
            std::process::id()
        ));
        let request = Request::new(super::super::RequestedAction::Pause {});
        assert_eq!(
            send(&missing, &request).await.unwrap_err(),
            SendError::Unreachable
        );
    }
}
