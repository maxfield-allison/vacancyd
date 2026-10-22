use super::*;
use axum::routing::{get, post};
use axum::Json;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::Mutex;

/// A ComfyUI `/prompt` stand-in. By default it behaves like a release before
/// 0.3.45, which ignores a submitted `prompt_id` and answers `job-123`.
#[derive(Default)]
struct ComfyStub {
    complete: AtomicBool,
    reply: AtomicUsize,
    submissions: AtomicUsize,
    history_reads: AtomicUsize,
    /// Each forwarded `/prompt` body, byte for byte, and its Content-Length.
    bodies: Mutex<Vec<(Vec<u8>, Option<String>)>>,
    /// Ids whose history shows them finished.
    finished: Mutex<Vec<String>>,
    /// Pending and running prompt ids. A prompt the stub acknowledges is
    /// queued before the reply, as ComfyUI does.
    pending: Mutex<Vec<String>>,
    running: Mutex<Vec<String>>,
    queue_reads: AtomicUsize,
    /// What the next `/queue` reads or job lookups do, one step per read:
    /// `"hide"` leaves out every prompt, `"fail"` answers 500. Then reads
    /// are truthful. `queue_reads` counts both kinds of read.
    script: Mutex<std::collections::VecDeque<&'static str>>,
    /// ComfyUI 0.6.0 and later: `GET /api/jobs/{id}` exists. Without it the
    /// route is missing and the tracker falls back to `/queue`.
    jobs: AtomicBool,
    deletes: Mutex<Vec<serde_json::Value>>,
    /// With `hold_reply`, the handler signals `entered` and waits for
    /// `release` before answering; `queue_early` queues the prompt before
    /// waiting (a reply lost on the wire), otherwise after (slow validation).
    hold_reply: AtomicBool,
    queue_early: AtomicBool,
    entered: tokio::sync::Notify,
    release: tokio::sync::Notify,
    /// The version `/system_stats` reports; none by default.
    version: Mutex<Option<&'static str>>,
}

impl ComfyStub {
    fn last_body(&self) -> serde_json::Value {
        serde_json::from_slice(&self.bodies.lock().unwrap().last().unwrap().0).unwrap()
    }
    fn assigned(&self) -> String {
        self.last_body()["prompt_id"].as_str().unwrap().to_owned()
    }
}

async fn fixture(timeout: u32) -> (Arc<ProxyCtx>, Arc<ComfyStub>, tokio::task::JoinHandle<()>) {
    fixture_with(timeout, "").await
}

async fn fixture_with(
    timeout: u32,
    policy: &str,
) -> (Arc<ProxyCtx>, Arc<ComfyStub>, tokio::task::JoinHandle<()>) {
    let fixture = Arc::new(ComfyStub::default());
    let app = Router::new()
        .route("/prompt", post(|State(s): State<Arc<ComfyStub>>, headers: HeaderMap, body: axum::body::Bytes| async move {
            s.submissions.fetch_add(1, Ordering::SeqCst);
            let length = headers.get("content-length").map(|v| v.to_str().unwrap().to_owned());
            s.bodies.lock().unwrap().push((body.to_vec(), length));
            let submitted: serde_json::Value = serde_json::from_slice(&body).unwrap();
            let own = submitted["prompt_id"].as_str().unwrap_or_default().to_owned();
            if s.hold_reply.load(Ordering::SeqCst) {
                if s.queue_early.load(Ordering::SeqCst) {
                    s.pending.lock().unwrap().push(own.clone());
                }
                s.entered.notify_one();
                s.release.notified().await;
                if !s.queue_early.load(Ordering::SeqCst) {
                    s.pending.lock().unwrap().push(own.clone());
                }
            }
            let validation = serde_json::json!({"error":{"type":"prompt_outputs_failed_validation",
                "message":"Prompt outputs failed validation", "details":"Missing checkpoint", "extra_info":{}},
                "node_errors":{}});
            let mode = s.reply.load(Ordering::SeqCst);
            if !s.hold_reply.load(Ordering::SeqCst) {
                match mode {
                    0 => s.pending.lock().unwrap().push("job-123".into()),
                    5 | 8 => s.pending.lock().unwrap().push(own.clone()),
                    _ => {}
                }
            }
            // The connection fails before any reply: 7 never queued the
            // prompt, 8 queued it under the submitted id. Unwinding the
            // connection's task closes it without a response, and
            // `resume_unwind` skips the panic hook, so nothing is printed.
            if matches!(mode, 7 | 8) {
                std::panic::resume_unwind(Box::new("the stub drops the connection"));
            }
            match mode {
                1 => (StatusCode::BAD_REQUEST, Json(validation)),
                2 => (StatusCode::OK, Json(serde_json::json!({}))),
                3 => (StatusCode::INTERNAL_SERVER_ERROR, Json(validation)),
                4 => (StatusCode::BAD_REQUEST, Json(serde_json::json!({"error":"gateway failed"}))),
                // 0.3.45 and later: the submitted id is the job's id.
                5 => (StatusCode::OK, Json(serde_json::json!({"prompt_id": own, "number":0, "node_errors":{}}))),
                // A reply too large to be a submission reply.
                6 => (StatusCode::OK, Json(serde_json::json!({"prompt_id": own, "padding": "x".repeat(2 << 20)}))),
                _ => (StatusCode::OK, Json(serde_json::json!({"prompt_id":"job-123", "number":0, "node_errors":{}}))),
            }
        }))
        .route("/history/{id}", get(|State(s): State<Arc<ComfyStub>>, axum::extract::Path(id): axum::extract::Path<String>| async move {
            s.history_reads.fetch_add(1, Ordering::SeqCst);
            let done = (id == "job-123" && s.complete.load(Ordering::SeqCst))
                || s.finished.lock().unwrap().contains(&id);
            Json(if done {
                serde_json::json!({id: {"status":{"status_str":"success", "completed":true}, "outputs":{}}})
            } else { serde_json::json!({}) })
        }))
        .route("/api/jobs/{id}", get(|State(s): State<Arc<ComfyStub>>, axum::extract::Path(id): axum::extract::Path<String>| async move {
            use axum::response::IntoResponse;
            if !s.jobs.load(Ordering::SeqCst) {
                return (StatusCode::NOT_FOUND, [("content-type", "text/plain; charset=utf-8")], "404: Not Found").into_response();
            }
            s.queue_reads.fetch_add(1, Ordering::SeqCst);
            let step = s.script.lock().unwrap().pop_front();
            if step == Some("fail") {
                return (StatusCode::INTERNAL_SERVER_ERROR, Json(serde_json::json!({"error": "boom"}))).into_response();
            }
            let status = if step == Some("hide") {
                None
            } else if s.finished.lock().unwrap().contains(&id) || (id == "job-123" && s.complete.load(Ordering::SeqCst)) {
                Some("completed")
            } else if s.running.lock().unwrap().contains(&id) {
                Some("in_progress")
            } else if s.pending.lock().unwrap().contains(&id) {
                Some("pending")
            } else {
                None
            };
            match status {
                Some(status) => Json(serde_json::json!({"id": id, "status": status})).into_response(),
                None => (StatusCode::NOT_FOUND, Json(serde_json::json!({"error": "Job not found"}))).into_response(),
            }
        }))
        .route("/system_stats", get(|State(s): State<Arc<ComfyStub>>| async move {
            let version = *s.version.lock().unwrap();
            Json(match version {
                Some(version) => serde_json::json!({"system": {"comfyui_version": version}}),
                None => serde_json::json!({"system": {}}),
            })
        }))
        .route("/queue", get(|State(s): State<Arc<ComfyStub>>| async move {
            s.queue_reads.fetch_add(1, Ordering::SeqCst);
            let step = s.script.lock().unwrap().pop_front();
            if step == Some("fail") {
                return (StatusCode::INTERNAL_SERVER_ERROR, Json(serde_json::json!(null)));
            }
            let hide = step == Some("hide");
            let list = |ids: &Mutex<Vec<String>>| -> Vec<serde_json::Value> {
                if hide {
                    return Vec::new();
                }
                ids.lock().unwrap().iter().map(|id| serde_json::json!([0, id, {}, {}, []])).collect()
            };
            (StatusCode::OK, Json(serde_json::json!({"queue_running": list(&s.running), "queue_pending": list(&s.pending)})))
        }).post(|State(s): State<Arc<ComfyStub>>, Json(body): Json<serde_json::Value>| async move {
            if body["clear"] == true {
                s.pending.lock().unwrap().clear();
            }
            for id in body["delete"].as_array().into_iter().flatten() {
                s.pending.lock().unwrap().retain(|p| p != id.as_str().unwrap());
            }
            s.deletes.lock().unwrap().push(body);
            StatusCode::OK
        }))
        .with_state(fixture.clone());
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let target = listener.local_addr().unwrap();
    let server = tokio::spawn(async move {
        axum::serve(listener, app).await.unwrap();
    });
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
        id = "comfy"
        listen = "127.0.0.1:0"
        target = "http://{target}"
        kind = "comfyui"
        resource = "mem"
        completion_timeout_s = {timeout}
        {policy}
    "#
    ))
    .unwrap();
    let state = Arc::new(AppState::new(config).unwrap());
    state.mark_presence_sampled();
    sample(&state);
    state.upstreams["comfy"]
        .models
        .write()
        .unwrap()
        .record_success(crate::lifecycle::Models::default());
    let ctx = Arc::new(ProxyCtx {
        upstream: state.config.upstreams[0].clone(),
        state,
    });
    (ctx, fixture, server)
}

fn sample(state: &AppState) {
    state.resources["mem"]
        .capacity
        .write()
        .unwrap()
        .record_success(crate::plugin::probe::Capacity::default());
}

