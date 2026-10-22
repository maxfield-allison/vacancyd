//! Network-facing hardening of the proxy: header hygiene, redirects, error
//! text, WebSocket tunnel limits and listener binding.
use super::*;
use std::sync::atomic::{AtomicUsize, Ordering};
use tokio::io::{AsyncReadExt, AsyncWriteExt};

fn state_for(target: &str, kind: &str, extra_server: &str) -> Arc<AppState> {
    let config = toml::from_str(&format!(
        r#"
[server]
listen = "127.0.0.1:0"
{extra_server}
[owner.gpu_pressure]
enabled = false
[[resource]]
id = "mem"
kind = "system-memory"
enabled = false
[[upstream]]
id = "u"
listen = "127.0.0.1:0"
target = "{target}"
kind = "{kind}"
resource = "mem"
"#
    ))
    .unwrap();
    Arc::new(AppState::new(config).unwrap())
}

async fn start_proxy(state: Arc<AppState>) -> SocketAddr {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    let upstream = state.config.upstreams[0].clone();
    tokio::spawn(serve_on(listener, state, upstream));
    address
}

async fn read_head(stream: &mut tokio::net::TcpStream) -> String {
    let mut bytes = Vec::new();
    let mut byte = [0u8; 1];
    while !bytes.ends_with(b"\r\n\r\n") {
        tokio::time::timeout(Duration::from_secs(5), stream.read_exact(&mut byte))
            .await
            .expect("head read timed out")
            .expect("head read failed");
        bytes.push(byte[0]);
    }
    String::from_utf8(bytes).unwrap().to_ascii_lowercase()
}

async fn exchange(address: SocketAddr, request: &str) -> String {
    let mut stream = tokio::net::TcpStream::connect(address).await.unwrap();
    stream.write_all(request.as_bytes()).await.unwrap();
    let mut response = Vec::new();
    tokio::time::timeout(Duration::from_secs(10), stream.read_to_end(&mut response))
        .await
        .expect("response timed out")
        .unwrap();
    String::from_utf8_lossy(&response).into_owned()
}

/// A one-shot upstream that records the request head it receives, reads
/// the body its `Content-Length` declares, and answers with `reply`. Counts
/// every connection it accepts.
async fn recording_upstream(
    reply: &'static str,
) -> (
    SocketAddr,
    tokio::sync::mpsc::UnboundedReceiver<String>,
    Arc<AtomicUsize>,
) {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    let (heads, received) = tokio::sync::mpsc::unbounded_channel();
    let connections = Arc::new(AtomicUsize::new(0));
    let counted = connections.clone();
    tokio::spawn(async move {
        loop {
            let (mut stream, _) = listener.accept().await.unwrap();
            counted.fetch_add(1, Ordering::SeqCst);
            let heads = heads.clone();
            tokio::spawn(async move {
                let head = read_head(&mut stream).await;
                // Read the body before replying and close the write side
                // cleanly: on Windows, closing a socket that still holds
                // unread request bytes sends a reset, which the proxy can see
                // before the reply.
                let length = head
                    .lines()
                    .find_map(|line| line.strip_prefix("content-length:"))
                    .and_then(|value| value.trim().parse::<usize>().ok())
                    .unwrap_or(0);
                let _ = heads.send(head);
                let mut body = vec![0u8; length];
                if stream.read_exact(&mut body).await.is_err() {
                    return;
                }
                let _ = stream.write_all(reply.as_bytes()).await;
                let _ = stream.shutdown().await;
            });
        }
    });
    (address, received, connections)
}

#[tokio::test]
async fn hop_by_hop_headers_and_proxy_credentials_stay_on_their_own_hop() {
    let (upstream, mut heads, _) = recording_upstream(
        "HTTP/1.1 200 OK\r\nContent-Length: 2\r\nConnection: close, x-upstream-hop\r\n\
         X-Upstream-Hop: 1\r\nKeep-Alive: timeout=5\r\nProxy-Authenticate: Basic\r\n\
         X-Kept-Reply: yes\r\n\r\nok",
    )
    .await;
    let proxy = start_proxy(state_for(&format!("http://{upstream}"), "ollama", "")).await;
    let port = proxy.port();
    let response = exchange(
        proxy,
        &format!(
            "GET /api/tags HTTP/1.1\r\nHost: 127.0.0.1:{port}\r\n\
             Connection: close, x-secret-hop\r\nX-Secret-Hop: 1\r\n\
             Proxy-Authorization: Basic c2VjcmV0\r\nKeep-Alive: 300\r\nTE: trailers\r\n\
             Proxy-Connection: keep-alive\r\nOrigin: http://127.0.0.1\r\nX-Kept: yes\r\n\r\n"
        ),
    )
    .await
    .to_ascii_lowercase();
    let head = heads.recv().await.unwrap();
    for gone in [
        "x-secret-hop",
        "proxy-authorization",
        "keep-alive",
        "te:",
        "proxy-connection",
    ] {
        assert!(!head.contains(gone), "{gone} reached the upstream:\n{head}");
    }
    // End-to-end headers, including Origin for the upstream's own browser
    // checks, still arrive.
    assert!(head.contains("x-kept: yes"), "{head}");
    assert!(head.contains("origin: http://127.0.0.1"), "{head}");

    assert!(response.starts_with("http/1.1 200"), "{response}");
    for gone in ["x-upstream-hop", "keep-alive", "proxy-authenticate"] {
        assert!(
            !response.contains(gone),
            "{gone} reached the borrower:\n{response}"
        );
    }
    assert!(response.contains("x-kept-reply: yes"), "{response}");
}

