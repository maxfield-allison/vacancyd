//! Route policy at the proxy: which requests are refused before anything is
//! forwarded or reserved, cross-site requests to a loopback listener, and
//! what an upstream redirect reveals.
use super::*;
use std::sync::atomic::{AtomicUsize, Ordering};
use tokio::io::{AsyncReadExt, AsyncWriteExt};

/// A lendable memory-only upstream of `kind` in front of `target`.
fn lendable_state(target: &str, kind: &str, upstream_extra: &str) -> AppState {
    let config = toml::from_str(&format!(
        r#"
[server]
listen = "127.0.0.1:0"
[owner.gpu_pressure]
enabled = false
[[resource]]
id = "mem"
kind = "system-memory"
[[upstream]]
id = "u"
listen = "127.0.0.1:0"
target = "{target}"
kind = "{kind}"
resource = "mem"
{upstream_extra}
"#
    ))
    .unwrap();
    let state = AppState::new(config).unwrap();
    state.mark_presence_sampled();
    state.resources["mem"]
        .capacity
        .write()
        .unwrap()
        .record_success(crate::plugin::probe::Capacity::default());
    state.resources["mem"]
        .probe_lendable
        .store(true, Ordering::Relaxed);
    state
}

fn ctx_for(state: AppState) -> Arc<ProxyCtx> {
    let state = Arc::new(state);
    Arc::new(ProxyCtx {
        upstream: state.config.upstreams[0].clone(),
        state,
    })
}

/// An upstream that answers every request with `reply` and counts the
/// connections it accepts.
async fn counting_upstream(reply: String) -> (SocketAddr, Arc<AtomicUsize>) {
    counting_upstream_at(|_| reply).await
}

/// As [`counting_upstream`], with a reply that can name the upstream's own
/// address.
async fn counting_upstream_at(
    reply: impl FnOnce(SocketAddr) -> String,
) -> (SocketAddr, Arc<AtomicUsize>) {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    let reply = reply(address);
    let connections = Arc::new(AtomicUsize::new(0));
    let counted = connections.clone();
    tokio::spawn(async move {
        loop {
            let (mut stream, _) = listener.accept().await.unwrap();
            counted.fetch_add(1, Ordering::SeqCst);
            let reply = reply.clone();
            tokio::spawn(async move {
                let mut head = Vec::new();
                let mut byte = [0u8; 1];
                while !head.ends_with(b"\r\n\r\n") {
                    if stream.read_exact(&mut byte).await.is_err() {
                        return;
                    }
                    head.push(byte[0]);
                }
                // Read the body before replying and close the write side
                // cleanly. On Windows, closing a socket that still holds
                // unread request bytes sends a reset, and the client can see
                // it before the reply.
                let length = String::from_utf8_lossy(&head)
                    .lines()
                    .find_map(|line| {
                        let (name, value) = line.split_once(':')?;
                        name.trim()
                            .eq_ignore_ascii_case("content-length")
                            .then(|| value.trim().parse::<usize>().ok())?
                    })
                    .unwrap_or(0);
                let mut body = vec![0u8; length];
                if stream.read_exact(&mut body).await.is_err() {
                    return;
                }
                let _ = stream.write_all(reply.as_bytes()).await;
                let _ = stream.shutdown().await;
            });
        }
    });
    (address, connections)
}

fn ok_reply() -> String {
    "HTTP/1.1 200 OK\r\nContent-Length: 2\r\nConnection: close\r\n\r\nok".into()
}