async fn submit(ctx: Arc<ProxyCtx>) -> Response {
    submit_body(ctx, r#"{"prompt":{}}"#).await
}

async fn submit_body(ctx: Arc<ProxyCtx>, body: &'static str) -> Response {
    handle(
        State(ctx),
        ConnectInfo("127.0.0.1:1234".parse().unwrap()),
        Request::builder()
            .method("POST")
            .uri("/prompt")
            .header("content-length", body.len())
            .body(Body::from(body))
            .unwrap(),
    )
    .await
}

fn work(ctx: &ProxyCtx) -> Vec<vacancyd::status::Work> {
    ctx.state.admission.lock().unwrap().snapshot()
}

async fn wait_for(what: &str, done: impl Fn() -> bool) {
    // A hang guard, not a timing claim: tracking and cancellation take as
    // long as the host's load and the size of what they read make them.
    tokio::time::timeout(Duration::from_secs(60), async {
        while !done() {
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .unwrap_or_else(|_| panic!("timed out waiting until {what}"));
}

#[tokio::test]
async fn queued_work_stays_reserved_after_client_disconnect_until_matching_completion() {
    let (ctx, fixture, server) = fixture(30).await;
    let response = submit(ctx.clone()).await;
    assert_eq!(response.status(), StatusCode::OK);
    drop(response); // The borrower never consumes the submission reply.
    assert_eq!(
        submit(ctx.clone()).await.status(),
        StatusCode::SERVICE_UNAVAILABLE
    );
    assert_eq!(fixture.submissions.load(Ordering::SeqCst), 1);
    fixture.complete.store(true, Ordering::SeqCst);
    tokio::time::timeout(Duration::from_secs(4), async {
        while !ctx.state.admission.lock().unwrap().snapshot().is_empty() {
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .unwrap();
    // Completion does not turn the pre-job capacity reading into a new reading.
    assert_eq!(
        submit(ctx.clone()).await.status(),
        StatusCode::SERVICE_UNAVAILABLE
    );
    sample(&ctx.state);
    assert_eq!(submit(ctx.clone()).await.status(), StatusCode::OK);
    assert!(fixture.history_reads.load(Ordering::SeqCst) > 0);
    server.abort();
}

#[tokio::test]
async fn tracker_deadline_and_missing_history_leave_an_explicit_operator_hold() {
    let (ctx, _, server) = fixture(1).await;
    assert_eq!(submit(ctx.clone()).await.status(), StatusCode::OK);
    tokio::time::timeout(Duration::from_secs(3), async {
        loop {
            if ctx.state.admission.lock().unwrap().snapshot()[0].phase == "held" {
                break;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .unwrap();
    sample(&ctx.state);
    assert_eq!(
        submit(ctx.clone()).await.status(),
        StatusCode::SERVICE_UNAVAILABLE
    );
    let snapshot = ctx.state.admission.lock().unwrap().snapshot();
    assert_eq!(snapshot[0].tracking_id.as_deref(), Some("job-123"));
    ctx.state
        .admission
        .lock()
        .unwrap()
        .release(snapshot[0].id)
        .unwrap();
    assert_eq!(
        submit(ctx.clone()).await.status(),
        StatusCode::SERVICE_UNAVAILABLE
    );
    server.abort();
}

#[tokio::test]
async fn stale_queue_observation_cannot_authorize_a_submission() {
    let (ctx, fixture, server) = fixture(1).await;
    ctx.state.upstreams["comfy"]
        .models
        .write()
        .unwrap()
        .record_success_at(
            crate::lifecycle::Models::default(),
            std::time::Instant::now() - Duration::from_secs(20),
        );
    assert_eq!(
        submit(ctx.clone()).await.status(),
        StatusCode::SERVICE_UNAVAILABLE
    );
    assert_eq!(fixture.submissions.load(Ordering::SeqCst), 0);
    server.abort();
}

#[tokio::test]
async fn verified_validation_rejection_releases_only_after_fresh_capacity() {
    let (ctx, fixture, server) = fixture(30).await;
    fixture.reply.store(1, Ordering::SeqCst);
    assert_eq!(submit(ctx.clone()).await.status(), StatusCode::BAD_REQUEST);
    assert!(ctx.state.admission.lock().unwrap().snapshot().is_empty());
    assert_eq!(
        submit(ctx.clone()).await.status(),
        StatusCode::SERVICE_UNAVAILABLE
    );
    assert_eq!(fixture.submissions.load(Ordering::SeqCst), 1);
    sample(&ctx.state);
    fixture.reply.store(0, Ordering::SeqCst);
    fixture.complete.store(true, Ordering::SeqCst);
    assert_eq!(submit(ctx.clone()).await.status(), StatusCode::OK);
    server.abort();
}

/// A reply that does not name the job cannot release it, and cannot lose
/// it either: the id vacancyd assigned is followed until the upstream's
/// history shows a job under it ended. An older ComfyUI that ignored the
/// assigned id never shows one, so the reservation stays until an operator
/// releases it.
#[tokio::test]
async fn ambiguous_submission_replies_follow_the_assigned_id() {
    for (mode, expected) in [
        (2, StatusCode::OK),
        (3, StatusCode::INTERNAL_SERVER_ERROR),
        (4, StatusCode::BAD_REQUEST),
        (6, StatusCode::BAD_GATEWAY),
    ] {
        for honored in [true, false] {
            // Tracking expiry needs a short tracking bound, which also bounds
            // the submission exchange; the 2 MiB reply of mode 6 could outlast
            // it on a starved host. Mode 6 is covered where history ends
            // tracking, with a bound that never matters.
            if mode == 6 && !honored {
                continue;
            }
            let (ctx, fixture, server) = fixture(if honored { 30 } else { 2 }).await;
            fixture.reply.store(mode, Ordering::SeqCst);
            let response = submit(ctx.clone()).await;
            assert_eq!(response.status(), expected, "mode {mode}");
            let assigned = fixture.assigned();
            if mode == 6 {
                let body = axum::body::to_bytes(response.into_body(), 4096)
                    .await
                    .unwrap();
                let body = String::from_utf8_lossy(&body);
                assert!(body.contains(&assigned), "{body}");
            }
            let entry = work(&ctx);
            assert_eq!(entry.len(), 1);
            assert_eq!(entry[0].phase, "tracking", "mode {mode}");
            assert_eq!(entry[0].tracking_id.as_deref(), Some(assigned.as_str()));
            sample(&ctx.state);
            assert_eq!(
                submit(ctx.clone()).await.status(),
                StatusCode::SERVICE_UNAVAILABLE
            );
            assert_eq!(fixture.submissions.load(Ordering::SeqCst), 1);
            if honored {
                fixture.finished.lock().unwrap().push(assigned);
                wait_for("history releases the job", || work(&ctx).is_empty()).await;
            } else {
                wait_for("the tracking bound holds the job", || {
                    work(&ctx).first().is_some_and(|w| w.phase == "held")
                })
                .await;
            }
            server.abort();
        }
    }
}

/// vacancyd names the job before forwarding. The borrower's workflow is
/// forwarded byte for byte with vacancyd's `prompt_id` appended as the last
/// member, which Python's JSON decoder keeps over one the borrower set.
#[tokio::test]
async fn the_forwarded_prompt_carries_the_assigned_id_in_place_of_the_borrowers() {
    const BODY: &str = "{\"prompt\":{\"3\":{\"inputs\":{\"seed\":18446744073709551616,\"cfg\":7.50}}},\n \"prompt_id\":\"0f2c5a1e-6b1d-4d6b-9d7e-3b1f0c2a9e11\", \"client_id\":\"c\"}\n";
    let (ctx, fixture, server) = fixture(30).await;
    fixture.reply.store(5, Ordering::SeqCst);
    let response = submit_body(ctx.clone(), BODY).await;
    assert_eq!(response.status(), StatusCode::OK);
    let (forwarded, length) = fixture.bodies.lock().unwrap()[0].clone();
    let assigned = fixture.assigned();
    assert_ne!(assigned, "0f2c5a1e-6b1d-4d6b-9d7e-3b1f0c2a9e11");
    assert!(crate::admission::is_safe_tracking_id(&assigned));
    let original = BODY.trim_end().strip_suffix('}').unwrap();
    assert_eq!(
        String::from_utf8(forwarded.clone()).unwrap(),
        format!("{original},\"prompt_id\":\"{assigned}\"}}")
    );
    // The length is the forwarded body's, not the borrower's.
    assert_eq!(length, Some(forwarded.len().to_string()));
    // The reply names the assigned id, which is now confirmed and tracked.
    let reply = axum::body::to_bytes(response.into_body(), 4096)
        .await
        .unwrap();
    assert!(String::from_utf8_lossy(&reply).contains(&assigned));
    let entry = work(&ctx);
    assert_eq!(entry[0].phase, "tracking");
    assert_eq!(entry[0].tracking_id.as_deref(), Some(assigned.as_str()));

    // An empty object gains the id without a stray comma.
    let (ctx, fixture, server2) = fixture_with(30, "").await;
    fixture.reply.store(5, Ordering::SeqCst);
    assert_eq!(
        submit_body(ctx.clone(), " { } ").await.status(),
        StatusCode::OK
    );
    let (forwarded, _) = fixture.bodies.lock().unwrap()[0].clone();
    assert_eq!(
        String::from_utf8(forwarded).unwrap(),
        format!(" {{ \"prompt_id\":\"{}\"}}", fixture.assigned())
    );
    server.abort();
    server2.abort();
}

/// A body that cannot be a submission would be rejected by ComfyUI with a
/// generic error that proves nothing, leaving the resource held. It is
/// refused before anything is reserved or forwarded.
#[tokio::test]
async fn a_body_that_is_not_a_json_object_is_refused_before_reservation() {
    let (ctx, fixture, server) = fixture(30).await;
    for body in [
        "",
        "  ",
        "[]",
        "\"prompt\"",
        "not json",
        "{\"prompt\":",
        "{} {}",
        "{\"n\":NaN}",
    ] {
        let response = submit_body(ctx.clone(), body).await;
        assert_eq!(response.status(), StatusCode::BAD_REQUEST, "{body:?}");
        let reply = axum::body::to_bytes(response.into_body(), 4096)
            .await
            .unwrap();
        let reply: serde_json::Value = serde_json::from_slice(&reply).unwrap();
        assert_eq!(reply["error"]["type"], "invalid_submission", "{body:?}");
        assert!(work(&ctx).is_empty());
    }
    assert_eq!(fixture.submissions.load(Ordering::SeqCst), 0);
    assert_eq!(
        ctx.state
            .metrics
            .requests_total
            .with_label_values(&["comfy", "refused_request"])
            .get(),
        8
    );
    // The resource was never reserved, so no fresh sample is needed.
    fixture.reply.store(5, Ordering::SeqCst);
    assert_eq!(submit(ctx.clone()).await.status(), StatusCode::OK);
    server.abort();
}

/// Start a submission whose reply the stub holds, then let the owner return.
async fn owner_returns_mid_submission(
    policy: &str,
    queue_early: bool,
) -> (
    Arc<ProxyCtx>,
    Arc<ComfyStub>,
    tokio::task::JoinHandle<()>,
    tokio::task::JoinHandle<Response>,
    tokio::task::JoinHandle<()>,
) {
    let (ctx, fixture, server) = fixture_with(30, policy).await;
    fixture.reply.store(5, Ordering::SeqCst);
    fixture.hold_reply.store(true, Ordering::SeqCst);
    fixture.queue_early.store(queue_early, Ordering::SeqCst);
    let borrower = tokio::spawn(submit(ctx.clone()));
    tokio::time::timeout(Duration::from_secs(5), fixture.entered.notified())
        .await
        .unwrap();
    assert_eq!(work(&ctx)[0].phase, "forwarding");
    ctx.state.veto.observe(Some("owner".into()), 0);
    let reclaim = tokio::spawn(crate::yield_control::on_veto_engaged(ctx.state.clone()));
    (ctx, fixture, server, borrower, reclaim)
}

/// The owner-return deadline used to cut off the `/prompt` reply, so
/// the prompt stayed queued with no id to cancel. Now the reply is still
/// read and the prompt cancelled, whether the reply lands while the
/// reclaim waits for it or after the reclaim has moved on.
#[tokio::test]
async fn owner_return_during_the_submission_reply_still_cancels_the_prompt() {
    for policy in [
        "on_owner_return = \"cancel\"",
        "on_owner_return = \"drain\"\ndrain_timeout_s = 1",
    ] {
        for reply_after in [Duration::from_millis(100), Duration::from_millis(1500)] {
            for queue_early in [true, false] {
                let case =
                    format!("{policy} / reply after {reply_after:?} / queued early {queue_early}");
                let (ctx, fixture, server, borrower, reclaim) =
                    owner_returns_mid_submission(policy, queue_early).await;
                if policy.contains("drain") {
                    tokio::time::sleep(Duration::from_millis(1000)).await;
                }
                // The borrower is answered at the deadline, reply or not.
                let response = tokio::time::timeout(Duration::from_secs(1), borrower)
                    .await
                    .unwrap_or_else(|_| panic!("{case}: borrower not answered at the deadline"))
                    .unwrap();
                assert_eq!(response.status(), StatusCode::SERVICE_UNAVAILABLE, "{case}");
                tokio::time::sleep(reply_after).await;
                fixture.release.notify_one();
                tokio::time::timeout(Duration::from_secs(5), reclaim)
                    .await
                    .unwrap()
                    .unwrap();
                let assigned = fixture.assigned();
                wait_for(&format!("{case}: the prompt is cancelled"), || {
                    work(&ctx).is_empty() && fixture.pending.lock().unwrap().is_empty()
                })
                .await;
                // Exactly one side cancelled it, naming only its own id.
                assert_eq!(
                    *fixture.deletes.lock().unwrap(),
                    vec![serde_json::json!({"delete": [assigned]})],
                    "{case}"
                );
                server.abort();
            }
        }
    }
}

/// A borrower that goes away mid-submission cannot orphan the job: the
/// exchange finishes without it and the job is tracked to completion.
#[tokio::test]
async fn a_borrower_that_disconnects_mid_submission_leaves_the_job_tracked() {
    let (ctx, fixture, server) = fixture(30).await;
    fixture.reply.store(5, Ordering::SeqCst);
    fixture.hold_reply.store(true, Ordering::SeqCst);
    let borrower = tokio::spawn(submit(ctx.clone()));
    tokio::time::timeout(Duration::from_secs(5), fixture.entered.notified())
        .await
        .unwrap();
    borrower.abort();
    assert!(borrower.await.unwrap_err().is_cancelled());
    fixture.release.notify_one();
    let assigned = fixture.assigned();
    wait_for("the reply is recorded", || {
        work(&ctx).first().is_some_and(|w| w.phase == "tracking")
    })
    .await;
    assert_eq!(
        work(&ctx)[0].tracking_id.as_deref(),
        Some(assigned.as_str())
    );
    fixture.finished.lock().unwrap().push(assigned);
    wait_for("history releases the job", || work(&ctx).is_empty()).await;
    server.abort();
}

fn memory_ctx(target: String) -> Arc<ProxyCtx> {
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
        kind = "ollama"
        resource = "mem"
        "#
    ))
    .unwrap();
    let state = Arc::new(AppState::new(config).unwrap());
    state.mark_presence_sampled();
    sample(&state);
    Arc::new(ProxyCtx {
        upstream: state.config.upstreams[0].clone(),
        state,
    })
}

async fn generate(ctx: &Arc<ProxyCtx>) -> Response {
    handle(
        State(ctx.clone()),
        ConnectInfo("127.0.0.1:1234".parse().unwrap()),
        Request::builder()
            .method("POST")
            .uri("/api/generate")
            .body(Body::from(r#"{"model":"m","prompt":"hi"}"#))
            .unwrap(),
    )
    .await
}

#[tokio::test]
async fn refused_upstream_connection_releases_the_reservation_after_a_fresh_sample() {
    let closed = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = closed.local_addr().unwrap();
    drop(closed);
    let ctx = memory_ctx(format!("http://{addr}"));
    assert_eq!(generate(&ctx).await.status(), StatusCode::BAD_GATEWAY);
    // Nothing reached the upstream, so nothing is held for an operator.
    assert!(ctx.state.admission.lock().unwrap().snapshot().is_empty());
    // The resource still needs a capacity sample taken after the attempt.
    let upstream = ctx.upstream.clone();
    let refusal = match crate::admission::reserve(
        &ctx.state,
        &upstream,
        None,
        crate::plugin::adapter::Completion::ResponseClose,
    ) {
        Ok(_) => panic!("admitted without a post-attempt sample"),
        Err(refusal) => refusal,
    };
    assert!(
        refusal.detail.contains("capacity sample"),
        "{}",
        refusal.detail
    );
    sample(&ctx.state);
    // Admitted again: the borrower sees the upstream's state, not a 503.
    assert_eq!(generate(&ctx).await.status(), StatusCode::BAD_GATEWAY);
}

/// Stands in for an upstream whose body reports end-of-stream some time after
/// its final byte, while the server in front already stopped polling the body
/// once it wrote the declared Content-Length.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn declared_length_completes_work_even_when_the_body_is_dropped_before_eof() {
    let ctx = memory_ctx("http://127.0.0.1:1".into());
    let state = ctx.state.clone();
    let upstream = ctx.upstream.clone();
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let app_state = state.clone();
    let app = Router::new().fallback(move || {
        let state = app_state.clone();
        let upstream = upstream.clone();
        async move {
            let ticket = crate::admission::reserve(
                &state,
                &upstream,
                None,
                crate::plugin::adapter::Completion::ResponseClose,
            )
            .unwrap();
            let source = futures_util::stream::unfold(0u8, |step| async move {
                if step == 0 {
                    return Some((
                        Ok::<_, std::io::Error>(axum::body::Bytes::from_static(b"done")),
                        1,
                    ));
                }
                tokio::time::sleep(Duration::from_millis(200)).await;
                None
            });
            Response::builder()
                .header("content-length", "4")
                .body(streaming_body(
                    source,
                    Some(4),
                    None,
                    Some(ticket),
                    Duration::from_secs(60),
                ))
                .unwrap()
        }
    });
    let server = tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
    let body = reqwest::get(format!("http://{addr}/"))
        .await
        .unwrap()
        .bytes()
        .await
        .unwrap();
    assert_eq!(body.as_ref(), b"done");
    tokio::time::sleep(Duration::from_millis(500)).await;
    assert!(state.admission.lock().unwrap().snapshot().is_empty());
    server.abort();
}

async fn content_length_upstream(body: &'static str) -> std::net::SocketAddr {
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move {
        loop {
            let (mut socket, _) = listener.accept().await.unwrap();
            tokio::spawn(async move {
                let mut buf = vec![0u8; 8192];
                while matches!(socket.read(&mut buf).await, Ok(n) if n > 0) {
                    let reply = format!(
                        "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\n\r\n{body}",
                        body.len()
                    );
                    if socket.write_all(reply.as_bytes()).await.is_err() {
                        return;
                    }
                }
            });
        }
    });
    addr
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn content_length_replies_through_the_proxy_always_release() {
    let addr = content_length_upstream(r#"{"model":"m","response":"hello","done":true}"#).await;
    let ctx = memory_ctx(format!("http://{addr}"));
    let state = ctx.state.clone();
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let proxy = listener.local_addr().unwrap();
    let server = tokio::spawn(serve_on(listener, state.clone(), ctx.upstream.clone()));
    let client = reqwest::Client::new();
    for _ in 0..50 {
        let response = client
            .post(format!("http://{proxy}/api/generate"))
            .body(r#"{"model":"m","prompt":"hi"}"#)
            .send()
            .await
            .unwrap();
        assert_eq!(response.status(), 200);
        response.bytes().await.unwrap();
        tokio::time::timeout(Duration::from_secs(2), async {
            while !state.admission.lock().unwrap().snapshot().is_empty() {
                tokio::time::sleep(Duration::from_millis(5)).await;
            }
        })
        .await
        .expect("a fully delivered Content-Length reply left its reservation");
        sample(&state);
    }
    server.abort();
}

/// As found live: gemma3:12b is resident (8,107,684,003 bytes, context
/// 65536), 473 MiB is free, and the configured load requirement is 8200 MiB.
/// A follow-up that reuses the runner as loaded is admitted; one whose
/// num_ctx would make Ollama reload it is still charged the full load.
#[tokio::test]
async fn a_resident_ollama_model_serves_its_follow_up_through_the_proxy() {
    let chats = Arc::new(AtomicUsize::new(0));
    let app = Router::new()
        .route(
            "/api/ps",
            get(|| async {
                Json(serde_json::json!({"models":[{
                    "name":"gemma3:12b","model":"gemma3:12b","size":8107684003u64,
                    "size_vram":8107684003u64,"digest":"f4031aab637d",
                    "expires_at":"2318-01-02T03:04:05.678-06:00","context_length":65536}]}))
            }),
        )
        .route(
            "/api/chat",
            post(|State(chats): State<Arc<AtomicUsize>>| async move {
                chats.fetch_add(1, Ordering::SeqCst);
                Json(serde_json::json!({"model":"gemma3:12b","done":true}))
            }),
        )
        .with_state(chats.clone());
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let target = listener.local_addr().unwrap();
    let server = tokio::spawn(async move {
        axum::serve(listener, app).await.unwrap();
    });
    let config = toml::from_str(&format!(
        r#"
        [server]
        listen = "127.0.0.1:0"
        [owner.gpu_pressure]
        enabled = false
        [[resource]]
        id = "gpu0"
        kind = "nvidia-gpu"
        [[upstream]]
        id = "ollama"
        listen = "127.0.0.1:0"
        target = "http://{target}"
        kind = "ollama"
        resource = "gpu0"
        [upstream.models]
        "gemma3:12b" = 8200
    "#
    ))
    .unwrap();
    let state = Arc::new(AppState::new(config).unwrap());
    state.mark_presence_sampled();
    let free = |state: &AppState| {
        let resource = &state.resources["gpu0"];
        let mut observation = resource.capacity.write().unwrap();
        resource
            .probe_lendable
            .store(true, std::sync::atomic::Ordering::Relaxed);
        observation.record_success(crate::plugin::probe::Capacity {
            free_bytes: 473 * 1024 * 1024,
            ..Default::default()
        });
    };
    free(&state);
    let ctx = Arc::new(ProxyCtx {
        upstream: state.config.upstreams[0].clone(),
        state: state.clone(),
    });
    let chat = |num_ctx: u64| {
        handle(
            State(ctx.clone()),
            ConnectInfo("127.0.0.1:1234".parse().unwrap()),
            Request::builder()
                .method("POST")
                .uri("/api/chat")
                .body(Body::from(format!(
                    r#"{{"model":"registry.ollama.ai/library/gemma3:12b","messages":[],"stream":false,"options":{{"num_ctx":{num_ctx},"temperature":0.2}}}}"#
                )))
                .unwrap(),
        )
    };

    // No model observation yet: the refusal path observes /api/ps once.
    let response = chat(65536).await;
    assert_eq!(response.status(), StatusCode::OK);
    axum::body::to_bytes(response.into_body(), 1 << 20)
        .await
        .unwrap();
    assert_eq!(chats.load(Ordering::SeqCst), 1);
    tokio::time::timeout(Duration::from_secs(2), async {
        while !state.admission.lock().unwrap().snapshot().is_empty() {
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
    })
    .await
    .unwrap();

    free(&state);
    let response = chat(4096).await;
    assert_eq!(response.status(), StatusCode::SERVICE_UNAVAILABLE);
    let body = axum::body::to_bytes(response.into_body(), 1 << 20)
        .await
        .unwrap();
    let body = String::from_utf8_lossy(&body);
    assert!(body.contains("needs 8200 MiB free"), "{body}");
    assert!(
        body.contains("num_ctx 4096 differs from the resident context 65536"),
        "{body}"
    );
    assert_eq!(chats.load(Ordering::SeqCst), 1);
    server.abort();
}

/// A reclaim deadline that passes while admission waits for a capacity
/// sample leaves a reservation that nothing was sent under. It is released,
/// not held for an operator, and nothing reaches the upstream.
#[tokio::test]
async fn a_deadline_before_forwarding_releases_instead_of_holding() {
    let (ctx, fixture, server) = fixture_with(30, "on_owner_return = \"cancel\"").await;
    // A finished job makes the next admission wait for a newer sample.
    crate::admission::reserve(
        &ctx.state,
        &ctx.upstream,
        None,
        crate::plugin::adapter::Completion::Tracked,
    )
    .unwrap()
    .submission_rejected();
    let borrower = tokio::spawn(submit(ctx.clone()));
    tokio::time::sleep(Duration::from_millis(40)).await;
    assert!(
        !borrower.is_finished(),
        "admission did not wait for a sample"
    );
    // GPU pressure on the resource: an edge the gate has not recorded yet.
    ctx.state.resources["mem"]
        .reclaims
        .send(tokio::time::Instant::now())
        .unwrap();
    tokio::time::sleep(Duration::from_millis(40)).await;
    sample(&ctx.state);
    let response = tokio::time::timeout(Duration::from_secs(5), borrower)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(response.status(), StatusCode::SERVICE_UNAVAILABLE);
    assert_eq!(fixture.submissions.load(Ordering::SeqCst), 0);
    assert!(work(&ctx).is_empty(), "{:?}", work(&ctx));
    server.abort();
}

/// Owner return against a real ComfyUI, for checking a release by hand:
///
/// ```text
/// VACANCYD_TEST_COMFYUI=http://127.0.0.1:8188 cargo test --locked -- --ignored real_comfyui
/// ```
///
/// Use a throwaway instance (it runs CPU-only stock nodes), never one someone
/// is using. A borrowed prompt runs through the proxy while the owner queues
/// a prompt directly with a 2 MiB workflow; then the owner returns. On
/// 0.3.57 and later the borrowed prompt is stopped and the owner's runs; on
/// earlier releases nothing is interrupted and both run to completion. The
/// assigned id is the job's id from 0.3.45.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore = "needs a throwaway ComfyUI at VACANCYD_TEST_COMFYUI"]
async fn real_comfyui_owner_return_stops_only_borrowed_work() {
    let base = std::env::var("VACANCYD_TEST_COMFYUI").expect("VACANCYD_TEST_COMFYUI");
    let base = base.trim_end_matches('/').to_owned();
    let http = reqwest::Client::new();
    let get = |path: String| {
        let http = http.clone();
        async move {
            http.get(path)
                .send()
                .await
                .unwrap()
                .json::<serde_json::Value>()
                .await
                .unwrap()
        }
    };
    let version = get(format!("{base}/system_stats")).await["system"]["comfyui_version"]
        .as_str()
        .unwrap_or("unknown")
        .to_owned();
    let parsed = {
        let v: Vec<u64> = version
            .trim_start_matches('v')
            .split(['.', '-'])
            .take(3)
            .map(|p| p.parse().unwrap_or(0))
            .collect();
        [v[0], v[1], *v.get(2).unwrap_or(&0)]
    };
    let queue = get(format!("{base}/queue")).await;
    assert!(
        queue["queue_running"].as_array().unwrap().is_empty()
            && queue["queue_pending"].as_array().unwrap().is_empty(),
        "the test ComfyUI must start idle"
    );
    let workflow = |blurs: usize| {
        let mut graph = serde_json::json!({"1": {"class_type": "EmptyImage",
            "inputs": {"width": 2048, "height": 2048, "batch_size": 1, "color": unique_color()}}});
        let mut previous = "1".to_owned();
        for n in 0..blurs {
            let id = (n + 2).to_string();
            graph[&id] = serde_json::json!({"class_type": "ImageBlur",
                "inputs": {"image": [previous, 0], "blur_radius": 31, "sigma": 10.0}});
            previous = id;
        }
        graph["999"] =
            serde_json::json!({"class_type": "PreviewImage", "inputs": {"images": [previous, 0]}});
        graph
    };

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
        id = "comfy"
        listen = "127.0.0.1:0"
        target = "{base}"
        kind = "comfyui"
        resource = "mem"
        on_owner_return = "cancel"
    "#
    ))
    .unwrap();
    let state = Arc::new(AppState::new(config).unwrap());
    state.mark_presence_sampled();
    sample(&state);
    state.upstreams["comfy"]
        .models
        .write()
        .unwrap()
        .record_success(crate::lifecycle::Models::default());
    let ctx = Arc::new(ProxyCtx {
        upstream: state.config.upstreams[0].clone(),
        state: state.clone(),
    });

    // The borrower supplies its own prompt_id, which vacancyd replaces.
    let borrowed_body = serde_json::json!({"prompt": workflow(6),
        "prompt_id": "0f2c5a1e-6b1d-4d6b-9d7e-3b1f0c2a9e11", "client_id": "borrower"})
    .to_string();
    let response = handle(
        State(ctx.clone()),
        ConnectInfo("127.0.0.1:1234".parse().unwrap()),
        Request::builder()
            .method("POST")
            .uri("/prompt")
            .body(Body::from(borrowed_body))
            .unwrap(),
    )
    .await;
    assert_eq!(response.status(), StatusCode::OK);
    let reply: serde_json::Value = serde_json::from_slice(
        &axum::body::to_bytes(response.into_body(), 1 << 20)
            .await
            .unwrap(),
    )
    .unwrap();
    let borrowed = reply["prompt_id"].as_str().unwrap().to_owned();
    assert_ne!(borrowed, "0f2c5a1e-6b1d-4d6b-9d7e-3b1f0c2a9e11");
    let tracked = work(&ctx)[0].tracking_id.clone().unwrap();
    assert_eq!(tracked, borrowed, "the reply's id is the one tracked");
    eprintln!("ComfyUI {version}: borrowed prompt {borrowed}");

    // Wait until it runs, then the owner queues a large workflow directly.
    wait_for_async(&http, &format!("{base}/queue"), |queue| {
        queue["queue_running"][0][1] == borrowed.as_str()
    })
    .await;
    let owner: serde_json::Value = http
        .post(format!("{base}/prompt"))
        .json(&serde_json::json!({"prompt": workflow(1),
            "extra_data": {"extra_pnginfo": {"workflow": {"note": "o".repeat(2 << 20)}}}}))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    let owner = owner["prompt_id"].as_str().unwrap().to_owned();

    ctx.state.veto.observe(Some("owner".into()), 0);
    let started = std::time::Instant::now();
    tokio::time::timeout(
        Duration::from_secs(30),
        crate::yield_control::on_veto_engaged(state.clone()),
    )
    .await
    .unwrap();
    let history = |id: String| {
        let http = http.clone();
        let base = base.clone();
        async move {
            loop {
                let h: serde_json::Value = http
                    .get(format!("{base}/history/{id}"))
                    .send()
                    .await
                    .unwrap()
                    .json()
                    .await
                    .unwrap();
                if let Some(status) = h[&id]["status"]["status_str"].as_str() {
                    return status.to_owned();
                }
                tokio::time::sleep(Duration::from_millis(250)).await;
            }
        }
    };
    if parsed >= [0, 3, 57] {
        // Stopped at the next node boundary, released on its history, and
        // the owner's prompt ran.
        assert_eq!(
            tokio::time::timeout(Duration::from_secs(15), history(borrowed.clone()))
                .await
                .unwrap(),
            "error"
        );
        eprintln!("borrowed prompt stopped after {:?}", started.elapsed());
        wait_for("the stopped prompt is released", || work(&ctx).is_empty()).await;
        assert_eq!(
            tokio::time::timeout(Duration::from_secs(120), history(owner))
                .await
                .unwrap(),
            "success"
        );
    } else {
        // Never interrupted: both finish, and the tracker releases the
        // borrowed prompt on its history.
        assert_eq!(work(&ctx).len(), 1);
        assert_eq!(
            tokio::time::timeout(Duration::from_secs(180), history(borrowed))
                .await
                .unwrap(),
            "success"
        );
        assert_eq!(
            tokio::time::timeout(Duration::from_secs(120), history(owner))
                .await
                .unwrap(),
            "success"
        );
        wait_for("the tracker releases the borrowed prompt", || {
            work(&ctx).is_empty()
        })
        .await;
    }
}

async fn wait_for_async(
    http: &reqwest::Client,
    url: &str,
    done: impl Fn(&serde_json::Value) -> bool,
) {
    tokio::time::timeout(Duration::from_secs(60), async {
        loop {
            let value: serde_json::Value =
                http.get(url).send().await.unwrap().json().await.unwrap();
            if done(&value) {
                return;
            }
            tokio::time::sleep(Duration::from_millis(100)).await;
        }
    })
    .await
    .unwrap_or_else(|_| panic!("timed out polling {url}"));
}

/// Submit through the proxy to a ComfyUI that acknowledges the assigned id
/// and queues the prompt, as 0.3.45 and later do.
async fn acknowledged(ctx: &Arc<ProxyCtx>, fixture: &ComfyStub) -> String {
    fixture.reply.store(5, Ordering::SeqCst);
    assert_eq!(submit(ctx.clone()).await.status(), StatusCode::OK);
    let id = fixture.assigned();
    assert_eq!(work(ctx)[0].tracking_id.as_deref(), Some(id.as_str()));
    id
}

/// Wait until the tracker has read the queue `reads` more times.
async fn queue_reads(fixture: &ComfyStub, reads: usize) {
    let target = fixture.queue_reads.load(Ordering::SeqCst) + reads;
    wait_for(&format!("{reads} more queue reads"), || {
        fixture.queue_reads.load(Ordering::SeqCst) >= target
    })
    .await;
}

async fn owner_posts_queue(ctx: &ProxyCtx, body: serde_json::Value) {
    let reply = reqwest::Client::new()
        .post(format!("{}/queue", ctx.upstream.target))
        .json(&body)
        .send()
        .await
        .unwrap();
    assert!(reply.status().is_success());
}

/// A borrowed prompt the owner deletes from ComfyUI's queue never runs
/// and never reaches its history. It used to stay reserved until
/// `completion_timeout_s` and then be held for an administrator.
#[tokio::test]
async fn an_owner_deleting_a_pending_borrowed_prompt_releases_it() {
    // Against a release with the jobs route, and one without it.
    for jobs in [true, false] {
        let (ctx, fixture, server) = fixture(3600).await;
        fixture.jobs.store(jobs, Ordering::SeqCst);
        let id = acknowledged(&ctx, &fixture).await;
        // Pending: however many checks pass, it stays reserved.
        queue_reads(&fixture, 4).await;
        assert_eq!(work(&ctx)[0].phase, "tracking");
        owner_posts_queue(&ctx, serde_json::json!({"delete": [id]})).await;
        let deleted_at = fixture.queue_reads.load(Ordering::SeqCst);
        wait_for("the deleted prompt is released", || work(&ctx).is_empty()).await;
        // Two consecutive checks found it gone, not one.
        assert!(fixture.queue_reads.load(Ordering::SeqCst) >= deleted_at + 2);
        // Like any finished job, reuse waits for a fresh capacity sample.
        assert_eq!(
            submit(ctx.clone()).await.status(),
            StatusCode::SERVICE_UNAVAILABLE
        );
        sample(&ctx.state);
        fixture.reply.store(5, Ordering::SeqCst);
        assert_eq!(submit(ctx.clone()).await.status(), StatusCode::OK);
        server.abort();
    }
}

#[tokio::test]
async fn an_owner_clearing_the_queue_releases_the_borrowed_prompt() {
    // Against a release with the jobs route, and one without it.
    for jobs in [true, false] {
        let (ctx, fixture, server) = fixture(3600).await;
        fixture.jobs.store(jobs, Ordering::SeqCst);
        fixture
            .pending
            .lock()
            .unwrap()
            .extend(["owner-1".to_owned(), "owner-2".to_owned()]);
        acknowledged(&ctx, &fixture).await;
        queue_reads(&fixture, 3).await;
        assert_eq!(work(&ctx)[0].phase, "tracking");
        owner_posts_queue(&ctx, serde_json::json!({"clear": true})).await;
        wait_for("the cleared prompt is released", || work(&ctx).is_empty()).await;
        server.abort();
    }
}

/// A prompt that ran and whose history was then cleared (or evicted, or
/// lost with a ComfyUI restart) is no longer anywhere; it has ended.
#[tokio::test]
async fn a_finished_prompt_whose_history_was_cleared_is_released() {
    // Against a release with the jobs route, and one without it.
    for jobs in [true, false] {
        let (ctx, fixture, server) = fixture(3600).await;
        fixture.jobs.store(jobs, Ordering::SeqCst);
        let id = acknowledged(&ctx, &fixture).await;
        // Running: still reserved.
        fixture.pending.lock().unwrap().retain(|p| *p != id);
        fixture.running.lock().unwrap().push(id.clone());
        queue_reads(&fixture, 4).await;
        assert_eq!(work(&ctx)[0].phase, "tracking");
        // It ends, and its history is gone before the tracker reads it.
        fixture.running.lock().unwrap().clear();
        wait_for("the prompt is released", || work(&ctx).is_empty()).await;
        server.abort();
    }
}

/// One reading that misses the prompt is not enough, and a failed reading
/// is not evidence: it restarts the count.
#[tokio::test]
async fn a_single_absent_or_failed_queue_read_does_not_release() {
    // Against a release with the jobs route, and one without it.
    for jobs in [true, false] {
        let (ctx, fixture, server) = fixture(3600).await;
        fixture.jobs.store(jobs, Ordering::SeqCst);
        acknowledged(&ctx, &fixture).await;
        for script in [
            vec!["hide"],
            vec!["hide", "fail", "hide"],
            vec!["fail", "hide", "fail", "hide"],
        ] {
            fixture
                .script
                .lock()
                .unwrap()
                .extend(script.iter().copied());
            queue_reads(&fixture, script.len() + 3).await;
            assert!(fixture.script.lock().unwrap().is_empty());
            assert_eq!(work(&ctx)[0].phase, "tracking", "{script:?}");
        }
        // Two absent reads in a row do release it.
        fixture.script.lock().unwrap().extend(["hide", "hide"]);
        wait_for("two absent reads release it", || work(&ctx).is_empty()).await;
        server.abort();
    }
}

/// An id vacancyd assigned but ComfyUI never acknowledged may still be
/// queued later, or belong to a release that ignored it: its absence proves
/// nothing, so the queue is not even consulted for it.
#[tokio::test]
async fn an_unacknowledged_prompt_is_not_released_by_its_absence() {
    // Against a release with the jobs route, and one without it.
    for jobs in [true, false] {
        let (ctx, fixture, server) = fixture(2).await;
        fixture.jobs.store(jobs, Ordering::SeqCst);
        fixture.reply.store(2, Ordering::SeqCst);
        assert_eq!(submit(ctx.clone()).await.status(), StatusCode::OK);
        let assigned = fixture.assigned();
        assert_eq!(
            work(&ctx)[0].tracking_id.as_deref(),
            Some(assigned.as_str())
        );
        wait_for("the tracking bound holds it", || {
            work(&ctx).first().is_some_and(|w| w.phase == "held")
        })
        .await;
        assert_eq!(fixture.queue_reads.load(Ordering::SeqCst), 0);
        server.abort();
    }
}

/// As found live: ComfyUI's connection failed part way through a borrowed
/// submission, and the prompt was never queued. Its reservation held the GPU
/// until the tracking bound, and then until an administrator released it.
/// On a release that keeps the assigned id, a prompt that is still nowhere
/// once ComfyUI has had time to finish validating it never will be.
#[tokio::test]
async fn a_submission_lost_before_any_reply_is_released_when_comfyui_never_queued_it() {
    // Against a release with the jobs route, and one without it.
    for (jobs, version) in [(true, "0.26.1"), (false, "0.3.45")] {
        let (ctx, fixture, server) = fixture(3600).await;
        fixture.jobs.store(jobs, Ordering::SeqCst);
        *fixture.version.lock().unwrap() = Some(version);
        fixture.reply.store(7, Ordering::SeqCst);
        let submitted = std::time::Instant::now();
        assert_eq!(submit(ctx.clone()).await.status(), StatusCode::BAD_GATEWAY);
        let assigned = fixture.assigned();
        let entry = work(&ctx);
        assert_eq!(entry[0].phase, "tracking");
        assert_eq!(entry[0].tracking_id.as_deref(), Some(assigned.as_str()));
        wait_for("the lost prompt is released", || work(&ctx).is_empty()).await;
        // Not within the grace that tests give ComfyUI to queue it late, and
        // only after two checks that both found it nowhere.
        assert!(submitted.elapsed() >= Duration::from_millis(600));
        assert!(fixture.queue_reads.load(Ordering::SeqCst) >= 2);
        // Like any finished job, reuse waits for a fresh capacity sample.
        assert_eq!(
            submit(ctx.clone()).await.status(),
            StatusCode::SERVICE_UNAVAILABLE
        );
        server.abort();
    }
}

/// A submission whose connection failed after ComfyUI queued it is a prompt
/// that will run. Seen in the queue, it is followed like an acknowledged one.
#[tokio::test]
async fn a_lost_submission_that_comfyui_queued_stays_reserved_until_it_ends() {
    // Against a release with the jobs route, and one without it.
    for jobs in [true, false] {
        let (ctx, fixture, server) = fixture(3600).await;
        fixture.jobs.store(jobs, Ordering::SeqCst);
        *fixture.version.lock().unwrap() = Some("0.3.45");
        fixture.reply.store(8, Ordering::SeqCst);
        assert_eq!(submit(ctx.clone()).await.status(), StatusCode::BAD_GATEWAY);
        let assigned = fixture.assigned();
        // Queue checks start after the grace; each finds it pending.
        queue_reads(&fixture, 4).await;
        assert_eq!(work(&ctx)[0].phase, "tracking");
        fixture.pending.lock().unwrap().retain(|p| *p != assigned);
        fixture.finished.lock().unwrap().push(assigned);
        wait_for("its history releases it", || work(&ctx).is_empty()).await;
        server.abort();
    }
}

/// A release that may have queued the prompt under an id of its own, or that
/// does not say which release it is, gives no meaning to the assigned id's
/// absence: the lost submission is held as before.
#[tokio::test]
async fn a_lost_submission_is_held_on_a_comfyui_that_may_ignore_the_assigned_id() {
    for version in [None, Some("0.3.44"), Some("unknown")] {
        let (ctx, fixture, server) = fixture(2).await;
        *fixture.version.lock().unwrap() = version;
        fixture.reply.store(7, Ordering::SeqCst);
        assert_eq!(submit(ctx.clone()).await.status(), StatusCode::BAD_GATEWAY);
        wait_for("the tracking bound holds it", || {
            work(&ctx).first().is_some_and(|w| w.phase == "held")
        })
        .await;
        assert_eq!(fixture.queue_reads.load(Ordering::SeqCst), 0, "{version:?}");
        server.abort();
    }
}

/// Prompts the owner removes, against a real ComfyUI, for checking a release
/// by hand:
///
/// ```text
/// VACANCYD_TEST_COMFYUI=http://127.0.0.1:8188 cargo test --locked -- --ignored real_comfyui
/// ```
///
/// Use a throwaway instance. The owner keeps a long prompt running while
/// borrowed prompts wait behind it. One the owner deletes, and then one the
/// owner clears from the queue, are released; one that runs to completion is
/// released on its history as before. The owner's prompt is never touched.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore = "needs a throwaway ComfyUI at VACANCYD_TEST_COMFYUI"]
async fn real_comfyui_tracking_releases_prompts_the_owner_removed() {
    let base = std::env::var("VACANCYD_TEST_COMFYUI").expect("VACANCYD_TEST_COMFYUI");
    let base = base.trim_end_matches('/').to_owned();
    let http = reqwest::Client::new();
    let json = |request: reqwest::RequestBuilder| async move {
        request
            .send()
            .await
            .unwrap()
            .json::<serde_json::Value>()
            .await
            .unwrap()
    };
    let version = json(http.get(format!("{base}/system_stats"))).await["system"]["comfyui_version"]
        .as_str()
        .unwrap_or("unknown")
        .to_owned();
    let queue = json(http.get(format!("{base}/queue"))).await;
    assert!(
        queue["queue_running"].as_array().unwrap().is_empty()
            && queue["queue_pending"].as_array().unwrap().is_empty(),
        "the test ComfyUI must start idle"
    );
    let workflow = |blurs: usize| {
        let mut graph = serde_json::json!({"1": {"class_type": "EmptyImage",
            "inputs": {"width": 2048, "height": 2048, "batch_size": 1, "color": unique_color()}}});
        let mut previous = "1".to_owned();
        for n in 0..blurs {
            let id = (n + 2).to_string();
            graph[&id] = serde_json::json!({"class_type": "ImageBlur",
                "inputs": {"image": [previous, 0], "blur_radius": 31, "sigma": 10.0}});
            previous = id;
        }
        graph["999"] =
            serde_json::json!({"class_type": "PreviewImage", "inputs": {"images": [previous, 0]}});
        graph
    };
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
        id = "comfy"
        listen = "127.0.0.1:0"
        target = "{base}"
        kind = "comfyui"
        resource = "mem"
    "#
    ))
    .unwrap();
    let state = Arc::new(AppState::new(config).unwrap());
    state.mark_presence_sampled();
    let ctx = Arc::new(ProxyCtx {
        upstream: state.config.upstreams[0].clone(),
        state: state.clone(),
    });
    let borrow = |blurs: usize| {
        let ctx = ctx.clone();
        let body =
            serde_json::json!({"prompt": workflow(blurs), "client_id": "borrower"}).to_string();
        async move {
            // Fresh evidence for admission, as the collectors would supply.
            sample(&ctx.state);
            ctx.state.upstreams["comfy"]
                .models
                .write()
                .unwrap()
                .record_success(crate::lifecycle::Models::default());
            let response = handle(
                State(ctx.clone()),
                ConnectInfo("127.0.0.1:1234".parse().unwrap()),
                Request::builder()
                    .method("POST")
                    .uri("/prompt")
                    .body(Body::from(body))
                    .unwrap(),
            )
            .await;
            assert_eq!(response.status(), StatusCode::OK);
            let reply: serde_json::Value = serde_json::from_slice(
                &axum::body::to_bytes(response.into_body(), 1 << 20)
                    .await
                    .unwrap(),
            )
            .unwrap();
            reply["prompt_id"].as_str().unwrap().to_owned()
        }
    };
    let status = |id: String| {
        let http = http.clone();
        let base = base.clone();
        async move {
            let history: serde_json::Value = http
                .get(format!("{base}/history/{id}"))
                .send()
                .await
                .unwrap()
                .json()
                .await
                .unwrap();
            history[&id]["status"]["status_str"]
                .as_str()
                .map(str::to_owned)
        }
    };

    // ComfyUI answers a queue change with an empty 200.
    let owner_queue = |body: serde_json::Value| {
        let request = http.post(format!("{base}/queue")).json(&body);
        async move { assert!(request.send().await.unwrap().status().is_success()) }
    };

    // The owner's long prompt, queued directly.
    let owner = json(
        http.post(format!("{base}/prompt"))
            .json(&serde_json::json!({"prompt": workflow(12)})),
    )
    .await;
    let owner = owner["prompt_id"].as_str().unwrap().to_owned();
    wait_for_async(&http, &format!("{base}/queue"), |queue| {
        queue["queue_running"][0][1] == owner.as_str()
    })
    .await;

    // 1. Pending behind it, it stays reserved; deleted, it is released.
    let deleted = borrow(1).await;
    tokio::time::sleep(Duration::from_secs(2)).await;
    assert_eq!(
        work(&ctx)[0].phase,
        "tracking",
        "a pending prompt was released"
    );
    let started = std::time::Instant::now();
    owner_queue(serde_json::json!({"delete": [deleted]})).await;
    wait_for("the deleted prompt is released", || work(&ctx).is_empty()).await;
    eprintln!(
        "ComfyUI {version}: deleted prompt released after {:?}",
        started.elapsed()
    );
    assert_eq!(status(deleted).await, None);

    // 2. The owner clears the queue.
    let cleared = borrow(1).await;
    tokio::time::sleep(Duration::from_secs(2)).await;
    assert_eq!(work(&ctx)[0].phase, "tracking");
    let started = std::time::Instant::now();
    owner_queue(serde_json::json!({"clear": true})).await;
    wait_for("the cleared prompt is released", || work(&ctx).is_empty()).await;
    eprintln!(
        "ComfyUI {version}: cleared prompt released after {:?}",
        started.elapsed()
    );
    assert_eq!(status(cleared).await, None);

    // 3. One that waits, runs and finishes is released on its history.
    let finished = borrow(1).await;
    tokio::time::timeout(Duration::from_secs(180), async {
        while !work(&ctx).is_empty() {
            assert_ne!(
                status(owner.clone()).await.as_deref(),
                Some("error"),
                "the owner's prompt failed"
            );
            tokio::time::sleep(Duration::from_millis(250)).await;
        }
    })
    .await
    .expect("the finished prompt was not released");
    assert_eq!(status(finished).await.as_deref(), Some("success"));
    assert_eq!(status(owner).await.as_deref(), Some("success"));
}

/// A different `EmptyImage` colour for every test prompt. ComfyUI caches node
/// outputs, so a graph identical to one it already ran finishes at once.
fn unique_color() -> u64 {
    static NEXT: AtomicUsize = AtomicUsize::new(0);
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .subsec_nanos() as u64;
    (nanos + NEXT.fetch_add(1, Ordering::SeqCst) as u64 * 7919) % 0xFF_FFFF
}

/// Cancellation bounds against a real ComfyUI: owner return while the owner has a large
/// queue pending behind a running borrowed prompt. ComfyUI 0.37.2 copies the
/// whole pending queue to answer the stop request, which took about 40
/// seconds with 100 MiB pending on the development VM; a fixed 10-second
/// cancellation budget gave up before the answer. Run by hand:
///
/// ```text
/// VACANCYD_TEST_COMFYUI=http://127.0.0.1:8188 cargo test --locked -- --ignored real_comfyui_large_queue
/// ```
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore = "needs a throwaway ComfyUI at VACANCYD_TEST_COMFYUI; takes minutes"]
async fn real_comfyui_large_queue_owner_return_still_stops_the_borrowed_prompt() {
    let base = std::env::var("VACANCYD_TEST_COMFYUI").expect("VACANCYD_TEST_COMFYUI");
    let base = base.trim_end_matches('/').to_owned();
    let http = reqwest::Client::new();
    let queue_len = |queue: &serde_json::Value| {
        queue["queue_running"].as_array().unwrap().len()
            + queue["queue_pending"].as_array().unwrap().len()
    };
    let queue: serde_json::Value = http
        .get(format!("{base}/queue"))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(queue_len(&queue), 0, "the test ComfyUI must start idle");
    let workflow = |blurs: usize, size: u64| {
        let mut graph = serde_json::json!({"1": {"class_type": "EmptyImage",
            "inputs": {"width": size, "height": size, "batch_size": 1, "color": unique_color()}}});
        let mut previous = "1".to_owned();
        for n in 0..blurs {
            let id = (n + 2).to_string();
            graph[&id] = serde_json::json!({"class_type": "ImageBlur",
                "inputs": {"image": [previous, 0], "blur_radius": 31, "sigma": 10.0}});
            previous = id;
        }
        graph["999"] =
            serde_json::json!({"class_type": "PreviewImage", "inputs": {"images": [previous, 0]}});
        graph
    };
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
        id = "comfy"
        listen = "127.0.0.1:0"
        target = "{base}"
        kind = "comfyui"
        resource = "mem"
        on_owner_return = "cancel"
    "#
    ))
    .unwrap();
    let state = Arc::new(AppState::new(config).unwrap());
    state.mark_presence_sampled();
    sample(&state);
    state.upstreams["comfy"]
        .models
        .write()
        .unwrap()
        .record_success(crate::lifecycle::Models::default());
    let ctx = Arc::new(ProxyCtx {
        upstream: state.config.upstreams[0].clone(),
        state: state.clone(),
    });
    // A long borrowed prompt, running.
    let body = serde_json::json!({"prompt": workflow(60, 2048)}).to_string();
    let response = handle(
        State(ctx.clone()),
        ConnectInfo("127.0.0.1:1234".parse().unwrap()),
        Request::builder()
            .method("POST")
            .uri("/prompt")
            .body(Body::from(body))
            .unwrap(),
    )
    .await;
    assert_eq!(response.status(), StatusCode::OK);
    let reply: serde_json::Value = serde_json::from_slice(
        &axum::body::to_bytes(response.into_body(), 1 << 20)
            .await
            .unwrap(),
    )
    .unwrap();
    let borrowed = reply["prompt_id"].as_str().unwrap().to_owned();
    wait_for_async(&http, &format!("{base}/queue"), |queue| {
        queue["queue_running"][0][1] == borrowed.as_str()
    })
    .await;
    // The owner queues 50 prompts that each carry a 2 MiB UI workflow.
    let node = serde_json::json!({"id": 1, "type": "KSampler", "pos": [10.5, 20.25], "size": [315, 262],
        "widgets_values": [123456789, "randomize", 30, 7.5, "euler", "normal", 1.0]});
    let nodes = vec![node; (2 << 20) / 150];
    let extra = serde_json::json!({"extra_pnginfo": {"workflow": {"nodes": nodes, "links": []}}});
    for _ in 0..50 {
        let reply = http
            .post(format!("{base}/prompt"))
            .json(&serde_json::json!({"prompt": workflow(1, 64), "extra_data": extra}))
            .send()
            .await
            .unwrap();
        assert!(reply.status().is_success());
    }

    ctx.state.veto.observe(Some("owner".into()), 0);
    let started = std::time::Instant::now();
    crate::yield_control::on_veto_engaged(state.clone()).await;
    eprintln!(
        "reclaim with a large owner queue took {:?}",
        started.elapsed()
    );
    let history: serde_json::Value = http
        .get(format!("{base}/history/{borrowed}"))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    // Stopped by the cancellation itself, not by running to completion.
    assert_eq!(history[&borrowed]["status"]["status_str"], "error");
    assert!(work(&ctx).is_empty(), "{:?}", work(&ctx));
    // The owner's queue is untouched.
    let queue: serde_json::Value = http
        .get(format!("{base}/queue"))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(queue_len(&queue), 50);
    assert!(http
        .post(format!("{base}/queue"))
        .json(&serde_json::json!({"clear": true}))
        .send()
        .await
        .unwrap()
        .status()
        .is_success());
}

/// A background Responses request would be answered `queued` while the
/// upstream kept generating, so releasing its reservation on the reply would
/// lend the resource twice. It is refused before anything is reserved or
/// forwarded; the same request without `background` is admitted and released.
#[tokio::test]
async fn a_background_responses_request_is_refused_before_reservation() {
    let hits = Arc::new(AtomicUsize::new(0));
    let app = Router::new()
        .route(
            "/v1/responses",
            post(|State(hits): State<Arc<AtomicUsize>>| async move {
                hits.fetch_add(1, Ordering::SeqCst);
                Json(serde_json::json!({"id":"resp_1","status":"completed","output":[]}))
            }),
        )
        .with_state(hits.clone());
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let target = listener.local_addr().unwrap();
    let server = tokio::spawn(async move {
        axum::serve(listener, app).await.unwrap();
    });
    let ctx = memory_ctx(format!("http://{target}"));
    let respond = |body: &'static str| {
        handle(
            State(ctx.clone()),
            ConnectInfo("127.0.0.1:1234".parse().unwrap()),
            Request::builder()
                .method("POST")
                .uri("/v1/responses")
                .body(Body::from(body))
                .unwrap(),
        )
    };
    for body in [
        r#"{"model":"m","input":"hi","background":true,"store":true}"#,
        r#"{"model":"m","input":"hi","background":true,"stream":true}"#,
        r#"{"model":"m","input":"hi","background":"true"}"#,
    ] {
        let response = respond(body).await;
        assert_eq!(response.status(), StatusCode::BAD_REQUEST, "{body}");
        let reply = axum::body::to_bytes(response.into_body(), 4096)
            .await
            .unwrap();
        let reply: serde_json::Value = serde_json::from_slice(&reply).unwrap();
        assert_eq!(reply["error"]["type"], "unsupported_request", "{body}");
        assert!(work(&ctx).is_empty());
    }
    assert_eq!(hits.load(Ordering::SeqCst), 0);
    assert_eq!(
        ctx.state
            .metrics
            .requests_total
            .with_label_values(&["u", "refused_request"])
            .get(),
        3
    );
    // The resource was never reserved, so the foreground request needs no
    // fresh sample, and it is released when its reply ends.
    let response = respond(r#"{"model":"m","input":"hi","background":false}"#).await;
    assert_eq!(response.status(), StatusCode::OK);
    axum::body::to_bytes(response.into_body(), 4096)
        .await
        .unwrap();
    assert_eq!(hits.load(Ordering::SeqCst), 1);
    wait_for("the foreground response to release", || {
        work(&ctx).is_empty()
    })
    .await;
    server.abort();
}

/// An Ollama stand-in for an abandoned request. `/api/generate` either holds its
/// headers through a "cold load" (`slow_headers`) or answers at once and
/// holds the rest of its streamed body. Either way the work takes `WORK` and
/// then ends cleanly. It counts requests in progress, the most at once, and
/// requests that ran to their end.
#[derive(Default)]
struct SlowOllama {
    slow_headers: AtomicBool,
    active: AtomicUsize,
    peak: AtomicUsize,
    ended: AtomicUsize,
}

const SLOW_OLLAMA_WORK: Duration = Duration::from_millis(600);

struct InProgress(Arc<SlowOllama>);
impl InProgress {
    fn start(stub: &Arc<SlowOllama>) -> Self {
        let now = stub.active.fetch_add(1, Ordering::SeqCst) + 1;
        stub.peak.fetch_max(now, Ordering::SeqCst);
        Self(stub.clone())
    }
}
impl Drop for InProgress {
    fn drop(&mut self) {
        self.0.active.fetch_sub(1, Ordering::SeqCst);
    }
}

async fn slow_ollama() -> (
    std::net::SocketAddr,
    Arc<SlowOllama>,
    tokio::task::JoinHandle<()>,
) {
    let stub = Arc::new(SlowOllama::default());
    let app = Router::new()
        .route(
            "/api/generate",
            post(|State(stub): State<Arc<SlowOllama>>| async move {
                let progress = InProgress::start(&stub);
                if stub.slow_headers.load(Ordering::SeqCst) {
                    tokio::time::sleep(SLOW_OLLAMA_WORK).await;
                }
                let slow_body = !stub.slow_headers.load(Ordering::SeqCst);
                let body = futures_util::stream::unfold(
                    (0u8, Some(progress)),
                    move |(step, progress)| async move {
                        match step {
                            0 => Some((
                                Ok::<_, std::io::Error>(axum::body::Bytes::from_static(
                                    b"{\"model\":\"m\",\"response\":\"he\",\"done\":false}\n",
                                )),
                                (1, progress),
                            )),
                            1 => {
                                if slow_body {
                                    tokio::time::sleep(SLOW_OLLAMA_WORK).await;
                                }
                                Some((
                                    Ok(axum::body::Bytes::from_static(
                                        b"{\"model\":\"m\",\"response\":\"llo\",\"done\":true}\n",
                                    )),
                                    (2, progress),
                                ))
                            }
                            _ => {
                                if let Some(progress) = progress {
                                    progress.0.ended.fetch_add(1, Ordering::SeqCst);
                                }
                                None
                            }
                        }
                    },
                );
                Response::builder()
                    .header("content-type", "application/x-ndjson")
                    .body(Body::from_stream(body))
                    .unwrap()
            }),
        )
        .with_state(stub.clone());
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let server = tokio::spawn(async move {
        axum::serve(listener, app).await.unwrap();
    });
    (addr, stub, server)
}

/// A borrower whose own deadline expires (a client-side request timeout)
/// closes its connection while the upstream is still
/// working, either before the reply's headers (a cold load) or part way
/// through a streamed reply. A disconnect is not proof that the upstream
/// stopped, so nothing else may be admitted while the work may still run.
/// But the reservation must not outlive verified completion: it is released
/// once the upstream's work has ended, without an administrator.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_borrower_that_times_out_mid_request_does_not_leak_its_reservation() {
    for slow_headers in [true, false] {
        let (target, stub, upstream_server) = slow_ollama().await;
        stub.slow_headers.store(slow_headers, Ordering::SeqCst);
        let ctx = memory_ctx(format!("http://{target}"));
        let state = ctx.state.clone();
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let proxy = listener.local_addr().unwrap();
        let server = tokio::spawn(serve_on(listener, state.clone(), ctx.upstream.clone()));
        let url = format!("http://{proxy}/api/generate");
        let body = r#"{"model":"m","prompt":"hi"}"#;

        // The borrower gives up after 150 ms, well inside the upstream's work.
        let impatient = reqwest::Client::builder()
            .timeout(Duration::from_millis(150))
            .build()
            .unwrap();
        let outcome = match impatient.post(&url).body(body).send().await {
            Ok(response) => response.bytes().await.map(|_| ()),
            Err(error) => Err(error),
        };
        assert!(
            outcome.is_err_and(|error| error.is_timeout()),
            "slow_headers={slow_headers}: the borrower should have timed out"
        );

        // The upstream may still be working: a second job is refused.
        sample(&state);
        let second = reqwest::Client::new()
            .post(&url)
            .body(body)
            .send()
            .await
            .unwrap();
        assert_eq!(
            second.status(),
            StatusCode::SERVICE_UNAVAILABLE,
            "slow_headers={slow_headers}: admitted a second job while the first may still run"
        );

        // Once the upstream's work has had time to end, the reservation is
        // released on its own (with the usual post-job capacity sample).
        let released = tokio::time::timeout(Duration::from_secs(5), async {
            loop {
                sample(&state);
                if work(&ctx).is_empty() {
                    break;
                }
                tokio::time::sleep(Duration::from_millis(50)).await;
            }
        })
        .await;
        assert!(
            released.is_ok(),
            "slow_headers={slow_headers}: the reservation outlived the upstream's work \
             (upstream requests in progress {}, ended {}): {:?}",
            stub.active.load(Ordering::SeqCst),
            stub.ended.load(Ordering::SeqCst),
            work(&ctx)
                .iter()
                .map(|w| format!(
                    "id {} phase {} completion {} model_state {}",
                    w.id, w.phase, w.completion, w.model_state
                ))
                .collect::<Vec<_>>()
        );
        sample(&state);
        let third = reqwest::Client::new()
            .post(&url)
            .body(body)
            .send()
            .await
            .unwrap();
        assert_eq!(third.status(), StatusCode::OK);
        third.bytes().await.unwrap();
        assert_eq!(
            stub.peak.load(Ordering::SeqCst),
            1,
            "two jobs reached the upstream at once"
        );
        server.abort();
        upstream_server.abort();
    }
}

/// A speech-service stand-in: `/status` reports `busy` while a speech
/// request is in progress, and clears it before the reply's last byte, as a
/// service that resets its flag when synthesis ends does.
#[derive(Default)]
struct VoiceStub {
    busy: AtomicBool,
    speeches: AtomicUsize,
}

async fn voice_stub() -> (
    std::net::SocketAddr,
    Arc<VoiceStub>,
    tokio::task::JoinHandle<()>,
) {
    let stub = Arc::new(VoiceStub::default());
    let app = Router::new()
        .route(
            "/status",
            get(|State(stub): State<Arc<VoiceStub>>| async move {
                Json(serde_json::json!({
                    "loaded": true,
                    "busy": stub.busy.load(Ordering::SeqCst),
                    "resident_bytes": 1u64 << 30,
                }))
            }),
        )
        .route(
            "/v1/audio/speech",
            post(|State(stub): State<Arc<VoiceStub>>| async move {
                stub.speeches.fetch_add(1, Ordering::SeqCst);
                stub.busy.store(true, Ordering::SeqCst);
                // Load and synthesis.
                tokio::time::sleep(Duration::from_millis(400)).await;
                stub.busy.store(false, Ordering::SeqCst);
                axum::body::Bytes::from_static(b"ID3-audio")
            }),
        )
        .with_state(stub.clone());
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let server = tokio::spawn(async move {
        axum::serve(listener, app).await.unwrap();
    });
    (addr, stub, server)
}

/// What the background model observer does once per poll.
async fn observe_now(ctx: &ProxyCtx) {
    let runtime = ctx.runtime();
    let models = runtime
        .adapter
        .observe_models(&runtime.observer_http, ctx.upstream.target.as_str())
        .expect("the adapter observes models")
        .await
        .unwrap();
    runtime.models.write().unwrap().record_success(models);
}

/// A narration client sends window 1 the moment window 0's reply
/// ends. The model observer's last sample was taken while window 0 ran and
/// says busy. That sample describes vacancyd's own finished job, so it must
/// not refuse the next window when the upstream now reports idle. Busy work
/// the upstream still reports (the owner's own) is refused as before.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_busy_sample_of_the_finished_job_does_not_refuse_the_next_window() {
    let (target, stub, server) = voice_stub().await;
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
        id = "voice"
        listen = "127.0.0.1:0"
        target = "http://{target}"
        kind = "chatterbox"
        resource = "mem"
        "#
    ))
    .unwrap();
    let state = Arc::new(AppState::new(config).unwrap());
    state.mark_presence_sampled();
    sample(&state);
    let ctx = Arc::new(ProxyCtx {
        upstream: state.config.upstreams[0].clone(),
        state: state.clone(),
    });
    let speak = || {
        handle(
            State(ctx.clone()),
            ConnectInfo("127.0.0.1:1234".parse().unwrap()),
            Request::builder()
                .method("POST")
                .uri("/v1/audio/speech")
                .header("content-type", "application/json")
                .body(Body::from(
                    r#"{"model":"qwen","input":"window","voice":"v","response_format":"mp3"}"#,
                ))
                .unwrap(),
        )
    };

    // Conservative half (passes today): the upstream itself still reports
    // busy, fresh, with no vacancyd job on it. That is refused.
    stub.busy.store(true, Ordering::SeqCst);
    observe_now(&ctx).await;
    let refused = speak().await;
    assert_eq!(refused.status(), StatusCode::SERVICE_UNAVAILABLE);
    stub.busy.store(false, Ordering::SeqCst);
    observe_now(&ctx).await;

    // Window 0. The observer polls once while it runs and sees it busy.
    let window0 = tokio::spawn(speak());
    tokio::time::sleep(Duration::from_millis(150)).await;
    observe_now(&ctx).await;
    assert!(
        ctx.runtime()
            .models
            .read()
            .unwrap()
            .view(Duration::from_secs(10))
            .value
            .unwrap()
            .busy
    );
    let response = window0.await.unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    axum::body::to_bytes(response.into_body(), 1 << 20)
        .await
        .unwrap();
    wait_for("window 0 to release", || work(&ctx).is_empty()).await;
    // The collector's immediate post-job capacity sample.
    sample(&state);
    assert!(
        !stub.busy.load(Ordering::SeqCst),
        "the upstream is idle now"
    );

    // Window 1, sent at once.
    let response = speak().await;
    let status = response.status();
    let body = axum::body::to_bytes(response.into_body(), 1 << 20)
        .await
        .unwrap();
    assert_eq!(
        status,
        StatusCode::OK,
        "window 1 refused against window 0's own busy sample: {}",
        String::from_utf8_lossy(&body)
    );
    assert_eq!(stub.speeches.load(Ordering::SeqCst), 2);
    server.abort();
}
