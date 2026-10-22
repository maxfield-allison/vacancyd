//! The gated reverse proxy — one listener per upstream.
//!
//! Point the borrower (LiteLLM, a script, anything) at vacancyd's listen
//! port instead of the dedicated upstream. Supported work routes are forwarded
//! after owner-policy and resource admission. A refusal returns **503 with a
//! JSON reason and `Retry-After`** before forwarding that work. Gateway
//! failover depends on its adapter and configuration. Listed discovery and
//! health routes have their own pass-through policy.
//!
//! Inference request bodies are buffered (they are small) so the model name
//! can be read for per-model admission; response bodies stream, so token
//! streaming works unchanged. Buffering is bounded per request and per
//! proxy, and the listener bounds how long a body may take.

use std::net::SocketAddr;
use std::sync::Arc;
use std::time::Duration;

use axum::body::Body;
use axum::extract::{ConnectInfo, Request, State};
use axum::http::{HeaderMap, Method, StatusCode, Uri};
use axum::response::{IntoResponse, Response};
use axum::Router;
use futures_util::{StreamExt, TryStreamExt};
use hyper::upgrade::OnUpgrade;
use hyper_util::rt::TokioIo;
use tracing::{debug, info, warn};

use crate::admission::Acknowledgement;
use crate::config::UpstreamConfig;
use crate::gate;
use crate::state::AppState;
use crate::work_cancellation::{self, ForwardTask, WorkCancellation};

struct ProxyCtx {
    state: Arc<AppState>,
    upstream: UpstreamConfig,
}

impl ProxyCtx {
    fn runtime(&self) -> &crate::state::UpstreamRuntime {
        &self.state.upstreams[&self.upstream.id]
    }

    /// Whether a borrower's own `Authorization` header reaches the upstream.
    /// Never while vacancyd presents its own credential there: configuration
    /// refuses both at once, and a borrower's header must not replace it.
    fn forwards_authorization(&self) -> bool {
        self.upstream.forward_authorization && self.runtime().credential.is_none()
    }
}

pub async fn serve(state: Arc<AppState>, upstream: UpstreamConfig) -> anyhow::Result<()> {
    let listen = upstream.listen.clone();
    let listener = crate::listener::bind(&format!("proxy '{}'", upstream.id), &listen).await?;
    serve_on(listener, state, upstream).await
}

async fn serve_on(
    listener: tokio::net::TcpListener,
    state: Arc<AppState>,
    upstream: UpstreamConfig,
) -> anyhow::Result<()> {
    let listen = listener.local_addr()?;
    let ctx = Arc::new(ProxyCtx { state, upstream });
    let app = Router::new().fallback(handle).with_state(ctx.clone());
    ctx.state
        .ready_upstreams
        .write()
        .unwrap()
        .insert(ctx.upstream.id.clone());
    info!(
        upstream = ctx.upstream.id,
        listen = %listen,
        target = %ctx.upstream.target,
        "proxy listening"
    );
    let limits = ctx.state.config.server.limits.clone();
    crate::listener::serve(listener, app, &limits).await
}

async fn handle(
    State(ctx): State<Arc<ProxyCtx>>,
    ConnectInfo(peer): ConnectInfo<SocketAddr>,
    mut request: Request,
) -> Response {
    let method = request.method().clone();
    let uri = request.uri().clone();
    let headers = request.headers().clone();
    let path = forwarded_path(&uri);
    let listener_port = request
        .extensions()
        .get::<crate::listener::ListenerAddr>()
        .map(|listener| listener.0.port());

    // Authenticate before anything else — before reading the body, before
    // consulting the gate. An unauthorised caller should learn nothing about
    // the machine's state, including whether it currently has vacancy.
    if let Err(denial) = ctx.state.auth.check(
        crate::auth::Scope::Borrow,
        &headers,
        peer.ip(),
        listener_port,
    ) {
        ctx.state
            .metrics
            .auth_total
            .with_label_values(&["borrow", denial.label()])
            .inc();
        warn!(
            upstream = ctx.upstream.id,
            peer = %peer.ip(),
            reason = denial.label(),
            "borrow denied"
        );
        return crate::api::denial_response(&denial);
    }
    ctx.state
        .metrics
        .auth_total
        .with_label_values(&["borrow", "allowed"])
        .inc();
    // The adapter decides what counts as work; the proxy never switches on
    // dialect itself.
    let adapter = &ctx.state.upstreams[&ctx.upstream.id].adapter;
    let class = adapter.classify_request(&method, &path, uri.query());
    if let Some(refused) = route_policy_refusal(&ctx, adapter.kind(), class, &method, &path) {
        discard_rest(&ctx, request.into_body().into_data_stream(), 0);
        return refused;
    }
    let gated = class.requires_admission();
    // Subscribe before the gate reads the veto. Any return after acceptance
    // reaches this ticket, including one racing with the upstream connection.
    let cancellation = gated.then(|| WorkCancellation::for_upstream(&ctx.state, &ctx.upstream));

    // WebSocket upgrades carry progress and queue events for ComfyUI; work is
    // still submitted through the separately classified POST /prompt route.
    // Authenticate and classify the HTTP handshake before turning it into a
    // raw bidirectional tunnel.
    let mut ticket = None;
    if is_websocket_upgrade(&headers) {
        if gated {
            match crate::admission::reserve(
                &ctx.state,
                &ctx.upstream,
                None,
                crate::plugin::adapter::Completion::Unknown,
            ) {
                Ok(reserved) => ticket = Some(reserved),
                Err(refusal) => {
                    ctx.state
                        .metrics
                        .requests_total
                        .with_label_values(&[
                            ctx.upstream.id.as_str(),
                            refusal.reason.outcome_label(),
                        ])
                        .inc();
                    return refusal_response(&refusal);
                }
            }
        }
        let client_upgrade = hyper::upgrade::on(&mut request);
        return forward_websocket(
            ctx,
            method,
            uri,
            headers,
            client_upgrade,
            cancellation,
            ticket,
        )
        .await;
    }

    // Buffer the request body (needed for forwarding either way; inference
    // bodies are prompt-sized), within this request's and this proxy's
    // limits. The reservation is held until the body has been forwarded.
    let (bytes, body_reservation) = match buffer_body(&ctx, &headers, request.into_body()).await {
        Ok(buffered) => buffered,
        Err(refused) => return *refused,
    };

    if gated {
        if let Some(message) = adapter.unsupported_request(&path, &bytes) {
            return refused_request(&ctx, &method, &path, &message, "unsupported_request");
        }
    }

    let completion = adapter.completion(&method, &path);
    // vacancyd chooses a tracked submission's upstream job id itself and
    // records it before forwarding: the job can then be found even if its
    // reply never arrives, and a borrower cannot name a job it does not own.
    let mut assigned = None;
    let bytes = if gated && completion == crate::plugin::adapter::Completion::Tracked {
        let id = match crate::admission::new_tracking_id() {
            Ok(id) => id,
            Err(error) => {
                warn!(upstream = ctx.upstream.id, error = %format!("{error:#}"), "cannot assign a job id");
                return (
                    StatusCode::INTERNAL_SERVER_ERROR,
                    "vacancyd could not assign a job id",
                )
                    .into_response();
            }
        };
        match adapter.with_tracking_id(&bytes, &id) {
            Some(Ok(body)) => {
                assigned = Some(id);
                axum::body::Bytes::from(body)
            }
            Some(Err(message)) => {
                return refused_request(&ctx, &method, &path, &message, "invalid_submission")
            }
            None => bytes,
        }
    } else {
        bytes
    };

    if gated {
        let model = adapter
            .capabilities()
            .reports_model
            .then(|| adapter.model_of(&bytes))
            .flatten();
        let demand = crate::gate::Demand {
            model: model.as_deref(),
            runner: adapter.runner_request(&path, &bytes),
        };
        match crate::admission::reserve_with_fresh_observations(
            &ctx.state,
            &ctx.upstream,
            demand,
            completion,
            assigned.as_deref(),
        )
        .await
        {
            Ok(reserved) => ticket = Some(reserved),
            Err(refusal) => {
                ctx.state
                    .metrics
                    .requests_total
                    .with_label_values(&[ctx.upstream.id.as_str(), refusal.reason.outcome_label()])
                    .inc();
                info!(
                    upstream = ctx.upstream.id,
                    path,
                    model = model.as_deref().unwrap_or(""),
                    reason = ?refusal.reason,
                    "refused: no vacancy"
                );
                return refusal_response(&refusal);
            }
        }
    }

    // Forward to the real service.
    let target = format!(
        "{}{}",
        ctx.upstream.target.as_str().trim_end_matches('/'),
        uri.path_and_query().map(|p| p.as_str()).unwrap_or("/")
    );
    let mut req = ctx.runtime().http.request(method, &target).body(bytes);
    for (name, value) in end_to_end(&headers).iter() {
        // Host must match the target. The length is the forwarded body's,
        // which differs from the borrower's where a job id was assigned.
        if name == axum::http::header::HOST || name == axum::http::header::CONTENT_LENGTH {
            continue;
        }
        // The Authorization header carries *vacancyd's* credential, which the
        // upstream has no business seeing — forwarding it would hand a
        // borrowing token to Ollama and into its logs. Opt in per upstream
        // only where the real service authenticates callers itself.
        if name == axum::http::header::AUTHORIZATION && !ctx.forwards_authorization() {
            continue;
        }
        req = req.header(name, value);
    }
    // A deadline that passed before anything was sent proves the upstream
    // never saw the request, so its reservation is released, not held.
    if work_cancellation::is_cancelled(&cancellation) {
        if let Some(ticket) = ticket.take() {
            ticket.submission_rejected();
        }
        return cancelled_response(&cancellation);
    }
    if completion == crate::plugin::adapter::Completion::Tracked {
        if let Some(ticket) = ticket.take() {
            return submit_tracked(
                &ctx,
                req,
                ticket,
                assigned,
                cancellation,
                &path,
                body_reservation,
            )
            .await;
        }
    }
    // An admitted reply's reservation ends only at the upstream's own end of
    // reply. Run the exchange apart from the borrower's connection, so a
    // borrower that leaves before the headers cannot take that evidence with
    // it: dropping this handler no longer cancels the request it admitted.
    if ticket
        .as_ref()
        .is_some_and(crate::admission::Ticket::ends_at_response_close)
    {
        let exchange = tokio::spawn(exchange_reply(
            ctx.clone(),
            req,
            cancellation,
            body_reservation,
            gated,
            path,
            ticket,
        ));
        return exchange
            .await
            .unwrap_or_else(|_| (StatusCode::BAD_GATEWAY, "forwarding failed").into_response());
    }
    exchange_reply(
        ctx,
        req,
        cancellation,
        body_reservation,
        gated,
        path,
        ticket,
    )
    .await
}