#[tokio::test]
async fn an_upstream_redirect_is_returned_to_the_borrower_not_followed() {
    let (upstream, _heads, connections) = recording_upstream(
        "HTTP/1.1 302 Found\r\nLocation: /landed\r\nContent-Length: 0\r\nConnection: close\r\n\r\n",
    )
    .await;
    let proxy = start_proxy(state_for(&format!("http://{upstream}"), "ollama", "")).await;
    let port = proxy.port();
    let response = exchange(
        proxy,
        &format!("GET /api/tags HTTP/1.1\r\nHost: 127.0.0.1:{port}\r\nConnection: close\r\n\r\n"),
    )
    .await;
    assert!(response.starts_with("HTTP/1.1 302"), "{response}");
    assert!(response.contains("location: /landed"), "{response}");
    assert_eq!(connections.load(Ordering::SeqCst), 1);
}

/// Serves `run_proxy_through_environment_proxy` in a child process, because
/// proxy environment variables are process-wide.
#[test]
fn forwarding_ignores_proxy_environment_variables() {
    let status = std::process::Command::new(std::env::current_exe().unwrap())
        .args([
            "--exact",
            "proxy::hardening_tests::run_proxy_through_environment_proxy",
            "--ignored",
            "--nocapture",
        ])
        .env("VACANCYD_TEST_CHILD", "1")
        .status()
        .unwrap();
    assert!(status.success());
}

#[tokio::test]
#[ignore = "run in a child process by forwarding_ignores_proxy_environment_variables"]
async fn run_proxy_through_environment_proxy() {
    assert_eq!(std::env::var("VACANCYD_TEST_CHILD").as_deref(), Ok("1"));
    // A "system proxy" that would receive the request if it were honoured.
    let (system_proxy, _, proxied) = recording_upstream(
        "HTTP/1.1 418 I'm a teapot\r\nContent-Length: 0\r\nConnection: close\r\n\r\n",
    )
    .await;
    let (upstream, _, direct) =
        recording_upstream("HTTP/1.1 200 OK\r\nContent-Length: 2\r\nConnection: close\r\n\r\nok")
            .await;
    for name in ["HTTP_PROXY", "http_proxy", "ALL_PROXY", "all_proxy"] {
        std::env::set_var(name, format!("http://{system_proxy}"));
    }
    for name in ["NO_PROXY", "no_proxy"] {
        std::env::remove_var(name);
    }
    let proxy = start_proxy(state_for(&format!("http://{upstream}"), "ollama", "")).await;
    let port = proxy.port();
    let response = exchange(
        proxy,
        &format!("GET /api/tags HTTP/1.1\r\nHost: 127.0.0.1:{port}\r\nConnection: close\r\n\r\n"),
    )
    .await;
    assert!(response.starts_with("HTTP/1.1 200"), "{response}");
    assert_eq!(direct.load(Ordering::SeqCst), 1);
    assert_eq!(proxied.load(Ordering::SeqCst), 0);
}

#[tokio::test]
async fn an_unreachable_upstream_is_reported_without_its_address_or_query() {
    // Other tests reach the same `warn!` callsite concurrently; see
    // `test_log` for why a thread-scoped subscriber could miss it.
    let log = crate::test_log::capture();
    // A port with nothing listening.
    let closed = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let target = closed.local_addr().unwrap();
    drop(closed);
    let state = state_for(&format!("http://{target}"), "ollama", "");
    let ctx = Arc::new(ProxyCtx {
        upstream: state.config.upstreams[0].clone(),
        state,
    });
    let request = Request::builder()
        .uri("/api/tags?token=QUERYSECRET")
        .header("host", "127.0.0.1:11435")
        .body(Body::empty())
        .unwrap();
    let response = handle(
        State(ctx),
        ConnectInfo("127.0.0.1:1234".parse().unwrap()),
        request,
    )
    .await;
    assert_eq!(response.status(), StatusCode::BAD_GATEWAY);
    let body = axum::body::to_bytes(response.into_body(), 4096)
        .await
        .unwrap();
    let body = String::from_utf8_lossy(&body);
    assert_eq!(body, "upstream u unreachable");
    let logged = log.contents();
    assert!(logged.contains("upstream unreachable"), "{logged}");
    assert!(!logged.contains("QUERYSECRET"), "{logged}");
    assert!(!logged.contains(&target.to_string()), "{logged}");
}

