//! Binding and serving the API and proxy listeners.
//!
//! `axum::serve` gives hyper no timer, which leaves hyper's header-read
//! timeout inert: a client that opens a connection and never finishes its
//! request headers, or goes quiet on a keep-alive connection, holds a socket
//! for as long as it likes. With the ports on a LAN that is enough to exhaust
//! the process's descriptors and take the API and every proxy down together.
//!
//! [`serve`] runs the same axum router through hyper directly, with:
//!
//! - a timer and a header-read timeout, which also closes keep-alive
//!   connections that sit idle between requests. It never applies while a
//!   response is being written, so a long streamed generation is unaffected;
//! - a cap on open connections per listener. At the cap the listener stops
//!   accepting, so new clients wait in the kernel backlog instead of being
//!   reset, and proceed as soon as a slot frees;
//! - a request-body deadline and idle timeout, which start once the headers
//!   are complete. Without them a client that sends its headers and then
//!   withholds the body holds its connection slot for as long as it likes,
//!   and a handful of them fill the cap. An expired body is answered `408`;
//! - a write-stall timeout: a client that stops accepting response bytes
//!   while some are waiting is disconnected, so it cannot hold its slot, or
//!   borrowed work streaming to it, by not reading.
//!
//! [`bind`] retries an address that is not assigned yet. A Windows service
//! set to start automatically can start before DHCP or Wi-Fi has given the
//! machine the LAN address it is configured to listen on; failing there used
//! up the service's restart budget and left it stopped.

use std::future::Future;
use std::net::SocketAddr;
use std::pin::Pin;
use std::sync::{Arc, OnceLock};
use std::task::{Context as TaskContext, Poll};
use std::time::Duration;

use anyhow::{Context, Result};
use axum::body::Bytes;
use axum::extract::ConnectInfo;
use axum::response::{IntoResponse, Response};
use axum::Router;
use hyper::body::{Body as HttpBody, Frame, Incoming, SizeHint};
use hyper_util::rt::{TokioIo, TokioTimer};
use hyper_util::service::TowerToHyperService;
use tokio::io::{AsyncRead, AsyncWrite, ReadBuf};
use tokio::net::TcpListener;
use tokio::time::{Instant, Sleep};
use tower::ServiceExt;
use tracing::{debug, info, warn};

use crate::config::ListenerLimits;

/// The local address a request arrived on, attached to every request so the
/// loopback trust decision can compare the `Host` header with the real port.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ListenerAddr(pub SocketAddr);

/// First wait after a bind failure worth retrying; doubles up to the cap.
const BIND_RETRY_INITIAL: Duration = Duration::from_secs(1);
const BIND_RETRY_MAX: Duration = Duration::from_secs(30);
/// How long an address held by another socket is retried. Long enough for a
/// previous instance to finish exiting after a restart; short enough that a
/// genuine port conflict still fails where someone will see it.
const ADDR_IN_USE_BUDGET: Duration = Duration::from_secs(60);

/// Bind `listen`, retrying while the address is not usable yet.
///
/// An address that is not assigned to this machine is retried indefinitely,
/// with a warning each attempt, because a DHCP address can arrive at any
/// time after boot and after a lease change. The listener stays absent from
/// `/readyz` meanwhile. An address already in use is retried for about a
/// minute. Anything else, such as a permission error, fails at once.
pub async fn bind(what: &str, listen: &str) -> Result<TcpListener> {
    bind_with(what, listen, |address| async move {
        TcpListener::bind(address).await
    })
    .await
}

async fn bind_with<F, Fut>(what: &str, listen: &str, mut attempt: F) -> Result<TcpListener>
where
    F: FnMut(String) -> Fut,
    Fut: std::future::Future<Output = std::io::Result<TcpListener>>,
{
    let started = tokio::time::Instant::now();
    let mut delay = BIND_RETRY_INITIAL;
    let mut failures = 0u32;
    loop {
        let error = match attempt(listen.to_string()).await {
            Ok(listener) => {
                if failures > 0 {
                    info!(listener = what, listen, failures, "bound after retrying");
                }
                return Ok(listener);
            }
            Err(error) => error,
        };
        let retry = match error.kind() {
            std::io::ErrorKind::AddrNotAvailable => true,
            std::io::ErrorKind::AddrInUse => started.elapsed() < ADDR_IN_USE_BUDGET,
            _ => false,
        };
        if !retry {
            return Err(error).with_context(|| format!("{what}: binding {listen}"));
        }
        failures += 1;
        warn!(
            listener = what,
            listen,
            %error,
            retry_in_s = delay.as_secs(),
            "cannot listen yet; retrying (is the address assigned to this machine?)"
        );
        tokio::time::sleep(delay).await;
        delay = (delay * 2).min(BIND_RETRY_MAX);
    }
}