/// Send the buffered request and stream the upstream's reply back. A ticket,
/// if any, ends at the upstream's end of reply; see [`streaming_body`].
async fn exchange_reply(
    ctx: Arc<ProxyCtx>,
    req: reqwest::RequestBuilder,
    cancellation: Option<WorkCancellation>,
    body_reservation: BodyReservation,
    gated: bool,
    path: String,
    mut ticket: Option<crate::admission::Ticket>,
) -> Response {
    let response = tokio::select! {
        biased;
        _ = work_cancellation::cancelled(&cancellation) => return cancelled_response(&cancellation),
        response = req.send() => response,
    };
    // The request, body included, has been sent or has failed.
    drop(body_reservation);
    match response {
        Ok(resp) => {
            if gated {
                ctx.state
                    .metrics
                    .requests_total
                    .with_label_values(&[ctx.upstream.id.as_str(), "admitted"])
                    .inc();
            }
            let status =
                StatusCode::from_u16(resp.status().as_u16()).unwrap_or(StatusCode::BAD_GATEWAY);
            let mut builder = Response::builder().status(status);
            let mut reply_headers = end_to_end(resp.headers());
            relativize_location(&mut reply_headers, ctx.upstream.target.as_str());
            for (name, value) in reply_headers.iter() {
                builder = builder.header(name, value);
            }
            let declared = declared_length(builder.headers_ref());
            let stream = resp.bytes_stream().map_err(std::io::Error::other);
            let follow_for = Duration::from_secs(ctx.upstream.completion_timeout_s.into());
            builder
                .body(streaming_body(
                    stream,
                    declared,
                    cancellation,
                    ticket,
                    follow_for,
                ))
                .unwrap_or_else(|e| {
                    (StatusCode::BAD_GATEWAY, format!("response build: {e}")).into_response()
                })
        }
        Err(e) => {
            ctx.state
                .metrics
                .requests_total
                .with_label_values(&[ctx.upstream.id.as_str(), "upstream_error"])
                .inc();
            // A connect-phase failure proves no request byte reached the
            // upstream, so nothing started. Any later failure is ambiguous
            // and the dropped ticket keeps the reservation held.
            if e.is_connect() {
                if let Some(ticket) = ticket.take() {
                    ticket.submission_rejected();
                }
            }
            // The borrower learns only which upstream failed. The error text
            // names the internal target, and the URL can carry a query string
            // with a credential in it, so neither leaves this machine or
            // enters the log.
            warn!(
                upstream = ctx.upstream.id,
                path,
                error = %error_chain(&e.without_url()),
                "upstream unreachable"
            );
            (
                StatusCode::BAD_GATEWAY,
                format!("upstream {} unreachable", ctx.upstream.id),
            )
                .into_response()
        }
    }
}

/// The reads a borrower may make on this upstream, for a refusal message.
fn readable_routes(ctx: &ProxyCtx) -> String {
    let adapter = &ctx.state.upstreams[&ctx.upstream.id].adapter;
    let routes: Vec<String> = adapter
        .borrower_reads()
        .iter()
        .map(crate::plugin::adapter::ReadRoute::shown)
        .collect();
    if routes.is_empty() {
        "none".into()
    } else {
        routes.join(", ")
    }
}

/// A buffered request body's share of its proxy's body budget. Dropping it
/// returns the share.
type BodyReservation = tokio::sync::OwnedSemaphorePermit;

/// How long a borrower refused for want of body budget is asked to wait.
const BODY_BUDGET_RETRY_AFTER_S: u64 = 2;

/// KiB, rounded up: the unit body budget is reserved in.
fn kib(bytes: usize) -> u32 {
    u32::try_from(bytes.div_ceil(1024)).unwrap_or(u32::MAX)
}

/// Read a request body into memory within the request's size limit and
/// the proxy's body budget, reserving the budget before the bytes arrive.
///
/// A declared `Content-Length` is checked and reserved in full before any
/// byte is read, so an oversized or unaffordable body is refused at once.
/// A chunked body reserves as it grows. How long the body may take is the
/// listener's to enforce; a body it cuts off is refused with `408`.
async fn buffer_body(
    ctx: &ProxyCtx,
    headers: &HeaderMap,
    body: Body,
) -> Result<(axum::body::Bytes, BodyReservation), Box<Response>> {
    let limits = &ctx.state.config.server.limits;
    let limit = limits.max_request_body_bytes();
    let declared = headers
        .get(axum::http::header::CONTENT_LENGTH)
        .and_then(|value| value.to_str().ok())
        .and_then(|value| value.trim().parse::<u64>().ok());
    if declared.is_some_and(|declared| declared > limit as u64) {
        return Err(Box::new(body_too_large(ctx, limits.max_request_body_mib)));
    }
    let mut stream = body.into_data_stream();
    let budget = ctx.state.upstreams[&ctx.upstream.id].body_budget.clone();
    let declared = declared.map_or(0, |declared| declared as usize);
    let Ok(mut reservation) = budget.clone().try_acquire_many_owned(kib(declared)) else {
        discard_rest(ctx, stream, 0);
        return Err(Box::new(body_budget_exhausted(ctx)));
    };
    let mut buffered = Vec::with_capacity(declared);
    while let Some(chunk) = stream.next().await {
        let chunk = match chunk {
            Ok(chunk) => chunk,
            Err(error) => return Err(Box::new(body_unreadable(ctx, &error))),
        };
        let total = buffered.len() + chunk.len();
        if total > limit {
            drop(buffered);
            drop(reservation);
            discard_rest(ctx, stream, total);
            return Err(Box::new(body_too_large(ctx, limits.max_request_body_mib)));
        }
        let needed = kib(total).saturating_sub(reservation.num_permits() as u32);
        if needed > 0 {
            let Ok(more) = budget.clone().try_acquire_many_owned(needed) else {
                drop(buffered);
                drop(reservation);
                discard_rest(ctx, stream, total);
                return Err(Box::new(body_budget_exhausted(ctx)));
            };
            reservation.merge(more);
        }
        buffered.extend_from_slice(&chunk);
    }
    Ok((axum::body::Bytes::from(buffered), reservation))
}