/// Read one response head, or give up after `within`.
async fn head_within(stream: &mut tokio::net::TcpStream, within: Duration) -> Option<String> {
    tokio::time::timeout(within, read_head(stream)).await.ok()
}

/// Read until the peer closes, returning what arrived, or `None` if it did
/// not close within `within`.
async fn until_closed(stream: &mut tokio::net::TcpStream, within: Duration) -> Option<String> {
    let mut response = Vec::new();
    let mut buffer = [0u8; 4096];
    tokio::time::timeout(within, async {
        loop {
            match stream.read(&mut buffer).await {
                Ok(0) | Err(_) => break,
                Ok(n) => response.extend_from_slice(&buffer[..n]),
            }
        }
    })
    .await
    .ok()?;
    Some(String::from_utf8_lossy(&response).into_owned())
}

/// A body-bearing request head for `path` on the proxy at `port`.
fn body_head(port: u16, path: &str, framing: &str) -> String {
    format!(
        "POST {path} HTTP/1.1\r\nHost: 127.0.0.1:{port}\r\nContent-Type: application/json\r\n\
         {framing}\r\n"
    )
}

#[tokio::test]
async fn the_request_body_limit_is_configurable() {
    let (upstream, _, connections) =
        recording_upstream("HTTP/1.1 200 OK\r\nContent-Length: 2\r\nConnection: close\r\n\r\nok")
            .await;
    let state = state_for(
        &format!("http://{upstream}"),
        "ollama",
        "[server.limits]\nmax_request_body_mib = 1",
    );
    let proxy = start_proxy(state.clone()).await;
    let port = proxy.port();
    // A declared length over the limit is refused before any of it is sent.
    let mut client = tokio::net::TcpStream::connect(proxy).await.unwrap();
    client
        .write_all(
            body_head(
                port,
                "/api/show",
                &format!("Content-Length: {}\r\n", 1024 * 1024 + 1),
            )
            .as_bytes(),
        )
        .await
        .unwrap();
    let head = head_within(&mut client, Duration::from_secs(5))
        .await
        .expect("an oversized declared body was not refused at once");
    assert!(head.starts_with("http/1.1 413"), "{head}");
    assert!(head.contains("connection: close\r\n"), "{head}");
    let mut rest = vec![0u8; 512];
    let n = client.read(&mut rest).await.unwrap();
    let rest = String::from_utf8_lossy(&rest[..n]);
    assert!(rest.contains("request_body_too_large"), "{rest}");
    assert!(rest.contains("max_request_body_mib"), "{rest}");

    // A chunked body is refused as soon as it passes the limit.
    let mut client = tokio::net::TcpStream::connect(proxy).await.unwrap();
    client
        .write_all(body_head(port, "/api/show", "Transfer-Encoding: chunked\r\n").as_bytes())
        .await
        .unwrap();
    let chunk = format!("{:x}\r\n{}\r\n", 256 * 1024, "x".repeat(256 * 1024));
    let mut refused = None;
    for _ in 0..8 {
        if client.write_all(chunk.as_bytes()).await.is_err() {
            break;
        }
        if let Some(head) = head_within(&mut client, Duration::from_millis(200)).await {
            refused = Some(head);
            break;
        }
    }
    let head = refused.expect("a chunked body over the limit was not refused");
    assert!(head.starts_with("http/1.1 413"), "{head}");
    assert_eq!(connections.load(Ordering::SeqCst), 0);
    // Every refused body returned its budget.
    let budget = &state.upstreams["u"].body_budget;
    assert_eq!(budget.available_permits(), 256 * 1024);

    // A body within the limit is forwarded whole.
    let body = "x".repeat(1024 * 1024);
    let response = exchange(
        proxy,
        &format!(
            "POST /api/show HTTP/1.1\r\nHost: 127.0.0.1:{port}\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
            body.len()
        ),
    )
    .await;
    assert!(response.starts_with("HTTP/1.1 200"), "{response}");
    assert_eq!(connections.load(Ordering::SeqCst), 1);
}