/// Serve `router` on `listener` until the task is aborted.
pub async fn serve(listener: TcpListener, router: Router, limits: &ListenerLimits) -> Result<()> {
    let local = listener.local_addr()?;
    let slots = Arc::new(tokio::sync::Semaphore::new(limits.max_connections));
    let mut http = hyper::server::conn::http1::Builder::new();
    http.timer(TokioTimer::new())
        .header_read_timeout(Duration::from_secs(limits.header_read_timeout_s));
    let body_limits = BodyLimits {
        total: Duration::from_secs(limits.body_read_timeout_s),
        idle: Duration::from_secs(limits.body_idle_timeout_s),
    };
    let write_stall = Duration::from_secs(limits.write_stall_timeout_s);
    let mut warned_full = false;
    loop {
        let permit = match slots.clone().try_acquire_owned() {
            Ok(permit) => permit,
            Err(_) => {
                if !warned_full {
                    warn!(
                        listen = %local,
                        max_connections = limits.max_connections,
                        "connection limit reached; new connections wait until one closes"
                    );
                    warned_full = true;
                }
                slots.clone().acquire_owned().await?
            }
        };
        if slots.available_permits() * 2 >= limits.max_connections {
            warned_full = false;
        }
        let (stream, peer) = match listener.accept().await {
            Ok(accepted) => accepted,
            Err(error) => {
                accept_failed(error).await;
                continue;
            }
        };
        let router = router.clone();
        let service = tower::service_fn(move |request: http::Request<Incoming>| {
            let expired = Arc::new(OnceLock::new());
            let (mut parts, body) = request.into_parts();
            parts.extensions.insert(ConnectInfo(peer));
            parts.extensions.insert(ListenerAddr(local));
            let request = http::Request::from_parts(
                parts,
                TimedBody::new(body, body_limits, expired.clone()),
            );
            let response = router.clone().oneshot(request);
            async move {
                let response = response.await?;
                // A handler that could not read the body because it expired
                // answers for a failed read; the client is told why.
                if let Some(timeout) = expired.get() {
                    if response.status() != http::StatusCode::REQUEST_TIMEOUT {
                        return Ok::<_, std::convert::Infallible>(body_timeout_response(
                            &timeout.to_string(),
                        ));
                    }
                }
                Ok(response)
            }
        });
        let connection = http
            .serve_connection(
                TokioIo::new(StallGuard::new(stream, write_stall)),
                TowerToHyperService::new(service),
            )
            .with_upgrades();
        tokio::spawn(async move {
            // Held until hyper finishes with the connection. An upgraded
            // (WebSocket) connection is handed off, and the proxy caps those
            // separately.
            let _permit = permit;
            if let Err(error) = connection.await {
                debug!(peer = %peer, %error, "connection closed with an error");
            }
        });
    }
}

/// How long a request body may take once the headers are complete.
#[derive(Debug, Clone, Copy)]
struct BodyLimits {
    total: Duration,
    idle: Duration,
}

/// A request body that did not arrive in time.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BodyTimeout {
    /// No body bytes arrived for this long.
    Idle(Duration),
    /// The whole body did not arrive within this long of the headers.
    Total(Duration),
}

impl std::fmt::Display for BodyTimeout {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Idle(limit) => write!(
                f,
                "no request body bytes arrived for {} seconds (server.limits.body_idle_timeout_s)",
                limit.as_secs()
            ),
            Self::Total(limit) => write!(
                f,
                "the request body did not arrive within {} seconds of its headers \
                 (server.limits.body_read_timeout_s)",
                limit.as_secs()
            ),
        }
    }
}

impl std::error::Error for BodyTimeout {}