/// Read and drop the rest of a refused request's body in the background, so
/// the client, which may still be sending it, receives the refusal rather
/// than a connection reset. Reading stops after twice
/// `max_request_body_mib` in all (counting the `already` bytes read), or
/// when the listener's body timeouts end it; nothing is kept.
fn discard_rest(ctx: &ProxyCtx, mut stream: axum::body::BodyDataStream, already: usize) {
    let cap = ctx
        .state
        .config
        .server
        .limits
        .max_request_body_bytes()
        .saturating_mul(2);
    tokio::spawn(async move {
        let mut read = already;
        while read < cap {
            match stream.next().await {
                Some(Ok(chunk)) => read = read.saturating_add(chunk.len()),
                Some(Err(_)) | None => return,
            }
        }
    });
}

/// Count a request-body refusal and build its JSON error response.
fn body_refusal(
    ctx: &ProxyCtx,
    status: StatusCode,
    outcome: &str,
    kind: &str,
    message: String,
    extra: &[(axum::http::HeaderName, String)],
) -> Response {
    ctx.state
        .metrics
        .requests_total
        .with_label_values(&[ctx.upstream.id.as_str(), outcome])
        .inc();
    info!(
        upstream = ctx.upstream.id,
        outcome, "refused a request body"
    );
    let body = serde_json::json!({
        "error": { "message": message, "type": kind, "code": kind }
    });
    let mut response = (
        status,
        [(axum::http::header::CONTENT_TYPE, "application/json")],
        body.to_string(),
    )
        .into_response();
    for (name, value) in extra {
        if let Ok(value) = axum::http::HeaderValue::from_str(value) {
            response.headers_mut().insert(name.clone(), value);
        }
    }
    response
}

fn body_too_large(ctx: &ProxyCtx, limit_mib: usize) -> Response {
    body_refusal(
        ctx,
        StatusCode::PAYLOAD_TOO_LARGE,
        "refused_body_too_large",
        "request_body_too_large",
        format!(
            "the request body exceeds {limit_mib} MiB, the largest this proxy accepts \
             (server.limits.max_request_body_mib)"
        ),
        // The rest of the body is discarded, not kept for another request.
        &[(axum::http::header::CONNECTION, "close".into())],
    )
}

fn body_budget_exhausted(ctx: &ProxyCtx) -> Response {
    body_refusal(
        ctx,
        StatusCode::SERVICE_UNAVAILABLE,
        "refused_body_budget",
        "request_body_budget",
        format!(
            "upstream '{}' is already holding as much request-body data as it may \
             (server.limits.max_buffered_body_mib); retry shortly",
            ctx.upstream.id
        ),
        &[
            (
                axum::http::header::RETRY_AFTER,
                BODY_BUDGET_RETRY_AFTER_S.to_string(),
            ),
            (axum::http::header::CONNECTION, "close".into()),
        ],
    )
}

fn body_unreadable(ctx: &ProxyCtx, error: &axum::Error) -> Response {
    if let Some(timeout) = crate::listener::BodyTimeout::find(error) {
        ctx.state
            .metrics
            .requests_total
            .with_label_values(&[ctx.upstream.id.as_str(), "refused_body_timeout"])
            .inc();
        info!(upstream = ctx.upstream.id, %timeout, "refused a request body");
        return crate::listener::body_timeout_response(&timeout.to_string());
    }
    // The client closed the connection or broke the framing: there is no
    // one left to answer, or nothing more to read from them.
    debug!(upstream = ctx.upstream.id, error = %error_chain(error), "request body unreadable");
    (
        StatusCode::BAD_REQUEST,
        [(axum::http::header::CONNECTION, "close")],
        "the request body could not be read",
    )
        .into_response()
}

/// The path the upstream receives. The forwarding URL resolves dot
/// segments, including percent-encoded ones, so `/x/../api/pull` reaches
/// `/api/pull`; routes are classified in that resolved form.
fn forwarded_path(uri: &Uri) -> String {
    reqwest::Url::parse(&format!("http://upstream.invalid{}", uri.path()))
        .map(|url| url.path().to_owned())
        .unwrap_or_else(|_| uri.path().to_owned())
}

/// Refuse, before anything is forwarded or reserved, a route the adapter
/// does not recognize, and a management route or unlisted read this
/// upstream does not allow.
fn route_policy_refusal(
    ctx: &ProxyCtx,
    kind: &str,
    class: crate::plugin::adapter::RequestClass,
    method: &Method,
    path: &str,
) -> Option<Response> {
    use crate::plugin::adapter::RequestClass;
    // A path is echoed back to the borrower that sent it, never to anyone
    // else, and bounded so an error body cannot be inflated by a long URL.
    let shown: String = path.chars().take(256).collect();
    let management_allowed = ctx.upstream.management == crate::config::ManagementPolicy::Allow;
    let (status, outcome, kind_label, message) = match class {
        RequestClass::UnknownRoute => (
            StatusCode::NOT_IMPLEMENTED,
            "refused_route",
            "unrecognized_route",
            format!(
                "vacancyd does not recognize {method} {shown} as a {kind} route, so it refused the \
                 request without forwarding it. A route vacancyd does not recognize could hold the \
                 hardware with no way to tell when its work ended. If this is a real {kind} route, \
                 report the method, path and {kind} version at {}/issues",
                env!("CARGO_PKG_REPOSITORY")
            ),
        ),
        RequestClass::UnknownRead if !management_allowed => (
            StatusCode::FORBIDDEN,
            "refused_management",
            "management_denied",
            format!(
                "{method} {shown} is not one of the routes a borrower may read on this {kind} \
                 server ({}). An unlisted read can return the owner's data, so upstream '{}' \
                 refuses it unless the owner sets management = \"allow\" on this upstream in the \
                 vacancyd configuration. If borrowers need this route, report the path and {kind} \
                 version at {}/issues",
                readable_routes(ctx),
                ctx.upstream.id,
                env!("CARGO_PKG_REPOSITORY")
            ),
        ),
        RequestClass::Management if !management_allowed => (
            StatusCode::FORBIDDEN,
            "refused_management",
            "management_denied",
            format!(
                "{method} {shown} manages this {kind} server or reads its owner's data, and \
                 upstream '{}' does not allow that through vacancyd. The owner can set \
                 management = \"allow\" on this upstream in the vacancyd configuration",
                ctx.upstream.id
            ),
        ),
        _ => return None,
    };
    ctx.state
        .metrics
        .requests_total
        .with_label_values(&[ctx.upstream.id.as_str(), outcome])
        .inc();
    info!(
        upstream = ctx.upstream.id,
        %method,
        path = shown,
        outcome,
        "refused before forwarding"
    );
    let body = serde_json::json!({
        "error": {
            "message": message,
            "type": kind_label,
            "code": kind_label,
            "setting": (outcome == "refused_management").then_some("management"),
        }
    });
    Some(
        (
            status,
            [("Content-Type", "application/json")],
            body.to_string(),
        )
            .into_response(),
    )
}

/// A submission reply: its status, headers and body.
type SubmitReply = (StatusCode, reqwest::header::HeaderMap, Vec<u8>);

/// How a tracked submission's exchange ended, or that it outlived its bound.
type Exchange = Result<Result<SubmitReply, SubmitError>, tokio::time::error::Elapsed>;

/// Longest silence while reading a submission reply's body.
const SUBMISSION_REPLY_STALL: Duration = Duration::from_secs(10);

/// Why a tracked submission's exchange ended without a usable reply.
enum SubmitError {
    /// The connection was refused or never opened: nothing was sent.
    Connect,
    /// Something was sent, and its outcome is unknown. `before_reply`: the
    /// connection failed before any of the reply arrived.
    Lost { reason: String, before_reply: bool },
}