#[tokio::test]
async fn withheld_bodies_are_refused_and_free_their_connections() {
    // The reported case: two connections send complete headers and one body
    // byte on a two-connection listener, and a third request must not wait
    // on them.
    let (upstream, _, connections) =
        recording_upstream("HTTP/1.1 200 OK\r\nContent-Length: 2\r\nConnection: close\r\n\r\nok")
            .await;
    let state = state_for(
        &format!("http://{upstream}"),
        "ollama",
        "[server.limits]\nheader_read_timeout_s = 1\nmax_connections = 2\n\
         max_request_body_mib = 1\nbody_idle_timeout_s = 1\nbody_read_timeout_s = 3",
    );
    let proxy = start_proxy(state.clone()).await;
    let port = proxy.port();
    let mut stalled = Vec::new();
    for _ in 0..2 {
        let mut client = tokio::net::TcpStream::connect(proxy).await.unwrap();
        client
            .write_all(
                format!(
                    "{}{{",
                    body_head(
                        port,
                        "/api/chat",
                        &format!("Content-Length: {}\r\n", 1024 * 1024)
                    )
                )
                .as_bytes(),
            )
            .await
            .unwrap();
        stalled.push(client);
    }
    let started = std::time::Instant::now();
    let response = tokio::time::timeout(
        Duration::from_secs(8),
        exchange(
            proxy,
            &format!(
                "GET /api/tags HTTP/1.1\r\nHost: 127.0.0.1:{port}\r\nConnection: close\r\n\r\n"
            ),
        ),
    )
    .await
    .expect("a withheld body still blocked the listener");
    assert!(response.starts_with("HTTP/1.1 200"), "{response}");
    assert!(
        started.elapsed() < Duration::from_secs(5),
        "{:?}",
        started.elapsed()
    );
    for mut client in stalled {
        let response = until_closed(&mut client, Duration::from_secs(5))
            .await
            .expect("a withheld body kept its connection");
        assert!(response.starts_with("HTTP/1.1 408"), "{response}");
        assert!(response.contains("request_body_timeout"), "{response}");
        assert!(response.contains("body_idle_timeout_s"), "{response}");
    }
    // Only the discovery request reached the upstream, and every body
    // returned its budget.
    assert_eq!(connections.load(Ordering::SeqCst), 1);
    assert_eq!(
        state.upstreams["u"].body_budget.available_permits(),
        state.config.server.limits.buffered_body_kib()
    );
    assert_eq!(
        state
            .metrics
            .requests_total
            .with_label_values(&["u", "refused_body_timeout"])
            .get(),
        2
    );
}

#[tokio::test]
async fn a_body_that_keeps_trickling_is_cut_off_at_the_deadline() {
    let (upstream, _, connections) =
        recording_upstream("HTTP/1.1 200 OK\r\nContent-Length: 2\r\nConnection: close\r\n\r\nok")
            .await;
    let state = state_for(
        &format!("http://{upstream}"),
        "ollama",
        "[server.limits]\nbody_idle_timeout_s = 1\nbody_read_timeout_s = 2",
    );
    let proxy = start_proxy(state).await;
    let port = proxy.port();
    let mut client = tokio::net::TcpStream::connect(proxy).await.unwrap();
    client
        .write_all(body_head(port, "/api/chat", "Content-Length: 1000\r\n").as_bytes())
        .await
        .unwrap();
    let started = std::time::Instant::now();
    // One byte every 300 ms never idles long enough for the idle timeout.
    let mut refused = None;
    for _ in 0..30 {
        if client.write_all(b" ").await.is_err() {
            break;
        }
        if let Some(head) = head_within(&mut client, Duration::from_millis(300)).await {
            refused = Some(head);
            break;
        }
    }
    let head = refused.expect("a trickling body was never refused");
    let elapsed = started.elapsed();
    assert!(head.starts_with("http/1.1 408"), "{head}");
    assert!(
        elapsed >= Duration::from_millis(1800) && elapsed < Duration::from_secs(6),
        "{elapsed:?}"
    );
    assert_eq!(connections.load(Ordering::SeqCst), 0);
}

#[tokio::test]
async fn a_slow_body_within_its_limits_is_forwarded() {
    let (upstream, _, connections) =
        recording_upstream("HTTP/1.1 200 OK\r\nContent-Length: 2\r\nConnection: close\r\n\r\nok")
            .await;
    let state = state_for(
        &format!("http://{upstream}"),
        "ollama",
        "[server.limits]\nbody_idle_timeout_s = 1\nbody_read_timeout_s = 5",
    );
    let proxy = start_proxy(state).await;
    let port = proxy.port();
    let mut client = tokio::net::TcpStream::connect(proxy).await.unwrap();
    let body = br#"{"model":"m","prompt":"slow"}"#;
    client
        .write_all(
            body_head(
                port,
                "/api/show",
                &format!("Content-Length: {}\r\nConnection: close\r\n", body.len()),
            )
            .as_bytes(),
        )
        .await
        .unwrap();
    for part in body.chunks(8) {
        tokio::time::sleep(Duration::from_millis(400)).await;
        client.write_all(part).await.unwrap();
    }
    let response = until_closed(&mut client, Duration::from_secs(5))
        .await
        .unwrap();
    assert!(response.starts_with("HTTP/1.1 200"), "{response}");
    assert_eq!(connections.load(Ordering::SeqCst), 1);
}