impl BodyTimeout {
    /// The body timeout that ended a failed body read, if one did.
    pub fn find(error: &(dyn std::error::Error + 'static)) -> Option<BodyTimeout> {
        let mut current = Some(error);
        while let Some(error) = current {
            if let Some(timeout) = error.downcast_ref::<BodyTimeout>() {
                return Some(*timeout);
            }
            current = error.source();
        }
        None
    }
}

/// `408 Request Timeout`, closing the connection: the rest of the body may
/// still be on its way, and nothing else can be read from the connection.
pub fn body_timeout_response(message: &str) -> Response {
    let body = serde_json::json!({
        "error": {
            "message": message,
            "type": "request_body_timeout",
            "code": "request_body_timeout",
        }
    });
    (
        http::StatusCode::REQUEST_TIMEOUT,
        [
            (http::header::CONTENT_TYPE, "application/json"),
            (http::header::CONNECTION, "close"),
        ],
        body.to_string(),
    )
        .into_response()
}

/// A request body that fails with [`BodyTimeout`] when the client stops
/// sending it or takes too long. Timers only run while the body is being
/// read and nothing is ready, so data already received is never lost to
/// them.
struct TimedBody {
    inner: Incoming,
    limits: BodyLimits,
    deadline: Instant,
    last_progress: Instant,
    idle: Option<Pin<Box<Sleep>>>,
    total: Option<Pin<Box<Sleep>>>,
    expired: Arc<OnceLock<BodyTimeout>>,
}

impl TimedBody {
    fn new(inner: Incoming, limits: BodyLimits, expired: Arc<OnceLock<BodyTimeout>>) -> Self {
        let now = Instant::now();
        Self {
            inner,
            limits,
            deadline: now + limits.total,
            last_progress: now,
            idle: None,
            total: None,
            expired,
        }
    }

    fn expire(
        &mut self,
        timeout: BodyTimeout,
    ) -> Poll<Option<Result<Frame<Bytes>, axum::BoxError>>> {
        let _ = self.expired.set(timeout);
        Poll::Ready(Some(Err(Box::new(timeout))))
    }
}

impl HttpBody for TimedBody {
    type Data = Bytes;
    type Error = axum::BoxError;

    fn poll_frame(
        self: Pin<&mut Self>,
        cx: &mut TaskContext<'_>,
    ) -> Poll<Option<Result<Frame<Self::Data>, Self::Error>>> {
        let this = self.get_mut();
        match Pin::new(&mut this.inner).poll_frame(cx) {
            Poll::Ready(Some(Ok(frame))) => {
                this.last_progress = Instant::now();
                if let Some(idle) = this.idle.as_mut() {
                    idle.as_mut().reset(this.last_progress + this.limits.idle);
                }
                return Poll::Ready(Some(Ok(frame)));
            }
            Poll::Ready(Some(Err(error))) => return Poll::Ready(Some(Err(Box::new(error)))),
            Poll::Ready(None) => return Poll::Ready(None),
            Poll::Pending => {}
        }
        let idle_until = this.last_progress + this.limits.idle;
        let idle = this
            .idle
            .get_or_insert_with(|| Box::pin(tokio::time::sleep_until(idle_until)));
        if idle.as_mut().poll(cx).is_ready() {
            return this.expire(BodyTimeout::Idle(this.limits.idle));
        }
        let deadline = this.deadline;
        let total = this
            .total
            .get_or_insert_with(|| Box::pin(tokio::time::sleep_until(deadline)));
        if total.as_mut().poll(cx).is_ready() {
            return this.expire(BodyTimeout::Total(this.limits.total));
        }
        Poll::Pending
    }

    fn is_end_stream(&self) -> bool {
        self.inner.is_end_stream()
    }

    fn size_hint(&self) -> SizeHint {
        self.inner.size_hint()
    }
}

/// A client connection whose writes fail once the client has accepted no
/// bytes for `limit` while some were waiting to be sent. Reads, and time
/// spent with nothing to send, never count: a response that is slow to
/// produce is not the client's stall.
struct StallGuard<T> {
    io: T,
    limit: Duration,
    stalled: Option<Pin<Box<Sleep>>>,
}

impl<T> StallGuard<T> {
    fn new(io: T, limit: Duration) -> Self {
        Self {
            io,
            limit,
            stalled: None,
        }
    }