/// Forward a tracked submission (a ComfyUI `/prompt`) and settle its
/// reservation from the reply.
///
/// The exchange runs in a task of its own, so neither the owner-return
/// deadline nor a borrower that disconnects can cut it off between the
/// upstream queueing the job and vacancyd reading the reply. The borrower is
/// still answered at the deadline. The task goes on to record the job, and
/// cancels it if owner-return cancellation reached it in the meantime. The
/// exchange is bounded by `completion_timeout_s`, the bound on tracking.
async fn submit_tracked(
    ctx: &Arc<ProxyCtx>,
    req: reqwest::RequestBuilder,
    ticket: crate::admission::Ticket,
    assigned: Option<String>,
    cancellation: Option<WorkCancellation>,
    path: &str,
    body_reservation: BodyReservation,
) -> Response {
    let (reply, replied) = tokio::sync::oneshot::channel();
    let ctx = ctx.clone();
    let path = path.to_owned();
    tokio::spawn(async move {
        let entry = ticket.entry();
        let bound = Duration::from_secs(u64::from(ctx.upstream.completion_timeout_s));
        let exchange = tokio::time::timeout(bound, async {
            let sent = req.send().await;
            drop(body_reservation);
            let resp = match sent {
                Ok(resp) => resp,
                Err(error) => {
                    let connect = error.is_connect();
                    ctx.state
                        .metrics
                        .requests_total
                        .with_label_values(&[ctx.upstream.id.as_str(), "upstream_error"])
                        .inc();
                    warn!(
                        upstream = ctx.upstream.id,
                        path,
                        error = %error_chain(&error.without_url()),
                        "upstream unreachable"
                    );
                    return Err(if connect {
                        SubmitError::Connect
                    } else {
                        SubmitError::Lost {
                            reason: "the upstream connection failed".into(),
                            before_reply: true,
                        }
                    });
                }
            };
            ctx.state
                .metrics
                .requests_total
                .with_label_values(&[ctx.upstream.id.as_str(), "admitted"])
                .inc();
            let status =
                StatusCode::from_u16(resp.status().as_u16()).unwrap_or(StatusCode::BAD_GATEWAY);
            let headers = resp.headers().clone();
            // Submission replies are small, so their size is capped. How
            // long one takes follows the host's load, so the read is bounded
            // by silence, not by a fixed total; the whole exchange stays
            // within `completion_timeout_s`.
            let read = async {
                let mut resp = resp;
                let mut bytes = Vec::new();
                loop {
                    let chunk = tokio::time::timeout(SUBMISSION_REPLY_STALL, resp.chunk())
                        .await
                        .map_err(|_| {
                            anyhow::anyhow!(
                                "the reply stalled for {} seconds",
                                SUBMISSION_REPLY_STALL.as_secs()
                            )
                        })??;
                    let Some(chunk) = chunk else {
                        break;
                    };
                    anyhow::ensure!(
                        bytes.len() + chunk.len() <= 1024 * 1024,
                        "the reply exceeds 1 MiB"
                    );
                    bytes.extend_from_slice(&chunk);
                }
                Ok::<_, anyhow::Error>(bytes)
            };
            match read.await {
                Ok(bytes) => Ok((status, headers, bytes)),
                Err(error) => Err(SubmitError::Lost {
                    reason: format!("{error:#}"),
                    before_reply: false,
                }),
            }
        })
        .await;
        let (response, cancel) = settle_submission(&ctx, &path, ticket, assigned, exchange);
        let _ = reply.send(response);
        if cancel {
            crate::yield_control::cancel_submitted(&ctx.state, &ctx.upstream, entry).await;
        }
    });
    tokio::select! {
        biased;
        _ = work_cancellation::cancelled(&cancellation) => cancelled_response(&cancellation),
        response = replied => response.unwrap_or_else(|_| {
            (StatusCode::BAD_GATEWAY, "submission reply unavailable; reservation retained").into_response()
        }),
    }
}

/// Settle a tracked submission's reservation from its exchange, and build
/// the borrower's response. Returns whether the caller must now cancel the
/// work, because owner-return cancellation reached it while in flight.
fn settle_submission(
    ctx: &ProxyCtx,
    path: &str,
    ticket: crate::admission::Ticket,
    assigned: Option<String>,
    exchange: Exchange,
) -> (Response, bool) {
    let adapter = &ctx.state.upstreams[&ctx.upstream.id].adapter;
    let upstream = || ctx.upstream.clone();
    // Without a reply naming the job, follow the id vacancyd assigned: its
    // history shows whether a job ran under it. Where nothing was assigned
    // there is nothing to follow, and the reservation waits for an operator.
    let follow_assigned = |ticket: crate::admission::Ticket, acknowledgement| match &assigned {
        Some(id) => ticket.track(upstream(), id.clone(), acknowledgement),
        None => ticket.hold(),
    };
    match exchange {
        Ok(Ok((status, headers, bytes))) => {
            let cancel = if status.is_success() {
                match adapter.tracking_id(&bytes) {
                    Some(id) => {
                        if assigned.as_ref().is_some_and(|assigned| *assigned != id) {
                            info!(
                                upstream = ctx.upstream.id,
                                "the upstream chose its own job id instead of the one vacancyd assigned"
                            );
                        }
                        ticket.track(upstream(), id, Acknowledgement::Confirmed)
                    }
                    None => {
                        warn!(
                            upstream = ctx.upstream.id,
                            path, "submission accepted without a usable job id"
                        );
                        follow_assigned(ticket, Acknowledgement::Unconfirmed)
                    }
                }
            } else if adapter.submission_rejected(status, &bytes) {
                ticket.submission_rejected();
                false
            } else {
                follow_assigned(ticket, Acknowledgement::Unconfirmed)
            };
            let mut builder = Response::builder().status(status);
            let mut reply_headers = end_to_end(&headers);
            relativize_location(&mut reply_headers, ctx.upstream.target.as_str());
            for (name, value) in reply_headers.iter() {
                builder = builder.header(name, value);
            }
            let response = builder.body(Body::from(bytes)).unwrap_or_else(|e| {
                (StatusCode::BAD_GATEWAY, format!("response build: {e}")).into_response()
            });
            (response, cancel)
        }
        Ok(Err(SubmitError::Connect)) => {
            // Nothing reached the upstream, so nothing started.
            ticket.submission_rejected();
            (
                (
                    StatusCode::BAD_GATEWAY,
                    format!("upstream {} unreachable", ctx.upstream.id),
                )
                    .into_response(),
                false,
            )
        }
        Ok(Err(SubmitError::Lost {
            reason,
            before_reply,
        })) => {
            warn!(
                upstream = ctx.upstream.id,
                path, reason, "submission reply unavailable; following the assigned job id"
            );
            // With no reply at all, an upstream that keeps the assigned id
            // queued the job under it, is about to, or never will. Where part
            // of a reply arrived, the job is left to its history, as for any
            // reply that does not name it.
            let acknowledgement = if before_reply {
                Acknowledgement::LostBeforeReply
            } else {
                Acknowledgement::Unconfirmed
            };
            let cancel = follow_assigned(ticket, acknowledgement);
            let message = match &assigned {
                Some(id) => format!(
                    "submission reply unavailable; the job may still run as {id}, and its reservation is kept until its completion is observed"
                ),
                None => "submission reply unavailable; reservation retained".into(),
            };
            ((StatusCode::BAD_GATEWAY, message).into_response(), cancel)
        }
        Err(_) => {
            warn!(
                upstream = ctx.upstream.id,
                path,
                completion_timeout_s = ctx.upstream.completion_timeout_s,
                "no submission reply within the tracking bound; reservation held"
            );
            let cancel = ticket.hold();
            (
                (
                    StatusCode::GATEWAY_TIMEOUT,
                    "no submission reply within the tracking bound; reservation retained",
                )
                    .into_response(),
                cancel,
            )
        }
    }
}

/// Refuse, with `400` and `error.type` `kind`, a request body that cannot be
/// a valid submission (`invalid_submission`) or asks for work vacancyd
/// cannot follow to its end (`unsupported_request`), before anything is
/// reserved or forwarded.
fn refused_request(
    ctx: &ProxyCtx,
    method: &Method,
    path: &str,
    message: &str,
    kind: &'static str,
) -> Response {
    let shown: String = path.chars().take(256).collect();
    ctx.state
        .metrics
        .requests_total
        .with_label_values(&[ctx.upstream.id.as_str(), "refused_request"])
        .inc();
    info!(
        upstream = ctx.upstream.id,
        %method,
        path = shown,
        kind,
        "refused a request body before forwarding"
    );
    let body = serde_json::json!({
        "error": {
            "message": format!("{method} {shown}: {message}"),
            "type": kind,
            "code": kind,
        }
    });
    (
        StatusCode::BAD_REQUEST,
        [("Content-Type", "application/json")],
        body.to_string(),
    )
        .into_response()
}