async fn send(ctx: &Arc<ProxyCtx>, method: &str, uri: &str, headers: &[(&str, &str)]) -> Response {
    let mut request = Request::builder().method(method).uri(uri);
    for (name, value) in headers {
        request = request.header(*name, *value);
    }
    handle(
        State(ctx.clone()),
        ConnectInfo("127.0.0.1:1234".parse().unwrap()),
        request.body(Body::from(r#"{"model":"m"}"#)).unwrap(),
    )
    .await
}

async fn error_of(response: Response) -> serde_json::Value {
    let body = axum::body::to_bytes(response.into_body(), 64 * 1024)
        .await
        .unwrap();
    serde_json::from_slice::<serde_json::Value>(&body).unwrap()["error"].clone()
}

#[tokio::test]
async fn an_unrecognized_mutation_is_refused_before_it_is_forwarded_or_reserved() {
    let (upstream, connections) = counting_upstream(ok_reply()).await;
    let ctx = ctx_for(lendable_state(&format!("http://{upstream}"), "ollama", ""));
    for (method, path) in [
        ("POST", "/api/future-inference"),
        ("PUT", "/api/chat"),
        ("DELETE", "/v1/chat/completions"),
        ("POST", "/api/signout"),
    ] {
        let response = send(&ctx, method, path, &[]).await;
        assert_eq!(
            response.status(),
            StatusCode::NOT_IMPLEMENTED,
            "{method} {path}"
        );
        let error = error_of(response).await;
        assert_eq!(error["type"], "unrecognized_route");
        let message = error["message"].as_str().unwrap();
        assert!(message.contains(&format!("{method} {path}")), "{message}");
        assert!(message.contains("/issues"), "{message}");
    }
    assert_eq!(connections.load(Ordering::SeqCst), 0);
    // Nothing is held for an operator to release.
    assert!(ctx.state.admission.lock().unwrap().snapshot().is_empty());
    assert_eq!(
        ctx.state
            .metrics
            .requests_total
            .with_label_values(&["u", "refused_route"])
            .get(),
        4
    );
    // A recognized inference route is still admitted and completes.
    let response = send(&ctx, "POST", "/v1/responses", &[]).await;
    assert_eq!(response.status(), StatusCode::OK);
    axum::body::to_bytes(response.into_body(), 1024)
        .await
        .unwrap();
    assert_eq!(connections.load(Ordering::SeqCst), 1);
}

#[tokio::test]
async fn model_store_changes_are_refused_unless_the_upstream_allows_management() {
    let (upstream, connections) = counting_upstream(ok_reply()).await;
    let denied = ctx_for(lendable_state(&format!("http://{upstream}"), "ollama", ""));
    for (method, path) in [
        ("DELETE", "/api/delete"),
        ("POST", "/api/pull"),
        ("POST", "/api/push"),
        ("POST", "/api/create"),
        ("POST", "/api/copy"),
        ("POST", "/api/blobs/sha256:29fdb92e57cf08"),
        // Dot segments resolve before forwarding, so they resolve first here.
        ("POST", "/v1/../api/pull"),
        ("POST", "/v1/%2e%2e/api/pull"),
    ] {
        let response = send(&denied, method, path, &[]).await;
        assert_eq!(response.status(), StatusCode::FORBIDDEN, "{method} {path}");
        let error = error_of(response).await;
        assert_eq!(error["type"], "management_denied");
        assert_eq!(error["setting"], "management");
        assert!(
            error["message"]
                .as_str()
                .unwrap()
                .contains("management = \"allow\""),
            "{error}"
        );
    }
    assert_eq!(connections.load(Ordering::SeqCst), 0);
    // Reading model metadata and the catalogue stays open.
    assert_eq!(
        send(&denied, "POST", "/api/show", &[]).await.status(),
        StatusCode::OK
    );
    assert_eq!(
        send(&denied, "GET", "/api/tags", &[]).await.status(),
        StatusCode::OK
    );
    assert_eq!(connections.load(Ordering::SeqCst), 2);

    let allowed = ctx_for(lendable_state(
        &format!("http://{upstream}"),
        "ollama",
        r#"management = "allow""#,
    ));
    let response = send(&allowed, "DELETE", "/api/delete", &[]).await;
    assert_eq!(response.status(), StatusCode::OK);
    assert_eq!(connections.load(Ordering::SeqCst), 3);
    // Allowed management is forwarded, not admitted: it holds nothing.
    assert!(allowed
        .state
        .admission
        .lock()
        .unwrap()
        .snapshot()
        .is_empty());
}

#[tokio::test]
async fn comfyui_owner_data_and_state_changes_need_management() {
    let (upstream, connections) = counting_upstream(ok_reply()).await;
    let ctx = ctx_for(lendable_state(&format!("http://{upstream}"), "comfyui", ""));
    for (method, path) in [
        ("GET", "/history"),
        ("GET", "/api/history?max_items=100"),
        ("GET", "/queue"),
        ("GET", "/userdata?dir=workflows"),
        ("GET", "/view/../history"),
        ("GET", "/%68istory"),
        ("GET", "/manager/reboot"),
        ("POST", "/interrupt"),
        ("POST", "/free"),
        ("POST", "/queue"),
        ("POST", "/upload/image"),
        ("POST", "/settings"),
        ("DELETE", "/userdata/workflows%2Fmine.json"),
    ] {
        let response = send(&ctx, method, path, &[]).await;
        assert_eq!(response.status(), StatusCode::FORBIDDEN, "{method} {path}");
    }
    assert_eq!(connections.load(Ordering::SeqCst), 0);
    // A borrower still collects its own results.
    for path in [
        "/history/0f2c5a1e-6b1d-4d6b-9d7e-3b1f0c2a9e11",
        "/view?filename=ComfyUI_00001_.png&type=output",
        "/object_info",
    ] {
        assert_eq!(
            send(&ctx, "GET", path, &[]).await.status(),
            StatusCode::OK,
            "{path}"
        );
    }
    assert_eq!(connections.load(Ordering::SeqCst), 3);
}

/// Treats every request, including a WebSocket handshake, as work.
struct GatedWebSocket;

impl crate::plugin::adapter::Adapter for GatedWebSocket {
    fn kind(&self) -> &'static str {
        "gated-websocket"
    }
    fn capabilities(&self) -> crate::plugin::adapter::AdapterCapabilities {
        crate::plugin::adapter::AdapterCapabilities::default()
    }
    fn classify(&self, _: &http::Method, _: &str) -> crate::plugin::adapter::RequestClass {
        crate::plugin::adapter::RequestClass::Work
    }
}

#[tokio::test]
async fn a_refused_websocket_upstream_connection_releases_the_reservation() {
    let closed = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = closed.local_addr().unwrap();
    drop(closed);
    let mut state = lendable_state(&format!("http://{address}"), "ollama", "");
    state.upstreams.get_mut("u").unwrap().adapter = Box::new(GatedWebSocket);
    let ctx = ctx_for(state);
    let response = send(
        &ctx,
        "GET",
        "/ws",
        &[
            ("connection", "Upgrade"),
            ("upgrade", "websocket"),
            ("sec-websocket-version", "13"),
            // The sample key from RFC 6455 section 1.3, not a credential.
            ("sec-websocket-key", "dGhlIHNhbXBsZSBub25jZQ=="), // gitleaks:allow
        ],
    )
    .await;
    assert_eq!(response.status(), StatusCode::BAD_GATEWAY);
    // Nothing reached the upstream, so nothing is held for an operator.
    assert!(ctx.state.admission.lock().unwrap().snapshot().is_empty());
}

async fn start_proxy(state: AppState) -> SocketAddr {
    let state = Arc::new(state);
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    let upstream = state.config.upstreams[0].clone();
    tokio::spawn(serve_on(listener, state, upstream));
    address
}

async fn exchange(address: SocketAddr, request: &str) -> String {
    let mut stream = tokio::net::TcpStream::connect(address).await.unwrap();
    stream.write_all(request.as_bytes()).await.unwrap();
    let mut response = Vec::new();
    tokio::time::timeout(Duration::from_secs(10), stream.read_to_end(&mut response))
        .await
        .expect("response timed out")
        .unwrap();
    String::from_utf8_lossy(&response).to_ascii_lowercase()
}

#[tokio::test]
async fn a_cross_site_form_post_to_a_loopback_proxy_is_not_trusted() {
    // The form a web page can submit to 127.0.0.1 without any preflight.
    let (upstream, connections) = counting_upstream(ok_reply()).await;
    let proxy = start_proxy(lendable_state(&format!("http://{upstream}"), "ollama", "")).await;
    let port = proxy.port();
    let form = |extra: &str| {
        format!(
            "POST /api/generate HTTP/1.1\r\nHost: 127.0.0.1:{port}\r\n\
             Content-Type: text/plain\r\nContent-Length: 2\r\n{extra}Connection: close\r\n\r\n{{}}"
        )
    };
    for extra in [
        "Origin: https://attacker.example\r\nSec-Fetch-Site: cross-site\r\n",
        "Origin: https://attacker.example\r\n",
        "Sec-Fetch-Site: cross-site\r\n",
        "Origin: null\r\n",
    ] {
        let response = exchange(proxy, &form(extra)).await;
        assert!(response.starts_with("http/1.1 403"), "{extra}: {response}");
    }
    assert_eq!(connections.load(Ordering::SeqCst), 0);
    // A local page keeps its trust and runs inference.
    let response = exchange(proxy, &form("Origin: http://localhost:3000\r\n")).await;
    assert!(response.starts_with("http/1.1 200"), "{response}");
    // So does a native client. (A discovery route, since the resource now
    // waits for a capacity sample taken after that job.)
    let response = exchange(
        proxy,
        &format!("GET /api/tags HTTP/1.1\r\nHost: 127.0.0.1:{port}\r\nConnection: close\r\n\r\n"),
    )
    .await;
    assert!(response.starts_with("http/1.1 200"), "{response}");
    assert_eq!(connections.load(Ordering::SeqCst), 2);
}

#[tokio::test]
async fn an_upstream_redirect_to_its_own_address_becomes_a_relative_path() {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let upstream = listener.local_addr().unwrap();
    drop(listener);
    let port = upstream.port();
    for (location, expected) in [
        (
            format!("http://{upstream}/landed?x=1"),
            "/landed?x=1".to_string(),
        ),
        (
            format!("http://localhost:{port}/other"),
            "/other".to_string(),
        ),
        (format!("http://{upstream}"), "/".to_string()),
        (
            "https://example.com/elsewhere".to_string(),
            "https://example.com/elsewhere".to_string(),
        ),
        (
            "/already/relative".to_string(),
            "/already/relative".to_string(),
        ),
        (
            format!("http://127.0.0.1:{}/other-port", port.wrapping_add(1)),
            format!("http://127.0.0.1:{}/other-port", port.wrapping_add(1)),
        ),
    ] {
        let mut headers = HeaderMap::new();
        headers.insert(axum::http::header::LOCATION, location.parse().unwrap());
        relativize_location(&mut headers, &format!("http://{upstream}"));
        assert_eq!(
            headers[axum::http::header::LOCATION],
            expected.as_str(),
            "{location}"
        );
    }
    // A target with a base path maps back to the borrower's own path.
    let mut headers = HeaderMap::new();
    headers.insert(
        axum::http::header::LOCATION,
        format!("http://{upstream}/base/next#part").parse().unwrap(),
    );
    relativize_location(&mut headers, &format!("http://{upstream}/base/"));
    assert_eq!(headers[axum::http::header::LOCATION], "/next#part");

    // Through the proxy, end to end.
    let (redirecting, _) = counting_upstream_at(|own| {
        format!(
            "HTTP/1.1 307 Temporary Redirect\r\nLocation: http://{own}/api/tags?page=2\r\n\
             Content-Length: 0\r\nConnection: close\r\n\r\n"
        )
    })
    .await;
    let proxy = start_proxy(lendable_state(
        &format!("http://{redirecting}"),
        "ollama",
        "",
    ))
    .await;
    let response = exchange(
        proxy,
        &format!(
            "GET /api/tags HTTP/1.1\r\nHost: 127.0.0.1:{}\r\nConnection: close\r\n\r\n",
            proxy.port()
        ),
    )
    .await;
    assert!(response.starts_with("http/1.1 307"), "{response}");
    assert!(
        response.contains("location: /api/tags?page=2\r\n"),
        "{response}"
    );
    assert!(!response.contains(&redirecting.to_string()), "{response}");
}

/// An upstream that answers every request with a body naming the owner's
/// prompt, as llama.cpp's `/slots` does, and counts connections.
fn owner_data_reply() -> String {
    let body = r#"[{"id":0,"is_processing":true,"prompt":"owner-secret-prompt"}]"#;
    format!(
        "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\n\
         Connection: close\r\n\r\n{body}",
        body.len()
    )
}

async fn body_text(response: Response) -> String {
    let body = axum::body::to_bytes(response.into_body(), 64 * 1024)
        .await
        .unwrap();
    String::from_utf8_lossy(&body).into_owned()
}

#[tokio::test]
async fn llamacpp_slot_prompts_and_server_state_are_not_borrower_reads() {
    for kind in ["llamacpp", "openai"] {
        let (upstream, connections) = counting_upstream(owner_data_reply()).await;
        let denied = ctx_for(lendable_state(&format!("http://{upstream}"), kind, ""));
        // Lending being open or closed makes no difference to a read.
        for (method, path) in [
            ("GET", "/slots"),
            ("HEAD", "/slots"),
            ("GET", "/slots?fail_on_no_slot=1"),
            ("GET", "/%73lots"),
            ("GET", "/metrics"),
            ("GET", "/props"),
            ("GET", "/lora-adapters"),
            ("GET", "/health?include_slots"),
            ("GET", "/x/../slots"),
            ("GET", "/"),
            ("GET", "/future-state"),
        ] {
            let response = send(&denied, method, path, &[]).await;
            assert_eq!(
                response.status(),
                StatusCode::FORBIDDEN,
                "{kind} {method} {path}"
            );
            let error = error_of(response).await;
            assert_eq!(error["type"], "management_denied", "{kind} {method} {path}");
            assert_eq!(error["setting"], "management");
            assert!(!error.to_string().contains("owner-secret-prompt"));
        }
        // An unlisted read names the reads a borrower does have.
        let error = error_of(send(&denied, "GET", "/future-state", &[]).await).await;
        let message = error["message"].as_str().unwrap();
        assert!(
            message.contains("/v1/models, /models, /health, /v1/health"),
            "{message}"
        );
        assert!(message.contains("management = \"allow\""), "{message}");
        // Other methods are never forwarded, whatever the policy.
        for path in ["/slots", "/v1/models", "/health"] {
            let response = send(&denied, "OPTIONS", path, &[]).await;
            assert_eq!(response.status(), StatusCode::NOT_IMPLEMENTED, "{path}");
            assert_eq!(error_of(response).await["type"], "unrecognized_route");
        }
        assert_eq!(connections.load(Ordering::SeqCst), 0, "{kind}");
        assert_eq!(
            denied
                .state
                .metrics
                .requests_total
                .with_label_values(&["u", "refused_management"])
                .get(),
            12,
            "{kind}"
        );
        // Discovery and health still pass through.
        for path in ["/health", "/v1/health", "/v1/models", "/models"] {
            assert_eq!(
                send(&denied, "GET", path, &[]).await.status(),
                StatusCode::OK,
                "{kind} {path}"
            );
        }
        assert_eq!(connections.load(Ordering::SeqCst), 4, "{kind}");

        // The owner can hand the borrower the server's state explicitly.
        let allowed = ctx_for(lendable_state(
            &format!("http://{upstream}"),
            kind,
            r#"management = "allow""#,
        ));
        let response = send(&allowed, "GET", "/slots", &[]).await;
        assert_eq!(response.status(), StatusCode::OK);
        assert!(body_text(response).await.contains("owner-secret-prompt"));
        assert_eq!(
            send(&allowed, "GET", "/future-state", &[]).await.status(),
            StatusCode::OK
        );
        // Allowing management never forwards an unrecognized method.
        assert_eq!(
            send(&allowed, "OPTIONS", "/slots", &[]).await.status(),
            StatusCode::NOT_IMPLEMENTED
        );
        assert_eq!(connections.load(Ordering::SeqCst), 6, "{kind}");
        assert!(allowed
            .state
            .admission
            .lock()
            .unwrap()
            .snapshot()
            .is_empty());
    }
}

#[tokio::test]
async fn every_adapter_refuses_unlisted_reads_and_unknown_methods() {
    for (kind, owner_read, listed) in [
        ("ollama", "/api/ps", "/api/tags"),
        ("openai", "/slots", "/v1/models"),
        ("llamacpp", "/slots", "/health"),
        ("comfyui", "/queue", "/object_info"),
        ("chatterbox", "/status", "/v1/audio/voices"),
    ] {
        let (upstream, connections) = counting_upstream(owner_data_reply()).await;
        let ctx = ctx_for(lendable_state(&format!("http://{upstream}"), kind, ""));
        for (method, path) in [
            ("GET", owner_read),
            ("HEAD", owner_read),
            ("GET", "/not-a-route"),
            ("HEAD", "/not-a-route"),
            ("GET", &format!("{listed}/")),
            ("GET", &format!("{listed}x")),
        ] {
            let response = send(&ctx, method, path, &[]).await;
            assert_eq!(
                response.status(),
                StatusCode::FORBIDDEN,
                "{kind} {method} {path}"
            );
        }
        for method in ["OPTIONS", "TRACE", "PROPFIND", "PUT", "PATCH"] {
            for path in [listed, "/not-a-route"] {
                let response = send(&ctx, method, path, &[]).await;
                assert_eq!(
                    response.status(),
                    StatusCode::NOT_IMPLEMENTED,
                    "{kind} {method} {path}"
                );
            }
        }
        assert_eq!(connections.load(Ordering::SeqCst), 0, "{kind}");
        for method in ["GET", "HEAD"] {
            assert_eq!(
                send(&ctx, method, listed, &[]).await.status(),
                StatusCode::OK,
                "{kind} {method} {listed}"
            );
        }
        assert_eq!(connections.load(Ordering::SeqCst), 2, "{kind}");
    }
}