    /// Track a write attempt's outcome: progress clears the stall, and a
    /// write still blocked `limit` after it first blocked is an error.
    fn track<R>(
        &mut self,
        cx: &mut TaskContext<'_>,
        polled: Poll<std::io::Result<R>>,
    ) -> Poll<std::io::Result<R>> {
        if polled.is_ready() {
            self.stalled = None;
            return polled;
        }
        let limit = self.limit;
        let stalled = self
            .stalled
            .get_or_insert_with(|| Box::pin(tokio::time::sleep(limit)));
        if stalled.as_mut().poll(cx).is_ready() {
            return Poll::Ready(Err(std::io::Error::new(
                std::io::ErrorKind::TimedOut,
                format!(
                    "the client accepted no data for {} seconds (server.limits.write_stall_timeout_s)",
                    limit.as_secs()
                ),
            )));
        }
        Poll::Pending
    }
}

impl<T: AsyncRead + Unpin> AsyncRead for StallGuard<T> {
    fn poll_read(
        mut self: Pin<&mut Self>,
        cx: &mut TaskContext<'_>,
        buf: &mut ReadBuf<'_>,
    ) -> Poll<std::io::Result<()>> {
        Pin::new(&mut self.io).poll_read(cx, buf)
    }
}

impl<T: AsyncWrite + Unpin> AsyncWrite for StallGuard<T> {
    fn poll_write(
        mut self: Pin<&mut Self>,
        cx: &mut TaskContext<'_>,
        data: &[u8],
    ) -> Poll<std::io::Result<usize>> {
        let polled = Pin::new(&mut self.io).poll_write(cx, data);
        self.track(cx, polled)
    }