/// The refusal for work its cancellation ticket ended, naming what ended it:
/// the owner's return, or pressure on one of the work's resources.
fn cancelled_response(cancellation: &Option<WorkCancellation>) -> Response {
    let trigger = work_cancellation::trigger(cancellation).unwrap_or_default();
    let (reason, resource) = if trigger.owner || trigger.resources.is_empty() {
        (gate::RefusalReason::OwnerActive, None)
    } else {
        (
            gate::RefusalReason::ResourcePressured,
            trigger.resources.first().cloned(),
        )
    };
    refusal_response(&gate::Refusal {
        reason,
        detail: "borrowed work was cancelled by the configured reclaim policy".into(),
        resource,
        retry_after_s: 30,
    })
}

/// The body length the borrower's side of the proxy will frame, if any.
fn declared_length(headers: Option<&HeaderMap>) -> Option<u64> {
    headers?
        .get(axum::http::header::CONTENT_LENGTH)?
        .to_str()
        .ok()?
        .trim()
        .parse()
        .ok()
}

/// Forward an upstream body and complete the ticket when the upstream has
/// delivered all of it. With a declared length, the server side stops
/// polling (and drops this body) as soon as it has written that many bytes,
/// so the final byte, not a later end-of-stream poll, marks completion.
fn streaming_body(
    stream: impl futures_util::Stream<Item = Result<axum::body::Bytes, std::io::Error>> + Send + 'static,
    declared: Option<u64>,
    cancellation: Option<WorkCancellation>,
    ticket: Option<crate::admission::Ticket>,
    follow_for: Duration,
) -> Body {
    let (send, receive) = tokio::sync::mpsc::channel(1);
    let producer_cancellation = cancellation.clone();
    // A reply that ends a reservation outlives its borrower: only pass-through
    // traffic is aborted when the borrower stops reading.
    let follows_reply = ticket
        .as_ref()
        .is_some_and(crate::admission::Ticket::ends_at_response_close);
    let task = ForwardTask::new(
        follows_reply,
        tokio::spawn(async move {
            let forward = async move {
                let mut stream = Box::pin(stream);
                let mut ticket = ticket;
                let mut received: u64 = 0;
                loop {
                    match stream.try_next().await {
                        Ok(Some(chunk)) => {
                            received = received.saturating_add(chunk.len() as u64);
                            if declared.is_some_and(|declared| received >= declared) {
                                if let Some(ticket) = ticket.take() {
                                    ticket.response_complete();
                                }
                            }
                            if send.send(Ok(chunk)).await.is_err() {
                                if let Some(ticket) = ticket
                                    .take()
                                    .filter(crate::admission::Ticket::ends_at_response_close)
                                {
                                    follow_abandoned_reply(
                                        stream, declared, received, ticket, follow_for,
                                    )
                                    .await;
                                }
                                return;
                            }
                        }
                        Ok(None) => {
                            if let Some(ticket) = ticket {
                                ticket.response_complete();
                            }
                            return;
                        }
                        Err(error) => {
                            let _ = send.send(Err(error)).await;
                            return;
                        }
                    }
                }
            };
            tokio::select! {
                biased;
                _ = work_cancellation::cancelled(&producer_cancellation) => {},
                _ = forward => {},
            }
            // Owner return still drops the upstream stream at once, holding any
            // ticket; its cancellation deadline never waits for a body poll.
        }),
    );
    let stream = futures_util::stream::unfold(
        (receive, cancellation, task, false),
        |(mut receive, cancellation, task, done)| async move {
            if done {
                return None;
            }
            let (chunk, done) = tokio::select! {
                biased;
                _ = work_cancellation::cancelled(&cancellation) => (
                    Some(Err(std::io::Error::new(std::io::ErrorKind::Interrupted, "borrowed work cancelled by reclaim policy"))), true),
                chunk = receive.recv() => (chunk, false),
            };
            chunk.map(|chunk| (chunk, (receive, cancellation, task, done)))
        },
    );
    Body::from_stream(stream)
}

/// Read a reply whose borrower has gone to its end and discard it, so its
/// reservation ends on the same evidence as any other reply: the upstream
/// ends a synchronous reply only after its work has finished. An error, or
/// the upstream's completion bound, leaves the work held as uncertain.
async fn follow_abandoned_reply<S>(
    mut stream: std::pin::Pin<Box<S>>,
    declared: Option<u64>,
    mut received: u64,
    ticket: crate::admission::Ticket,
    follow_for: Duration,
) where
    S: futures_util::Stream<Item = Result<axum::body::Bytes, std::io::Error>>,
{
    let entry = ticket.id();
    info!(
        entry,
        "borrower left before the reply ended; following the reply to its end"
    );
    let deadline = tokio::time::Instant::now() + follow_for;
    loop {
        match tokio::time::timeout_at(deadline, stream.try_next()).await {
            Ok(Ok(Some(chunk))) => {
                received = received.saturating_add(chunk.len() as u64);
                if declared.is_some_and(|declared| received >= declared) {
                    break;
                }
            }
            Ok(Ok(None)) => break,
            Ok(Err(_)) => {
                warn!(
                    entry,
                    "an abandoned reply ended in an error; reservation held"
                );
                return;
            }
            Err(_) => {
                warn!(
                    entry,
                    "an abandoned reply outlived its completion bound; reservation held"
                );
                return;
            }
        }
    }
    ticket.response_complete();
    info!(entry, "an abandoned reply ended; reservation released");
}

fn is_websocket_upgrade(headers: &HeaderMap) -> bool {
    let has_upgrade_token = headers
        .get_all(axum::http::header::CONNECTION)
        .iter()
        .filter_map(|value| value.to_str().ok())
        .flat_map(|value| value.split(','))
        .any(|token| token.trim().eq_ignore_ascii_case("upgrade"));
    let is_websocket = headers
        .get(axum::http::header::UPGRADE)
        .and_then(|value| value.to_str().ok())
        .is_some_and(|value| value.trim().eq_ignore_ascii_case("websocket"));
    has_upgrade_token && is_websocket
}

async fn forward_websocket(
    ctx: Arc<ProxyCtx>,
    method: Method,
    uri: Uri,
    headers: HeaderMap,
    client_upgrade: OnUpgrade,
    cancellation: Option<WorkCancellation>,
    ticket: Option<crate::admission::Ticket>,
) -> Response {
    let Ok(tunnel_slot) = ctx.state.upstreams[&ctx.upstream.id]
        .tunnels
        .clone()
        .try_acquire_owned()
    else {
        warn!(
            upstream = ctx.upstream.id,
            "websocket refused: tunnel limit reached"
        );
        return (
            StatusCode::SERVICE_UNAVAILABLE,
            [("Retry-After", "5")],
            "too many open websocket tunnels",
        )
            .into_response();
    };
    let outcome = tokio::select! {
        biased;
        _ = work_cancellation::cancelled(&cancellation) => return cancelled_response(&cancellation),
        outcome = tokio::time::timeout(
        Duration::from_secs(10),
        try_forward_websocket(&ctx, method, uri, headers, client_upgrade, cancellation.clone(), (ticket, tunnel_slot)),
        ) => outcome,
    };
    match outcome {
        Ok(Ok(response)) => response,
        outcome => {
            let error = match outcome {
                Ok(Err(error)) => error,
                Err(_) => anyhow::anyhow!("upstream handshake exceeded 10 seconds"),
                Ok(Ok(_)) => unreachable!(),
            };
            ctx.state
                .metrics
                .requests_total
                .with_label_values(&[ctx.upstream.id.as_str(), "upstream_error"])
                .inc();
            warn!(upstream = ctx.upstream.id, error = %format!("{error:#}"), "websocket upstream unreachable");
            (
                StatusCode::BAD_GATEWAY,
                format!("upstream {} websocket unavailable", ctx.upstream.id),
            )
                .into_response()
        }
    }
}