#[tokio::test]
async fn buffered_bodies_share_a_bounded_budget_per_proxy() {
    let (upstream, _, connections) =
        recording_upstream("HTTP/1.1 200 OK\r\nContent-Length: 2\r\nConnection: close\r\n\r\nok")
            .await;
    let state = state_for(
        &format!("http://{upstream}"),
        "ollama",
        "[server.limits]\nmax_request_body_mib = 1\nmax_buffered_body_mib = 1\n\
         body_idle_timeout_s = 30\nbody_read_timeout_s = 60",
    );
    let budget = state.upstreams["u"].body_budget.clone();
    let proxy = start_proxy(state.clone()).await;
    let port = proxy.port();
    // A declared 768 KiB body reserves its share before it is sent.
    let mut holder = tokio::net::TcpStream::connect(proxy).await.unwrap();
    holder
        .write_all(
            body_head(
                port,
                "/api/show",
                &format!("Content-Length: {}\r\n", 768 * 1024),
            )
            .as_bytes(),
        )
        .await
        .unwrap();
    holder.write_all(b"{").await.unwrap();
    tokio::time::timeout(Duration::from_secs(5), async {
        while budget.available_permits() != 256 {
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    })
    .await
    .expect("the declared body did not reserve its share");
    // Another body that would pass the budget is refused at once, with a
    // time to retry.
    let mut refused = tokio::net::TcpStream::connect(proxy).await.unwrap();
    refused
        .write_all(
            body_head(
                port,
                "/api/show",
                &format!("Content-Length: {}\r\n", 512 * 1024),
            )
            .as_bytes(),
        )
        .await
        .unwrap();
    let head = head_within(&mut refused, Duration::from_secs(5))
        .await
        .expect("an unaffordable body was not refused at once");
    assert!(head.starts_with("http/1.1 503"), "{head}");
    assert!(head.contains("retry-after: 2\r\n"), "{head}");
    // The client may still send the body it declared; it is read and
    // dropped, so the refusal arrives intact, and the connection then closes.
    refused.write_all(&vec![b'x'; 512 * 1024]).await.unwrap();
    let rest = until_closed(&mut refused, Duration::from_secs(5))
        .await
        .expect("the refused connection stayed open");
    assert!(rest.contains("request_body_budget"), "{rest}");
    assert!(rest.contains("max_buffered_body_mib"), "{rest}");
    // A chunked body is refused when it grows past what is left.
    let mut chunked = tokio::net::TcpStream::connect(proxy).await.unwrap();
    chunked
        .write_all(body_head(port, "/api/show", "Transfer-Encoding: chunked\r\n").as_bytes())
        .await
        .unwrap();
    let chunk = format!("{:x}\r\n{}\r\n", 128 * 1024, "x".repeat(128 * 1024));
    let mut head = None;
    for _ in 0..8 {
        if chunked.write_all(chunk.as_bytes()).await.is_err() {
            break;
        }
        if let Some(reply) = head_within(&mut chunked, Duration::from_millis(200)).await {
            head = Some(reply);
            break;
        }
    }
    let head = head.expect("a chunked body past the budget was not refused");
    assert!(head.starts_with("http/1.1 503"), "{head}");
    assert_eq!(
        budget.available_permits(),
        256,
        "a refused body kept budget"
    );
    // Disconnecting returns the holder's share, and a full-size body then fits.
    drop(holder);
    tokio::time::timeout(Duration::from_secs(5), async {
        while budget.available_permits() != 1024 {
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    })
    .await
    .expect("a disconnected body kept its share");
    let body = "x".repeat(1024 * 1024);
    let response = exchange(
        proxy,
        &format!(
            "POST /api/show HTTP/1.1\r\nHost: 127.0.0.1:{port}\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
            body.len()
        ),
    )
    .await;
    assert!(response.starts_with("HTTP/1.1 200"), "{response}");
    assert_eq!(connections.load(Ordering::SeqCst), 1);
    assert_eq!(budget.available_permits(), 1024);
    assert_eq!(
        state
            .metrics
            .requests_total
            .with_label_values(&["u", "refused_body_budget"])
            .get(),
        2
    );
}

#[tokio::test]
async fn many_concurrent_partial_bodies_never_exceed_the_budget() {
    let (upstream, _, connections) =
        recording_upstream("HTTP/1.1 200 OK\r\nContent-Length: 2\r\nConnection: close\r\n\r\nok")
            .await;
    let state = state_for(
        &format!("http://{upstream}"),
        "ollama",
        "[server.limits]\nmax_connections = 64\nmax_request_body_mib = 1\n\
         max_buffered_body_mib = 2\nbody_idle_timeout_s = 2\nbody_read_timeout_s = 4",
    );
    let budget = state.upstreams["u"].body_budget.clone();
    let proxy = start_proxy(state.clone()).await;
    let port = proxy.port();
    // Forty clients each declare 200 KiB and send half of it: 8 MiB claimed
    // against a 2 MiB budget.
    let mut clients = Vec::new();
    for _ in 0..40 {
        let mut client = tokio::net::TcpStream::connect(proxy).await.unwrap();
        let mut request = body_head(
            port,
            "/api/chat",
            &format!("Content-Length: {}\r\n", 200 * 1024),
        )
        .into_bytes();
        request.extend(std::iter::repeat_n(b'x', 100 * 1024));
        client.write_all(&request).await.unwrap();
        clients.push(client);
        // The budget is never overdrawn.
        assert!(budget.available_permits() <= 2048);
    }
    let mut statuses = Vec::new();
    for mut client in clients {
        let response = until_closed(&mut client, Duration::from_secs(10))
            .await
            .expect("a partial body kept its connection");
        statuses.push(response.lines().next().unwrap_or_default().to_owned());
    }
    let refused_budget = statuses
        .iter()
        .filter(|s| s.starts_with("HTTP/1.1 503"))
        .count();
    let timed_out = statuses
        .iter()
        .filter(|s| s.starts_with("HTTP/1.1 408"))
        .count();
    // At most ten 200 KiB reservations fit in 2 MiB; everyone else is
    // refused at once, and those admitted time out.
    assert_eq!(refused_budget + timed_out, 40, "{statuses:?}");
    assert!((1..=10).contains(&timed_out), "{statuses:?}");
    assert_eq!(connections.load(Ordering::SeqCst), 0);
    assert_eq!(budget.available_permits(), 2048);
    assert_eq!(
        state
            .metrics
            .requests_total
            .with_label_values(&["u", "refused_body_budget"])
            .get() as usize,
        refused_budget
    );
}

/// An upstream that completes every WebSocket handshake and then stays
/// silent, recording each handshake's head.
async fn silent_websocket_upstream() -> (SocketAddr, tokio::sync::mpsc::UnboundedReceiver<String>) {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    let (heads, received) = tokio::sync::mpsc::unbounded_channel();
    tokio::spawn(async move {
        loop {
            let (mut stream, _) = listener.accept().await.unwrap();
            let heads = heads.clone();
            tokio::spawn(async move {
                let head = read_head(&mut stream).await;
                let _ = heads.send(head);
                stream
                    .write_all(
                        b"HTTP/1.1 101 Switching Protocols\r\nConnection: Upgrade\r\n\
                          Upgrade: websocket\r\nSec-WebSocket-Accept: test-value\r\n\r\n",
                    )
                    .await
                    .unwrap();
                // Hold the connection open, reading and discarding.
                let mut sink = [0u8; 64];
                while matches!(stream.read(&mut sink).await, Ok(n) if n > 0) {}
            });
        }
    });
    (address, received)
}

fn websocket_handshake(port: u16) -> String {
    format!(
        "GET /ws HTTP/1.1\r\nHost: 127.0.0.1:{port}\r\nConnection: keep-alive, Upgrade\r\n\
         Upgrade: websocket\r\nSec-WebSocket-Version: 13\r\nSec-WebSocket-Key: dGVzdA==\r\n\
         Proxy-Authorization: Basic c2VjcmV0\r\n\r\n"
    )
}

#[tokio::test]
async fn an_idle_websocket_tunnel_is_closed() {
    let (upstream, mut heads) = silent_websocket_upstream().await;
    let state = state_for(
        &format!("http://{upstream}"),
        "comfyui",
        "[server.limits]\ntunnel_idle_timeout_s = 2",
    );
    let proxy = start_proxy(state).await;
    let mut client = tokio::net::TcpStream::connect(proxy).await.unwrap();
    client
        .write_all(websocket_handshake(proxy.port()).as_bytes())
        .await
        .unwrap();
    assert!(read_head(&mut client).await.starts_with("http/1.1 101"));
    let head = heads.recv().await.unwrap();
    assert!(head.contains("upgrade: websocket"), "{head}");
    assert!(head.contains("connection: upgrade"), "{head}");
    assert!(!head.contains("proxy-authorization"), "{head}");
    // Traffic keeps the tunnel open past the idle limit...
    for _ in 0..6 {
        tokio::time::sleep(Duration::from_millis(500)).await;
        client.write_all(b"ping").await.unwrap();
    }
    let mut probe = [0u8; 1];
    assert!(
        tokio::time::timeout(Duration::from_millis(100), client.read(&mut probe))
            .await
            .is_err(),
        "an active tunnel was closed"
    );
    // ...and silence closes it.
    let mut buffer = [0u8; 16];
    let closed = tokio::time::timeout(Duration::from_secs(5), client.read(&mut buffer))
        .await
        .expect("idle tunnel stayed open");
    assert!(matches!(closed, Ok(0) | Err(_)), "{closed:?}");
}

#[tokio::test]
async fn websocket_tunnels_are_capped_per_listener() {
    let (upstream, _heads) = silent_websocket_upstream().await;
    let state = state_for(
        &format!("http://{upstream}"),
        "comfyui",
        "[server.limits]\nmax_connections = 1",
    );
    let proxy = start_proxy(state).await;
    let mut first = tokio::net::TcpStream::connect(proxy).await.unwrap();
    first
        .write_all(websocket_handshake(proxy.port()).as_bytes())
        .await
        .unwrap();
    assert!(read_head(&mut first).await.starts_with("http/1.1 101"));
    // The upgraded connection has left the listener's connection limit, so a
    // second connection is accepted, but the tunnel limit refuses it.
    let mut second = tokio::net::TcpStream::connect(proxy).await.unwrap();
    second
        .write_all(websocket_handshake(proxy.port()).as_bytes())
        .await
        .unwrap();
    let refused = read_head(&mut second).await;
    assert!(refused.starts_with("http/1.1 503"), "{refused}");
    drop(second);
    // Closing the first tunnel frees its slot.
    drop(first);
    let mut third = None;
    for _ in 0..50 {
        let mut attempt = tokio::net::TcpStream::connect(proxy).await.unwrap();
        attempt
            .write_all(websocket_handshake(proxy.port()).as_bytes())
            .await
            .unwrap();
        if read_head(&mut attempt).await.starts_with("http/1.1 101") {
            third = Some(attempt);
            break;
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    assert!(third.is_some(), "tunnel slot was never released");
}

#[tokio::test(start_paused = true)]
async fn a_proxy_whose_address_is_not_assigned_waits_and_reports_not_ready() {
    let config = toml::from_str(
        r#"
[server]
listen = "127.0.0.1:0"
[owner.gpu_pressure]
enabled = false
[[resource]]
id = "mem"
kind = "system-memory"
[[upstream]]
id = "lan"
listen = "192.0.2.1:11435"
target = "http://127.0.0.1:11434"
kind = "ollama"
resource = "mem"
"#,
    )
    .unwrap();
    let state = Arc::new(AppState::new(config).unwrap());
    let upstream = state.config.upstreams[0].clone();
    let serving =
        tokio::time::timeout(Duration::from_secs(600), serve(state.clone(), upstream)).await;
    assert!(serving.is_err(), "the proxy gave up: {serving:?}");
    assert!(state
        .readiness_failures()
        .contains(&"upstream 'lan' is not listening".to_string()));
}

/// A secret file for one test, removed when it ends.
struct SecretFile(std::path::PathBuf);

impl SecretFile {
    fn new(contents: &str) -> Self {
        let path = std::env::temp_dir().join(format!(
            "vacancyd-proxy-credential-{}",
            crate::setup::nonce().unwrap()
        ));
        std::fs::write(&path, contents).unwrap();
        Self(path)
    }
}

impl Drop for SecretFile {
    fn drop(&mut self) {
        let _ = std::fs::remove_file(&self.0);
    }
}

/// State for one upstream presenting a bearer credential read from `secret`.
fn credentialed_state(target: &str, kind: &str, secret: &SecretFile) -> Arc<AppState> {
    let config = toml::from_str(&format!(
        r#"
[server]
listen = "127.0.0.1:0"
[owner.gpu_pressure]
enabled = false
[[resource]]
id = "mem"
kind = "system-memory"
enabled = false
[[upstream]]
id = "u"
listen = "127.0.0.1:0"
target = "{target}"
kind = "{kind}"
resource = "mem"
[upstream.credential]
scheme = "bearer"
value_file = '{}'
"#,
        secret.0.display()
    ))
    .unwrap();
    Arc::new(AppState::new(config).unwrap())
}

/// Everything a reader could be shown about this state: `/status`, and the
/// `Debug` form of the configuration, the resolved credentials and the
/// upstream's clients.
fn shown(state: &AppState) -> String {
    let runtime = &state.upstreams["u"];
    format!(
        "{}\n{:?}\n{:?}\n{:?}\n{:?}\n{:?}",
        serde_json::to_string(&crate::api::status_body(state)).unwrap(),
        state.config,
        state.config.resolve_credentials().unwrap(),
        runtime.http,
        runtime.observer_http,
        runtime.credential,
    )
}

#[tokio::test]
async fn an_upstream_credential_is_sent_for_the_borrower_and_never_shown() {
    let log = crate::test_log::capture_at(tracing::level_filters::LevelFilter::DEBUG);
    let secret = SecretFile::new("fake-upstream-secret\n");
    let (upstream, mut heads, _) =
        recording_upstream("HTTP/1.1 200 OK\r\nContent-Length: 2\r\nConnection: close\r\n\r\nok")
            .await;
    let state = credentialed_state(&format!("http://{upstream}"), "ollama", &secret);
    let proxy = start_proxy(state.clone()).await;
    let port = proxy.port();
    let response = exchange(
        proxy,
        &format!(
            "GET /api/tags HTTP/1.1\r\nHost: 127.0.0.1:{port}\r\n\
             Authorization: Bearer borrower-token\r\nConnection: close\r\n\r\n"
        ),
    )
    .await;
    assert!(response.starts_with("HTTP/1.1 200"), "{response}");
    let head = heads.recv().await.unwrap();
    assert!(
        head.contains("authorization: bearer fake-upstream-secret"),
        "{head}"
    );
    assert!(!head.contains("borrower-token"), "{head}");
    assert_eq!(head.matches("authorization:").count(), 1, "{head}");
    assert!(!response.contains("fake-upstream-secret"), "{response}");

    // vacancyd's own calls to the upstream present it too.
    let runtime = &state.upstreams["u"];
    let _ = runtime
        .adapter
        .observe_models(
            &runtime.observer_http,
            state.config.upstreams[0].target.as_str(),
        )
        .unwrap()
        .await;
    let head = heads.recv().await.unwrap();
    assert!(head.starts_with("get /api/ps"), "{head}");
    assert!(
        head.contains("authorization: bearer fake-upstream-secret"),
        "{head}"
    );

    let shown = shown(&state);
    assert!(!shown.contains("fake-upstream-secret"), "{shown}");
    let logged = log.contents();
    assert!(logged.contains("proxy listening"), "{logged}");
    assert!(!logged.contains("fake-upstream-secret"), "{logged}");
}

#[tokio::test]
async fn a_websocket_upgrade_presents_the_upstream_credential() {
    let secret = SecretFile::new("fake-upstream-secret");
    let (upstream, mut heads) = silent_websocket_upstream().await;
    let state = credentialed_state(&format!("http://{upstream}"), "comfyui", &secret);
    let proxy = start_proxy(state).await;
    let mut client = tokio::net::TcpStream::connect(proxy).await.unwrap();
    let handshake = websocket_handshake(proxy.port()).replace(
        "\r\n\r\n",
        "\r\nAuthorization: Bearer borrower-token\r\n\r\n",
    );
    client.write_all(handshake.as_bytes()).await.unwrap();
    let reply = read_head(&mut client).await;
    assert!(reply.starts_with("http/1.1 101"), "{reply}");
    assert!(!reply.contains("fake-upstream-secret"), "{reply}");
    let head = heads.recv().await.unwrap();
    assert!(
        head.contains("authorization: bearer fake-upstream-secret"),
        "{head}"
    );
    assert!(!head.contains("borrower-token"), "{head}");
}

#[tokio::test]
async fn an_unreachable_credentialed_upstream_reports_nothing_secret() {
    let log = crate::test_log::capture();
    let secret = SecretFile::new("fake-upstream-secret");
    let closed = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let target = closed.local_addr().unwrap();
    drop(closed);
    let state = credentialed_state(&format!("http://{target}"), "ollama", &secret);
    let ctx = Arc::new(ProxyCtx {
        upstream: state.config.upstreams[0].clone(),
        state,
    });
    let request = Request::builder()
        .uri("/api/tags")
        .header("host", "127.0.0.1:11435")
        .body(Body::empty())
        .unwrap();
    let response = handle(
        State(ctx),
        ConnectInfo("127.0.0.1:1234".parse().unwrap()),
        request,
    )
    .await;
    assert_eq!(response.status(), StatusCode::BAD_GATEWAY);
    let body = axum::body::to_bytes(response.into_body(), 4096)
        .await
        .unwrap();
    assert_eq!(String::from_utf8_lossy(&body), "upstream u unreachable");
    let logged = log.contents();
    assert!(logged.contains("upstream unreachable"), "{logged}");
    assert!(!logged.contains("fake-upstream-secret"), "{logged}");
}

/// Configuration refuses URL credentials; this is the second line, for a
/// target that reached the daemon anyway. Nothing it prints may carry them.
#[tokio::test]
async fn url_credentials_that_bypass_validation_are_redacted_everywhere() {
    let log = crate::test_log::capture_at(tracing::level_filters::LevelFilter::DEBUG);
    let (upstream, mut heads, _) =
        recording_upstream("HTTP/1.1 200 OK\r\nContent-Length: 2\r\nConnection: close\r\n\r\nok")
            .await;
    let state = state_for(&format!("http://demo:fake-secret@{upstream}"), "ollama", "");
    let proxy = start_proxy(state.clone()).await;
    let port = proxy.port();
    let response = exchange(
        proxy,
        &format!("GET /api/tags HTTP/1.1\r\nHost: 127.0.0.1:{port}\r\nConnection: close\r\n\r\n"),
    )
    .await;
    assert!(response.starts_with("HTTP/1.1 200"), "{response}");
    assert!(!response.contains("fake-secret"), "{response}");
    let _ = heads.recv().await.unwrap();

    let status = serde_json::to_string(&crate::api::status_body(&state)).unwrap();
    assert!(!status.contains("fake-secret"), "{status}");
    assert!(
        status.contains(&format!("http://***@{upstream}/")),
        "{status}"
    );
    let debug = format!("{:?}", state.config);
    assert!(!debug.contains("fake-secret"), "{debug}");
    let logged = log.contents();
    assert!(logged.contains("proxy listening"), "{logged}");
    assert!(logged.contains("***@"), "{logged}");
    assert!(!logged.contains("fake-secret"), "{logged}");
}