    fn poll_write_vectored(
        mut self: Pin<&mut Self>,
        cx: &mut TaskContext<'_>,
        bufs: &[std::io::IoSlice<'_>],
    ) -> Poll<std::io::Result<usize>> {
        let polled = Pin::new(&mut self.io).poll_write_vectored(cx, bufs);
        self.track(cx, polled)
    }

    fn is_write_vectored(&self) -> bool {
        self.io.is_write_vectored()
    }

    fn poll_flush(mut self: Pin<&mut Self>, cx: &mut TaskContext<'_>) -> Poll<std::io::Result<()>> {
        let polled = Pin::new(&mut self.io).poll_flush(cx);
        self.track(cx, polled)
    }

    fn poll_shutdown(
        mut self: Pin<&mut Self>,
        cx: &mut TaskContext<'_>,
    ) -> Poll<std::io::Result<()>> {
        let polled = Pin::new(&mut self.io).poll_shutdown(cx);
        self.track(cx, polled)
    }
}

/// Keep accepting after a failed accept. A per-connection failure is the
/// client's problem; anything else (usually descriptor exhaustion) is logged
/// and paced so the loop does not spin.
async fn accept_failed(error: std::io::Error) {
    use std::io::ErrorKind::{ConnectionAborted, ConnectionRefused, ConnectionReset};
    if matches!(
        error.kind(),
        ConnectionAborted | ConnectionRefused | ConnectionReset
    ) {
        return;
    }
    warn!(%error, "accept failed; pausing for one second");
    tokio::time::sleep(Duration::from_secs(1)).await;
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::routing::get;
    use futures_util::StreamExt;
    use std::sync::atomic::{AtomicU32, Ordering};
    use tokio::io::{AsyncReadExt, AsyncWriteExt};

    fn limits(header_read_timeout_s: u64, max_connections: usize) -> ListenerLimits {
        ListenerLimits {
            header_read_timeout_s,
            max_connections,
            ..ListenerLimits::default()
        }
    }

    async fn start(router: Router, limits: ListenerLimits) -> SocketAddr {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        tokio::spawn(async move { serve(listener, router, &limits).await });
        address
    }

    /// Read until the peer closes, or give up after `within`.
    async fn closed_within(stream: &mut tokio::net::TcpStream, within: Duration) -> bool {
        let mut buffer = [0u8; 1024];
        tokio::time::timeout(within, async {
            loop {
                match stream.read(&mut buffer).await {
                    Ok(0) | Err(_) => return,
                    Ok(_) => continue,
                }
            }
        })
        .await
        .is_ok()
    }

    #[tokio::test]
    async fn a_client_that_never_finishes_its_headers_is_disconnected() {
        let address = start(
            Router::new().route("/", get(|| async { "ok" })),
            limits(1, 8),
        )
        .await;
        let mut slow = tokio::net::TcpStream::connect(address).await.unwrap();
        slow.write_all(b"GET / HTTP/1.1\r\nHost: 127.0.0.1\r\n")
            .await
            .unwrap();
        assert!(closed_within(&mut slow, Duration::from_secs(5)).await);
    }

    #[tokio::test]
    async fn an_idle_keep_alive_connection_is_closed() {
        let address = start(
            Router::new().route("/", get(|| async { "ok" })),
            limits(1, 8),
        )
        .await;
        let mut client = tokio::net::TcpStream::connect(address).await.unwrap();
        client
            .write_all(b"GET / HTTP/1.1\r\nHost: 127.0.0.1\r\n\r\n")
            .await
            .unwrap();
        let mut response = [0u8; 256];
        let read = client.read(&mut response).await.unwrap();
        assert!(response[..read].starts_with(b"HTTP/1.1 200"));
        // The connection is kept alive for the next request, then closed once
        // it has sat idle past the header-read timeout.
        assert!(closed_within(&mut client, Duration::from_secs(5)).await);
    }

    #[tokio::test]
    async fn a_streamed_response_outlives_the_header_timeout() {
        // The timeout governs waiting for a request, never a response still
        // in flight: a long generation must keep streaming.
        let router = Router::new().route(
            "/stream",
            get(|| async {
                let chunks = futures_util::stream::unfold(0u8, |sent| async move {
                    if sent == 3 {
                        return None;
                    }
                    tokio::time::sleep(Duration::from_millis(1200)).await;
                    Some((Ok::<_, std::io::Error>(format!("chunk{sent}\n")), sent + 1))
                });
                axum::body::Body::from_stream(chunks)
            }),
        );
        // Waiting on a slow response is not a client stall either.
        let address = start(router, body_limits(1, 1, 1, 8)).await;
        let mut client = tokio::net::TcpStream::connect(address).await.unwrap();
        client
            .write_all(b"GET /stream HTTP/1.1\r\nHost: 127.0.0.1\r\nConnection: close\r\n\r\n")
            .await
            .unwrap();
        let mut body = Vec::new();
        tokio::time::timeout(Duration::from_secs(10), client.read_to_end(&mut body))
            .await
            .unwrap()
            .unwrap();
        let body = String::from_utf8(body).unwrap();
        assert!(body.contains("chunk2"), "{body}");
    }

    #[tokio::test]
    async fn connections_beyond_the_cap_wait_for_a_free_slot() {
        let address = start(
            Router::new().route("/", get(|| async { "ok" })),
            limits(10, 1),
        )
        .await;
        // The first connection takes the only slot and says nothing.
        let mut holder = tokio::net::TcpStream::connect(address).await.unwrap();
        // The second is accepted by the kernel but not served yet.
        let mut waiting = tokio::net::TcpStream::connect(address).await.unwrap();
        waiting
            .write_all(b"GET / HTTP/1.1\r\nHost: 127.0.0.1\r\nConnection: close\r\n\r\n")
            .await
            .unwrap();
        let mut response = Vec::new();
        let early = tokio::time::timeout(
            Duration::from_millis(500),
            waiting.read_to_end(&mut response),
        )
        .await;
        assert!(early.is_err(), "served beyond the connection cap");
        // Once the first connection is closed, the waiting one is served.
        holder.shutdown().await.unwrap();
        drop(holder);
        tokio::time::timeout(Duration::from_secs(10), waiting.read_to_end(&mut response))
            .await
            .unwrap()
            .unwrap();
        assert!(response.starts_with(b"HTTP/1.1 200"));
    }

    fn body_limits(
        idle: u64,
        total: u64,
        write_stall: u64,
        max_connections: usize,
    ) -> ListenerLimits {
        ListenerLimits {
            header_read_timeout_s: 1,
            body_idle_timeout_s: idle,
            body_read_timeout_s: total,
            write_stall_timeout_s: write_stall,
            max_connections,
            ..ListenerLimits::default()
        }
    }

    /// A router whose handler buffers the request body, as the API's JSON
    /// routes do, and answers with its length.
    fn echo_length() -> Router {
        Router::new().route(
            "/",
            axum::routing::post(|body: axum::body::Bytes| async move { body.len().to_string() }),
        )
    }

    #[tokio::test]
    async fn a_withheld_body_is_answered_408_on_any_router() {
        let address = start(echo_length(), body_limits(1, 5, 5, 1)).await;
        let mut stalled = tokio::net::TcpStream::connect(address).await.unwrap();
        stalled
            .write_all(b"POST / HTTP/1.1\r\nHost: 127.0.0.1\r\nContent-Length: 10\r\n\r\nab")
            .await
            .unwrap();
        let started = std::time::Instant::now();
        let mut response = Vec::new();
        tokio::time::timeout(Duration::from_secs(5), stalled.read_to_end(&mut response))
            .await
            .expect("a withheld body kept its connection")
            .unwrap();
        let response = String::from_utf8_lossy(&response);
        assert!(response.starts_with("HTTP/1.1 408"), "{response}");
        assert!(response.contains("request_body_timeout"), "{response}");
        assert!(started.elapsed() >= Duration::from_millis(900));
        // The only connection slot is free again.
        let mut next = tokio::net::TcpStream::connect(address).await.unwrap();
        next.write_all(b"POST / HTTP/1.1\r\nHost: 127.0.0.1\r\nContent-Length: 3\r\nConnection: close\r\n\r\nabc")
            .await
            .unwrap();
        let mut response = Vec::new();
        tokio::time::timeout(Duration::from_secs(5), next.read_to_end(&mut response))
            .await
            .unwrap()
            .unwrap();
        assert!(
            response.ends_with(b"\r\n\r\n3"),
            "{}",
            String::from_utf8_lossy(&response)
        );
    }

    #[tokio::test]
    async fn a_body_trickling_inside_the_idle_timeout_still_meets_the_deadline() {
        let address = start(echo_length(), body_limits(1, 2, 5, 8)).await;
        let mut client = tokio::net::TcpStream::connect(address).await.unwrap();
        client
            .write_all(b"POST / HTTP/1.1\r\nHost: 127.0.0.1\r\nContent-Length: 100\r\n\r\n")
            .await
            .unwrap();
        let started = std::time::Instant::now();
        let mut response = Vec::new();
        let mut buffer = [0u8; 1024];
        while started.elapsed() < Duration::from_secs(6) {
            let _ = client.write_all(b"x").await;
            match tokio::time::timeout(Duration::from_millis(300), client.read(&mut buffer)).await {
                Ok(Ok(0)) | Ok(Err(_)) => break,
                Ok(Ok(n)) => response.extend_from_slice(&buffer[..n]),
                Err(_) => {}
            }
        }
        let response = String::from_utf8_lossy(&response);
        assert!(response.starts_with("HTTP/1.1 408"), "{response}");
        assert!(response.contains("body_read_timeout_s"), "{response}");
        let elapsed = started.elapsed();
        assert!(
            elapsed >= Duration::from_millis(1800) && elapsed < Duration::from_secs(5),
            "{elapsed:?}"
        );
    }

    #[tokio::test]
    async fn a_body_inside_its_limits_is_unaffected() {
        let address = start(echo_length(), body_limits(1, 5, 5, 8)).await;
        let mut client = tokio::net::TcpStream::connect(address).await.unwrap();
        client
            .write_all(b"POST / HTTP/1.1\r\nHost: 127.0.0.1\r\nContent-Length: 6\r\nConnection: close\r\n\r\n")
            .await
            .unwrap();
        for part in [b"ab", b"cd", b"ef"] {
            tokio::time::sleep(Duration::from_millis(600)).await;
            client.write_all(part).await.unwrap();
        }
        let mut response = Vec::new();
        tokio::time::timeout(Duration::from_secs(5), client.read_to_end(&mut response))
            .await
            .unwrap()
            .unwrap();
        assert!(
            response.starts_with(b"HTTP/1.1 200"),
            "{}",
            String::from_utf8_lossy(&response)
        );
        assert!(response.ends_with(b"\r\n\r\n6"));
    }

    #[tokio::test]
    async fn a_client_that_stops_reading_is_disconnected() {
        // A response far larger than the socket buffers, to a client that
        // never reads it, on a one-connection listener.
        let router = Router::new()
            .route(
                "/big",
                get(|| async {
                    let chunk = axum::body::Bytes::from(vec![b'x'; 1024 * 1024]);
                    let chunks = futures_util::stream::repeat(chunk)
                        .take(256)
                        .map(Ok::<_, std::io::Error>);
                    axum::body::Body::from_stream(chunks)
                }),
            )
            .route("/", get(|| async { "ok" }));
        let address = start(router, body_limits(1, 5, 1, 1)).await;
        let mut stalled = tokio::net::TcpStream::connect(address).await.unwrap();
        stalled
            .write_all(b"GET /big HTTP/1.1\r\nHost: 127.0.0.1\r\n\r\n")
            .await
            .unwrap();
        tokio::time::sleep(Duration::from_millis(200)).await;
        // The next client is served once the stalled one is dropped.
        let mut next = tokio::net::TcpStream::connect(address).await.unwrap();
        next.write_all(b"GET / HTTP/1.1\r\nHost: 127.0.0.1\r\nConnection: close\r\n\r\n")
            .await
            .unwrap();
        let mut response = Vec::new();
        tokio::time::timeout(Duration::from_secs(8), next.read_to_end(&mut response))
            .await
            .expect("a client that stopped reading kept the only connection")
            .unwrap();
        assert!(response.starts_with(b"HTTP/1.1 200"));
        drop(stalled);
    }

    #[tokio::test]
    async fn requests_carry_the_peer_and_listener_addresses() {
        let router = Router::new().route(
            "/",
            get(
                |ConnectInfo(peer): ConnectInfo<SocketAddr>,
                 axum::Extension(ListenerAddr(local)): axum::Extension<ListenerAddr>| async move {
                    format!("{} {}", peer.ip(), local.port())
                },
            ),
        );
        let address = start(router, limits(5, 8)).await;
        let mut client = tokio::net::TcpStream::connect(address).await.unwrap();
        client
            .write_all(b"GET / HTTP/1.1\r\nHost: 127.0.0.1\r\nConnection: close\r\n\r\n")
            .await
            .unwrap();
        let mut response = Vec::new();
        client.read_to_end(&mut response).await.unwrap();
        let response = String::from_utf8(response).unwrap();
        assert!(
            response.ends_with(&format!("127.0.0.1 {}", address.port())),
            "{response}"
        );
    }

    #[tokio::test(start_paused = true)]
    async fn an_unassigned_address_is_retried_until_it_appears() {
        let attempts = AtomicU32::new(0);
        let listener = bind_with("test", "192.0.2.1:9811", |_| {
            let attempt = attempts.fetch_add(1, Ordering::SeqCst);
            async move {
                if attempt < 5 {
                    Err(std::io::ErrorKind::AddrNotAvailable.into())
                } else {
                    TcpListener::bind("127.0.0.1:0").await
                }
            }
        })
        .await
        .unwrap();
        assert!(listener.local_addr().is_ok());
        assert_eq!(attempts.load(Ordering::SeqCst), 6);
    }

    #[tokio::test(start_paused = true)]
    async fn a_real_unassigned_address_does_not_fail_the_bind() {
        // 192.0.2.0/24 is reserved for documentation and never assigned. The
        // bind keeps waiting instead of returning the error.
        let pending =
            tokio::time::timeout(Duration::from_secs(600), bind("test", "192.0.2.1:9811")).await;
        assert!(pending.is_err(), "bind gave up: {pending:?}");
    }

    #[tokio::test(start_paused = true)]
    async fn an_address_in_use_is_retried_briefly_then_reported() {
        let error = bind_with("test", "127.0.0.1:9811", |_| async {
            Err(std::io::ErrorKind::AddrInUse.into())
        })
        .await
        .unwrap_err();
        assert!(format!("{error:#}").contains("test: binding 127.0.0.1:9811"));
    }

    #[tokio::test]
    async fn other_bind_errors_fail_immediately() {
        let attempts = AtomicU32::new(0);
        let error = bind_with("test", "127.0.0.1:80", |_| {
            attempts.fetch_add(1, Ordering::SeqCst);
            async { Err(std::io::ErrorKind::PermissionDenied.into()) }
        })
        .await
        .unwrap_err();
        assert_eq!(attempts.load(Ordering::SeqCst), 1);
        assert!(format!("{error:#}").contains("binding 127.0.0.1:80"));
    }
}