/// Connect bound for a WebSocket upstream, below the 10 second limit on the
/// whole handshake.
const WEBSOCKET_CONNECT_TIMEOUT: Duration = Duration::from_secs(5);

async fn try_forward_websocket(
    ctx: &Arc<ProxyCtx>,
    method: Method,
    uri: Uri,
    headers: HeaderMap,
    client_upgrade: OnUpgrade,
    cancellation: Option<WorkCancellation>,
    (ticket, tunnel_slot): (
        Option<crate::admission::Ticket>,
        tokio::sync::OwnedSemaphorePermit,
    ),
) -> anyhow::Result<Response> {
    // Nothing reaches the upstream until a connection opens, so a failure
    // here proves nothing started: release the reservation, as the HTTP path
    // does. The connect bound sits inside the handshake's overall limit,
    // whose expiry could not tell a slow connect from a started request.
    let connected = async {
        let target = format!(
            "{}{}",
            ctx.upstream.target.as_str().trim_end_matches('/'),
            uri.path_and_query()
                .map(|part| part.as_str())
                .unwrap_or("/")
        );
        let target_url = reqwest::Url::parse(&target)?;
        let host = target_url
            .host_str()
            .ok_or_else(|| anyhow::anyhow!("upstream target has no host"))?;
        let port = target_url
            .port_or_known_default()
            .ok_or_else(|| anyhow::anyhow!("upstream target has no port"))?;
        let target_uri: Uri = target.parse()?;
        let authority = target_uri
            .authority()
            .ok_or_else(|| anyhow::anyhow!("upstream target has no authority"))?
            .clone();
        let upstream_path = target_uri
            .path_and_query()
            .map(|part| part.as_str())
            .unwrap_or("/")
            .to_owned();
        let stream = tokio::time::timeout(
            WEBSOCKET_CONNECT_TIMEOUT,
            tokio::net::TcpStream::connect((host, port)),
        )
        .await
        .map_err(|_| {
            anyhow::anyhow!("upstream connect exceeded {WEBSOCKET_CONNECT_TIMEOUT:?}")
        })??;
        anyhow::Ok((stream, authority, upstream_path))
    }
    .await;
    let (stream, authority, upstream_path) = match connected {
        Ok(connected) => connected,
        Err(error) => {
            if let Some(ticket) = ticket {
                ticket.submission_rejected();
            }
            return Err(error);
        }
    };
    let (mut sender, connection) =
        hyper::client::conn::http1::handshake(TokioIo::new(stream)).await?;
    let upstream_id = ctx.upstream.id.clone();
    tokio::spawn(async move {
        if let Err(error) = connection.with_upgrades().await {
            warn!(upstream = upstream_id, %error, "websocket upstream connection failed");
        }
    });

    let mut builder = hyper::Request::builder()
        .method(method)
        .uri(upstream_path.as_str())
        .header(axum::http::header::HOST, authority.as_str())
        .header(axum::http::header::CONNECTION, "upgrade");
    if let Some(protocol) = headers.get(axum::http::header::UPGRADE) {
        builder = builder.header(axum::http::header::UPGRADE, protocol);
    }
    for (name, value) in end_to_end(&headers).iter() {
        if name == axum::http::header::HOST {
            continue;
        }
        if name == axum::http::header::AUTHORIZATION && !ctx.forwards_authorization() {
            continue;
        }
        builder = builder.header(name, value);
    }
    // The tunnel does not go through the upstream's HTTP client, so it adds
    // the upstream credential that client would have sent.
    if let Some(credential) = &ctx.runtime().credential {
        builder = builder.header(axum::http::header::AUTHORIZATION, credential);
    }
    let request = builder.body(Body::empty())?;
    let mut upstream_response = sender.send_request(request).await?;

    if upstream_response.status() != StatusCode::SWITCHING_PROTOCOLS {
        let (mut parts, body) = upstream_response.into_parts();
        parts.headers = end_to_end(&parts.headers);
        relativize_location(&mut parts.headers, ctx.upstream.target.as_str());
        let declared = declared_length(Some(&parts.headers));
        return Ok(Response::from_parts(
            parts,
            streaming_body(
                Body::new(body)
                    .into_data_stream()
                    .map_err(std::io::Error::other),
                declared,
                cancellation,
                ticket,
                Duration::from_secs(ctx.upstream.completion_timeout_s.into()),
            ),
        ));
    }

    let upstream_upgrade = hyper::upgrade::on(&mut upstream_response);
    let (parts, _) = upstream_response.into_parts();
    let upstream_id = ctx.upstream.id.clone();
    let idle_limit = Duration::from_secs(ctx.state.config.server.limits.tunnel_idle_timeout_s);
    tokio::spawn(async move {
        let _ticket = ticket;
        let _tunnel_slot = tunnel_slot;
        let activity = Activity::new();
        let tunnel = async {
            match tokio::try_join!(client_upgrade, upstream_upgrade) {
                Ok((client, upstream)) => {
                    let mut client = activity.watch(TokioIo::new(client));
                    let mut upstream = activity.watch(TokioIo::new(upstream));
                    if let Err(error) =
                        tokio::io::copy_bidirectional(&mut client, &mut upstream).await
                    {
                        warn!(upstream = upstream_id, %error, "websocket tunnel closed with an error");
                    }
                }
                Err(error) => {
                    warn!(upstream = upstream_id, %error, "websocket upgrade failed");
                }
            }
        };
        tokio::select! {
            biased;
            _ = work_cancellation::cancelled(&cancellation) => {},
            _ = activity.idle_for(idle_limit) => {
                info!(upstream = upstream_id, idle_s = idle_limit.as_secs(), "websocket tunnel idle; closing");
            },
            _ = tunnel => {},
        }
    });

    Ok(Response::from_parts(parts, Body::empty()))
}

/// Headers that describe a single connection rather than the message (RFC
/// 9110 section 7.6.1), and the proxy credentials a client addresses to
/// whoever is proxying it. None of them is forwarded in either direction.
const HOP_BY_HOP: [&str; 9] = [
    "connection",
    "keep-alive",
    "proxy-connection",
    "proxy-authenticate",
    "proxy-authorization",
    "te",
    "trailer",
    "transfer-encoding",
    "upgrade",
];

/// Copy of `headers` without hop-by-hop headers, including any header the
/// sender named in its `Connection` header.
fn end_to_end(headers: &HeaderMap) -> HeaderMap {
    let nominated: Vec<String> = headers
        .get_all(axum::http::header::CONNECTION)
        .iter()
        .filter_map(|value| value.to_str().ok())
        .flat_map(|value| value.split(','))
        .map(|token| token.trim().to_ascii_lowercase())
        .filter(|token| !token.is_empty())
        .collect();
    let mut kept = HeaderMap::with_capacity(headers.len());
    for (name, value) in headers {
        let name_str = name.as_str();
        if HOP_BY_HOP.contains(&name_str) || nominated.iter().any(|token| token == name_str) {
            continue;
        }
        kept.append(name.clone(), value.clone());
    }
    kept
}

/// Turn a redirect to the upstream's own address into a path on the proxy.
///
/// A server that builds absolute redirects from its own listen address would
/// otherwise hand the borrower the internal upstream address, which it cannot
/// reach and should not learn. A `Location` naming any other host is left as
/// the upstream sent it. A loopback name and a loopback IP with the target's
/// scheme and port are the same address.
fn relativize_location(headers: &mut HeaderMap, target: &str) {
    let Some(value) = headers.get(axum::http::header::LOCATION) else {
        return;
    };
    let (Ok(location), Ok(target)) = (
        value
            .to_str()
            .map_err(|_| ())
            .and_then(|text| reqwest::Url::parse(text).map_err(|_| ())),
        reqwest::Url::parse(target),
    ) else {
        // Not UTF-8, or already relative: nothing names the upstream.
        return;
    };
    let is_loopback = |url: &reqwest::Url| {
        url.host_str().is_some_and(|host| {
            let host = host.trim_start_matches('[').trim_end_matches(']');
            host.eq_ignore_ascii_case("localhost")
                || host
                    .parse::<std::net::IpAddr>()
                    .is_ok_and(|ip| ip.is_loopback())
        })
    };
    let same_host = location.host_str() == target.host_str()
        || (is_loopback(&location) && is_loopback(&target));
    if location.scheme() != target.scheme()
        || !same_host
        || location.port_or_known_default() != target.port_or_known_default()
    {
        return;
    }
    // Requests are forwarded to `target` + the borrower's path, so remove
    // the target's own base path to recover the borrower's.
    let base = target.path().trim_end_matches('/');
    let path = location.path();
    let path = match path.strip_prefix(base) {
        Some(rest) if !base.is_empty() && (rest.is_empty() || rest.starts_with('/')) => {
            if rest.is_empty() {
                "/"
            } else {
                rest
            }
        }
        _ => path,
    };
    let mut relative = path.to_owned();
    if let Some(query) = location.query() {
        relative.push('?');
        relative.push_str(query);
    }
    if let Some(fragment) = location.fragment() {
        relative.push('#');
        relative.push_str(fragment);
    }
    if let Ok(value) = axum::http::HeaderValue::from_str(&relative) {
        headers.insert(axum::http::header::LOCATION, value);
    }
}

/// An error and its causes on one line, for the log.
fn error_chain(error: &dyn std::error::Error) -> String {
    let mut text = error.to_string();
    let mut source = error.source();
    while let Some(cause) = source {
        text.push_str(": ");
        text.push_str(&cause.to_string());
        source = cause.source();
    }
    text
}

/// The time of the last byte read on either side of a tunnel.
#[derive(Clone)]
struct Activity {
    started: tokio::time::Instant,
    last_ms: Arc<std::sync::atomic::AtomicU64>,
}

impl Activity {
    fn new() -> Self {
        Self {
            started: tokio::time::Instant::now(),
            last_ms: Arc::new(std::sync::atomic::AtomicU64::new(0)),
        }
    }

    fn watch<T>(&self, io: T) -> Watched<T> {
        Watched {
            io,
            activity: self.clone(),
        }
    }

    fn touch(&self) {
        let now = self.started.elapsed().as_millis() as u64;
        self.last_ms
            .fetch_max(now, std::sync::atomic::Ordering::Relaxed);
    }

    /// Resolves once no data has moved for `limit`.
    async fn idle_for(&self, limit: Duration) {
        loop {
            let last =
                Duration::from_millis(self.last_ms.load(std::sync::atomic::Ordering::Relaxed));
            let deadline = self.started + last + limit;
            if tokio::time::Instant::now() >= deadline {
                return;
            }
            tokio::time::sleep_until(deadline).await;
        }
    }
}

/// An I/O stream that records reads on its [`Activity`].
struct Watched<T> {
    io: T,
    activity: Activity,
}

impl<T: tokio::io::AsyncRead + Unpin> tokio::io::AsyncRead for Watched<T> {
    fn poll_read(
        mut self: std::pin::Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
        buf: &mut tokio::io::ReadBuf<'_>,
    ) -> std::task::Poll<std::io::Result<()>> {
        let before = buf.filled().len();
        let polled = std::pin::Pin::new(&mut self.io).poll_read(cx, buf);
        if buf.filled().len() > before {
            self.activity.touch();
        }
        polled
    }
}

impl<T: tokio::io::AsyncWrite + Unpin> tokio::io::AsyncWrite for Watched<T> {
    fn poll_write(
        mut self: std::pin::Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
        data: &[u8],
    ) -> std::task::Poll<std::io::Result<usize>> {
        std::pin::Pin::new(&mut self.io).poll_write(cx, data)
    }

    fn poll_flush(
        mut self: std::pin::Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
    ) -> std::task::Poll<std::io::Result<()>> {
        std::pin::Pin::new(&mut self.io).poll_flush(cx)
    }

    fn poll_shutdown(
        mut self: std::pin::Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
    ) -> std::task::Poll<std::io::Result<()>> {
        std::pin::Pin::new(&mut self.io).poll_shutdown(cx)
    }
}

fn refusal_response(refusal: &gate::Refusal) -> Response {
    let body = serde_json::json!({
        "error": {
            "message": format!("no vacancy: {}", refusal.detail),
            "type": "no_vacancy",
            "code": refusal.reason,
            "resource": refusal.resource,
        }
    });
    (
        StatusCode::SERVICE_UNAVAILABLE,
        [
            ("Retry-After", refusal.retry_after_s.to_string()),
            ("Content-Type", "application/json".to_string()),
        ],
        body.to_string(),
    )
        .into_response()
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::http::HeaderValue;
    use std::time::Duration;
    use tokio::io::{AsyncReadExt, AsyncWriteExt};

    #[tokio::test]
    async fn work_cancelled_by_resource_pressure_is_not_refused_as_owner_return() {
        use crate::work_cancellation::Trigger;
        let cases = [
            (
                Trigger {
                    owner: false,
                    resources: vec!["gpu0".into()],
                },
                "resource_pressured",
                serde_json::json!("gpu0"),
            ),
            (
                Trigger {
                    owner: true,
                    resources: vec!["gpu0".into()],
                },
                "owner_active",
                serde_json::Value::Null,
            ),
            (
                Trigger {
                    owner: true,
                    resources: Vec::new(),
                },
                "owner_active",
                serde_json::Value::Null,
            ),
        ];
        for (trigger, code, resource) in cases {
            let ticket = Some(WorkCancellation::cancelled_by(trigger));
            let response = cancelled_response(&ticket);
            assert_eq!(response.status(), StatusCode::SERVICE_UNAVAILABLE);
            let body = axum::body::to_bytes(response.into_body(), 4096)
                .await
                .unwrap();
            let body: serde_json::Value = serde_json::from_slice(&body).unwrap();
            assert_eq!(body["error"]["code"], code, "{body}");
            assert_eq!(body["error"]["resource"], resource, "{body}");
            assert_eq!(
                body["error"]["message"],
                "no vacancy: borrowed work was cancelled by the configured reclaim policy",
                "{body}"
            );
        }
    }

    #[tokio::test]
    async fn owner_return_refuses_new_work_and_applies_each_policy_before_headers() {
        for mode in ["cancel", "drain", "finish"] {
            let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
            let target = listener.local_addr().unwrap();
            let (entered, seen) = tokio::sync::oneshot::channel();
            let (complete, completion) = tokio::sync::oneshot::channel();
            let upstream_task = tokio::spawn(async move {
                let (mut socket, _) = listener.accept().await.unwrap();
                read_headers(&mut socket).await;
                let _ = entered.send(());
                if completion.await.is_ok() {
                    let _ = socket.write_all(b"HTTP/1.1 200 OK\r\nContent-Length: 4\r\nConnection: close\r\n\r\ndone").await;
                }
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
                id = "u"
                listen = "127.0.0.1:0"
                target = "http://{target}"
                kind = "ollama"
                resource = "mem"
                on_owner_return = "{mode}"
                drain_timeout_s = 1
            "#
            ))
            .unwrap();
            let state = Arc::new(AppState::new(config).unwrap());
            state.mark_presence_sampled();
            state.resources["mem"]
                .capacity
                .write()
                .unwrap()
                .record_success(crate::plugin::probe::Capacity::default());
            state.resources["mem"]
                .probe_lendable
                .store(true, std::sync::atomic::Ordering::Relaxed);
            let ctx = Arc::new(ProxyCtx {
                upstream: state.config.upstreams[0].clone(),
                state: state.clone(),
            });
            let request = || {
                Request::builder()
                    .method("POST")
                    .uri("/api/generate")
                    .body(Body::empty())
                    .unwrap()
            };
            let peer = ConnectInfo("127.0.0.1:1234".parse().unwrap());
            let accepted = tokio::spawn(handle(State(ctx.clone()), peer, request()));
            tokio::time::timeout(Duration::from_secs(5), seen)
                .await
                .unwrap()
                .unwrap();
            let returned = std::time::Instant::now();
            state.veto.observe(Some("owner".into()), 0);
            let refused = handle(State(ctx), peer, request()).await;
            assert_eq!(refused.status(), StatusCode::SERVICE_UNAVAILABLE, "{mode}");
            let mut complete = Some(complete);
            if mode == "finish" {
                complete.take().unwrap().send(()).unwrap();
            }
            let response = tokio::time::timeout(Duration::from_secs(5), accepted)
                .await
                .unwrap()
                .unwrap();
            if mode == "finish" {
                assert_eq!(response.status(), StatusCode::OK);
                assert_eq!(
                    axum::body::to_bytes(response.into_body(), 1024)
                        .await
                        .unwrap()
                        .as_ref(),
                    b"done"
                );
            } else {
                assert_eq!(response.status(), StatusCode::SERVICE_UNAVAILABLE, "{mode}");
                if mode == "drain" {
                    assert!(returned.elapsed() >= Duration::from_millis(900));
                }
            }
            drop(complete);
            upstream_task.await.unwrap();
        }
    }

    #[tokio::test]
    async fn cancellation_drops_upstream_even_without_a_borrower_body_poll() {
        use std::sync::atomic::{AtomicBool, Ordering};
        struct DropProof(Arc<AtomicBool>);
        impl Drop for DropProof {
            fn drop(&mut self) {
                self.0.store(true, Ordering::SeqCst);
            }
        }
        let dropped = Arc::new(AtomicBool::new(false));
        let proof = DropProof(dropped.clone());
        let source = futures_util::stream::unfold(proof, |proof| async {
            std::future::pending::<()>().await;
            Some((Ok::<_, std::io::Error>(axum::body::Bytes::new()), proof))
        });
        let upstream: UpstreamConfig = toml::from_str(
            r#"
            id = "u"
            listen = "127.0.0.1:1"
            target = "http://127.0.0.1:2"
            kind = "ollama"
            resource = "mem"
            on_owner_return = "cancel"
        "#,
        )
        .unwrap();
        let veto = crate::state::Veto::default();
        let body = streaming_body(
            source,
            None,
            Some(WorkCancellation::new(&veto, &upstream)),
            None,
            Duration::from_secs(60),
        );
        veto.observe(Some("owner".into()), 0);
        tokio::time::timeout(Duration::from_secs(1), async {
            while !dropped.load(Ordering::SeqCst) {
                tokio::task::yield_now().await;
            }
        })
        .await
        .unwrap();
        // Cancellation after response headers must be an interrupted body,
        // never a successful EOF masquerading as completed inference.
        assert!(axum::body::to_bytes(body, 1024).await.is_err());
    }

    #[tokio::test]
    async fn borrower_disconnect_aborts_the_upstream_forwarder() {
        let (dropped, observed) = tokio::sync::oneshot::channel();
        struct DropProof(Option<tokio::sync::oneshot::Sender<()>>);
        impl Drop for DropProof {
            fn drop(&mut self) {
                if let Some(send) = self.0.take() {
                    let _ = send.send(());
                }
            }
        }
        let source = futures_util::stream::unfold(DropProof(Some(dropped)), |proof| async {
            std::future::pending::<()>().await;
            Some((Ok::<_, std::io::Error>(axum::body::Bytes::new()), proof))
        });
        let body = streaming_body(source, None, None, None, Duration::from_secs(60));
        drop(body);
        tokio::time::timeout(Duration::from_secs(1), observed)
            .await
            .unwrap()
            .unwrap();
    }

    async fn read_headers(stream: &mut tokio::net::TcpStream) -> String {
        let mut bytes = Vec::new();
        let mut byte = [0u8; 1];
        while !bytes.ends_with(b"\r\n\r\n") {
            tokio::time::timeout(Duration::from_secs(5), stream.read_exact(&mut byte))
                .await
                .expect("header read timed out")
                .expect("header read failed");
            bytes.push(byte[0]);
            assert!(bytes.len() < 64 * 1024, "headers were unreasonably large");
        }
        String::from_utf8(bytes).expect("test headers should be UTF-8")
    }

    #[test]
    fn websocket_upgrade_requires_both_case_insensitive_headers() {
        let mut headers = HeaderMap::new();
        headers.insert(
            axum::http::header::CONNECTION,
            HeaderValue::from_static("keep-alive, Upgrade"),
        );
        headers.insert(
            axum::http::header::UPGRADE,
            HeaderValue::from_static("WebSocket"),
        );
        assert!(is_websocket_upgrade(&headers));

        headers.remove(axum::http::header::UPGRADE);
        assert!(!is_websocket_upgrade(&headers));
    }

    #[tokio::test]
    async fn websocket_handshake_is_authenticated_and_tunnels_until_upstream_closes() {
        let upstream_listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let upstream_addr = upstream_listener.local_addr().unwrap();
        let upstream_task = tokio::spawn(async move {
            let (mut stream, _) = upstream_listener.accept().await.unwrap();
            let request = read_headers(&mut stream).await;
            assert!(request.contains("GET /ws?client=1 HTTP/1.1"), "{request}");
            assert!(request.to_ascii_lowercase().contains("upgrade: websocket"));
            assert!(!request.to_ascii_lowercase().contains("authorization:"));
            stream
                .write_all(
                    b"HTTP/1.1 101 Switching Protocols\r\n\
                      Connection: Upgrade\r\n\
                      Upgrade: websocket\r\n\
                      Sec-WebSocket-Accept: test-value\r\n\r\n",
                )
                .await
                .unwrap();
            let mut ping = [0u8; 4];
            stream.read_exact(&mut ping).await.unwrap();
            assert_eq!(&ping, b"ping");
            stream.write_all(b"pong").await.unwrap();
        });

        let config: crate::config::Config = toml::from_str(&format!(
            r#"
[server]
listen = "127.0.0.1:0"

[auth]
trust_loopback = false

[[auth.token]]
name = "cluster"
scopes = ["borrow"]
value = "0123456789abcdef"

[owner.gpu_pressure]
enabled = false

[[resource]]
id = "mem"
kind = "system-memory"
enabled = false

[[upstream]]
id = "comfy"
listen = "127.0.0.1:0"
target = "http://{upstream_addr}"
kind = "comfyui"
resource = "mem"
"#
        ))
        .unwrap();
        let upstream = config.upstreams[0].clone();
        let state = Arc::new(AppState::new(config).unwrap());
        let proxy_listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let proxy_addr = proxy_listener.local_addr().unwrap();
        let proxy_task = tokio::spawn(serve_on(proxy_listener, state.clone(), upstream));

        let mut denied = tokio::net::TcpStream::connect(proxy_addr).await.unwrap();
        denied
            .write_all(
                b"GET /ws HTTP/1.1\r\nHost: localhost\r\nConnection: Upgrade\r\nUpgrade: websocket\r\nSec-WebSocket-Version: 13\r\nSec-WebSocket-Key: dGVzdA==\r\n\r\n",
            )
            .await
            .unwrap();
        assert!(read_headers(&mut denied).await.contains("401 Unauthorized"));

        let mut client = tokio::net::TcpStream::connect(proxy_addr).await.unwrap();
        client
            .write_all(
                b"GET /ws?client=1 HTTP/1.1\r\nHost: localhost\r\nConnection: keep-alive, Upgrade\r\nUpgrade: websocket\r\nSec-WebSocket-Version: 13\r\nSec-WebSocket-Key: dGVzdA==\r\nAuthorization: Bearer 0123456789abcdef\r\n\r\n",
            )
            .await
            .unwrap();
        let response = read_headers(&mut client).await;
        assert!(response.contains("101 Switching Protocols"), "{response}");
        client.write_all(b"ping").await.unwrap();
        let mut pong = [0u8; 4];
        client.read_exact(&mut pong).await.unwrap();
        assert_eq!(&pong, b"pong");

        upstream_task.await.unwrap();
        let mut after_close = [0u8; 1];
        let read = tokio::time::timeout(Duration::from_secs(5), client.read(&mut after_close))
            .await
            .expect("client socket remained open after upstream closed")
            .unwrap();
        assert_eq!(read, 0);
        assert_eq!(
            state
                .metrics
                .auth_total
                .with_label_values(&["borrow", "allowed"])
                .get(),
            1
        );
        assert_eq!(
            state
                .metrics
                .auth_total
                .with_label_values(&["borrow", "missing"])
                .get(),
            1
        );
        proxy_task.abort();
    }
}

#[cfg(test)]
#[path = "proxy_admission_tests.rs"]
mod admission_tests;

#[cfg(test)]
#[path = "proxy_hardening_tests.rs"]
mod hardening_tests;

#[cfg(test)]
#[path = "proxy_policy_tests.rs"]
mod policy_tests;
