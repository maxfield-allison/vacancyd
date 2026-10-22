//! The **adapter** seam: speaking one inference server's dialect.
//!
//! An adapter teaches the proxy three things about a server it fronts:
//! which requests are *work* (as opposed to discovery or health), how to
//! find the model name in one, and how to hand the hardware back.
//!
//! The split matters: discovery and health routes must always pass through,
//! so a borrower's health checks keep seeing a live service while lending is
//! closed. A borrower that thinks the host is *down* behaves differently
//! from one that knows it is *busy*, and only the second is true.
//!
//! Classification fails closed for every method. A borrower reaches only the
//! reads an adapter lists in its [`ReadRoute`] table; any other `GET` or
//! `HEAD` could read the owner's data (llama.cpp's `/slots` returns the
//! prompts it is processing), and any other method could run work vacancyd
//! cannot account for. A new upstream route therefore starts refused.
//!
//! To add a server (vLLM, TGI, llama.cpp's newer model-management API,
//! Automatic1111): implement [`Adapter`], register it in
//! [`AdapterRegistry::with_builtins`], document the `kind` string.

use std::collections::HashMap;

use anyhow::{bail, Result};
use futures_util::future::BoxFuture;
use serde::Serialize;

/// What an adapter can do beyond classifying requests. The gate and the
/// yield path branch on these rather than on the adapter's kind, so a new
/// adapter never requires a `match` somewhere else in the daemon.
#[derive(Debug, Clone, Copy, Default, Serialize)]
pub struct AdapterCapabilities {
    /// The server can be told to release its resident models.
    pub can_unload: bool,
    /// Request bodies carry a model name we can gate per-model on.
    pub reports_model: bool,
    /// Queued or running work named by its tracking id can be stopped at the
    /// upstream. Adapters that complete on response close are cancelled by
    /// closing the proxied response instead and leave this false.
    pub can_cancel: bool,
}

/// What [`Adapter::cancel`] established about each tracking id it was given.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct CancelOutcome {
    /// Neither queued nor running any more: removed before it started, or
    /// already ended. Nothing further will run for these ids.
    pub stopped: Vec<String>,
    /// Running when cancelled and asked to stop. Completion evidence from
    /// [`Adapter::completed`] confirms when it has.
    pub interrupted: Vec<String>,
    /// Still queued or running, with the reason cancellation failed.
    pub failed: Vec<(String, String)>,
    /// Running, and deliberately not stopped because this upstream cannot
    /// stop one job without risking another, with the reason. The work keeps
    /// its reservation until its completion is observed.
    pub left_running: Vec<(String, String)>,
    /// Unacknowledged ids the upstream showed queued or running during
    /// cancellation, which proves it accepted a submission under them.
    pub found: Vec<String>,
}

/// How an adapter's model observation proves that a request can reuse
/// memory the upstream already holds, instead of loading into free memory.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub enum Residency {
    /// No proof is available; every request is charged a cold load.
    #[default]
    None,
    /// Observation rows name loaded models with a keep-alive expiry and the
    /// context they were loaded with (Ollama `/api/ps`).
    NamedModels,
    /// The server holds one model at a time. An unnamed idle observation
    /// proves a fixed served model resident (llama.cpp `/slots`); a named
    /// one proves residency only for a request naming that same model.
    SingleModel,
    /// The server's allocator holds memory it reuses and evicts itself for
    /// the next job; the observation row's `resident_bytes` reports it
    /// (ComfyUI `torch_vram_total`).
    AllocatorPool,
}

/// The request routes of a dialect that may resolve runner defaults
/// differently, so what one taught about loaded contexts is not applied to
/// the other.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum RouteFamily {
    /// The upstream's own API (Ollama `/api/*`).
    Native,
    /// The OpenAI-compatible API (Ollama `/v1/*`).
    OpenAi,
}

/// The runner parameters a request sets, as far as they decide which
/// context a model is loaded with.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct RunnerRequest {
    pub family: RouteFamily,
    /// An explicit context length, or `None` for the upstream's default.
    pub num_ctx: Option<u64>,
}

/// How the proxy treats a request: admit it as work, forward it, or refuse
/// it before it reaches the upstream.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RequestClass {
    /// A known inference/submission endpoint. Subject to admission.
    Work,
    /// A listed discovery or health read, a result the borrower asked for, or
    /// a cheap operation that runs no model. Forwarded without admission.
    PassThrough,
    /// Changes the upstream's model store, state or files, or reads the
    /// owner's data on it, rather than using the hardware. Forwarded without
    /// admission only when the upstream sets `management = "allow"`;
    /// otherwise refused with 403.
    Management,
    /// A `GET` or `HEAD` the adapter does not list as a borrower read. It may
    /// read the owner's data, so it is treated as management: forwarded
    /// without admission only when the upstream sets `management = "allow"`,
    /// otherwise refused with 403.
    UnknownRead,
    /// Any other method on a route the adapter does not recognize. Refused
    /// before forwarding whatever the upstream's policy: admitting it would
    /// hold the resource with no way to tell when the work ended, and
    /// passing it through would let a new upstream route bypass admission.
    UnknownRoute,
}

impl RequestClass {
    pub fn requires_admission(self) -> bool {
        matches!(self, Self::Work)
    }
}

/// The refusal reason for an OpenAI Responses request (`POST /v1/responses`)
/// that asks for background mode, or whose body cannot be read to rule it
/// out. A background response answers `queued` at once and keeps generating
/// after its HTTP response has ended, so release on response close would
/// hand the resource back while the work still ran (vLLM implements this;
/// Ollama and llama.cpp ignore the flag and answer synchronously). Anything
/// other than an absent, `null` or `false` flag is refused, because servers
/// that validate leniently read `"true"` or `1` as true.
pub fn responses_background(path: &str, body: &[u8]) -> Option<String> {
    if path != "/v1/responses" {
        return None;
    }
    let Ok(serde_json::Value::Object(body)) = serde_json::from_slice(body) else {
        return Some(
            "vacancyd admits a Responses request only when its body is a JSON object it can read, so it can rule out background mode"
                .into(),
        );
    };
    match body.get("background") {
        None | Some(serde_json::Value::Null | serde_json::Value::Bool(false)) => None,
        Some(_) => Some(
            "background Responses are not supported through vacancyd: the upstream would keep generating after the reply, while vacancyd released the hardware. Send the request without \"background\", streaming it if the client needs progress"
                .into(),
        ),
    }
}

/// Conservative classification for a route an adapter does not recognize:
/// an unlisted read may expose the owner's data, and any other method may
/// run work.
pub fn classify_unknown(method: &http::Method) -> RequestClass {
    if is_read(method) {
        RequestClass::UnknownRead
    } else {
        RequestClass::UnknownRoute
    }
}

/// `GET` and `HEAD`, the methods a borrower reads with.
pub fn is_read(method: &http::Method) -> bool {
    matches!(*method, http::Method::GET | http::Method::HEAD)
}

/// A read a borrower may make without admission or management permission.
///
/// Matching is exact, so a lookalike (`/api/tagsx`, `/health/`) is not a
/// listed read. Every form the upstream's router could see must match: the
/// path as forwarded and its percent-decoded form, so `/%73lots` is not read
/// as anything but an unlisted route.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ReadRoute {
    /// Exactly this path.
    Exact(&'static str),
    /// This prefix followed by one non-empty path segment, such as a model
    /// or prompt ID.
    Child(&'static str),
}

impl ReadRoute {
    /// The route as documented, with `<id>` for a child segment.
    pub fn shown(&self) -> String {
        match self {
            Self::Exact(path) => (*path).to_owned(),
            Self::Child(prefix) => format!("{prefix}<id>"),
        }
    }

    fn matches_form(&self, path: &str) -> bool {
        match self {
            Self::Exact(exact) => path == *exact,
            Self::Child(prefix) => path.strip_prefix(prefix).is_some_and(is_safe_segment),
        }
    }

    /// Whether `path`, as forwarded and as the upstream decodes it, is this
    /// route.
    pub fn matches(&self, path: &str) -> bool {
        self.matches_form(path) && self.matches_form(&percent_decoded(path))
    }
}

/// `PassThrough` for a `GET` or `HEAD` on one of `reads`, and the fail-closed
/// [`classify_unknown`] class for anything else.
pub fn classify_listed(method: &http::Method, path: &str, reads: &[ReadRoute]) -> RequestClass {
    if is_read(method) && reads.iter().any(|read| read.matches(path)) {
        RequestClass::PassThrough
    } else {
        classify_unknown(method)
    }
}

/// One path segment that names a single resource: not empty, no separator
/// (`/`, or `\` for servers that treat it as one) and not a dot segment,
/// including percent-encoded dots, which URL parsing resolves.
pub fn is_safe_segment(segment: &str) -> bool {
    if segment.is_empty() || segment.contains(['/', '\\']) {
        return false;
    }
    !matches!(
        segment.to_ascii_lowercase().as_str(),
        "." | ".." | "%2e" | "%2e%2e" | ".%2e" | "%2e."
    )
}

/// `path` with every valid `%XX` escape decoded once, as upstream routers
/// decode a path before matching it. Invalid escapes are kept as written.
pub fn percent_decoded(path: &str) -> String {
    let bytes = path.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        let hex = |b: u8| (b as char).to_digit(16);
        if bytes[i] == b'%' && i + 2 < bytes.len() {
            if let (Some(high), Some(low)) = (hex(bytes[i + 1]), hex(bytes[i + 2])) {
                out.push((high * 16 + low) as u8);
                i += 3;
                continue;
            }
        }
        out.push(bytes[i]);
        i += 1;
    }
    String::from_utf8_lossy(&out).into_owned()
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Completion {
    ResponseClose,
    Tracked,
    #[default]
    Unknown,
}

pub trait Adapter: Send + Sync {
    fn observes_models(&self) -> bool {
        false
    }
    fn observe_models<'a>(
        &'a self,
        _http: &'a reqwest::Client,
        _base: &'a str,
    ) -> Option<BoxFuture<'a, Result<crate::lifecycle::Models>>> {
        None
    }
    /// Queue-based adapters require a fresh empty upstream queue before reuse.
    fn requires_idle_observation(&self) -> bool {
        false
    }
    /// How this dialect's model observation proves residency.
    fn residency(&self) -> Residency {
        Residency::None
    }
    /// What a gated request asks of the runner that would serve it, for
    /// comparison with a resident model. `None` means the request may make
    /// the upstream reload a resident model whatever its context (it sets
    /// another runner option, or cannot be parsed), so a
    /// [`Residency::NamedModels`] credit does not apply.
    fn runner_request(&self, path: &str, body: &[u8]) -> Option<RunnerRequest> {
        let _ = (path, body);
        None
    }

    /// Only known synchronous work may release at a clean upstream EOF.
    fn completion(&self, method: &http::Method, path: &str) -> Completion {
        let _ = (method, path);
        Completion::Unknown
    }
    fn tracking_id(&self, _body: &[u8]) -> Option<String> {
        None
    }
    /// The submission body to forward with `id` as its tracking id, for a
    /// dialect that lets the client choose it. vacancyd records the id before
    /// forwarding, so a submission whose reply never arrives can still be
    /// tracked and cancelled, and a borrower cannot name someone else's job.
    /// `None` means the dialect cannot carry a chosen id; `Some(Err)` is a
    /// message for a body that cannot be a valid submission at all, which is
    /// refused before anything is reserved or forwarded.
    fn with_tracking_id(&self, _body: &[u8], _id: &str) -> Option<Result<Vec<u8>, String>> {
        None
    }
    /// Why a gated request must be refused before anything is reserved or
    /// forwarded, although the upstream would accept it: it asks for work
    /// that outlives its HTTP response in a way this adapter cannot follow
    /// to its end, so admitting it would release the resource while the
    /// work still ran. `None` admits the request as usual.
    fn unsupported_request(&self, path: &str, body: &[u8]) -> Option<String> {
        let _ = (path, body);
        None
    }
    /// Positive, dialect-specific proof that a tracked submission was never
    /// queued. HTTP failure alone does not establish this (for example, a
    /// gateway can fail after the upstream accepted the work).
    fn submission_rejected(&self, _status: http::StatusCode, _body: &[u8]) -> bool {
        false
    }
    fn completed<'a>(
        &'a self,
        _http: &'a reqwest::Client,
        _base: &'a str,
        _id: &'a str,
    ) -> BoxFuture<'a, Result<bool>> {
        Box::pin(async { Ok(false) })
    }

    /// Whether one consistent upstream snapshot shows the job `id` neither
    /// queued nor running. `Ok(false)` means it is queued or running, or that
    /// the dialect cannot tell. The tracker uses this only for jobs known to
    /// have been queued under `id`, or never queued at all (see
    /// [`Adapter::keeps_assigned_ids`]), together with a missing history
    /// entry, and only when it holds on two consecutive checks.
    fn queue_absent<'a>(
        &'a self,
        _http: &'a reqwest::Client,
        _base: &'a str,
        _id: &'a str,
    ) -> BoxFuture<'a, Result<bool>> {
        Box::pin(async { Ok(false) })
    }

    /// Whether this upstream queues a submission under the id
    /// [`Adapter::with_tracking_id`] gave it, rather than one of its own.
    /// Where it does, a submission whose connection failed before any reply,
    /// and which is then absent from the queue and history, was never
    /// queued. `Ok(false)` means it may not, or that the dialect cannot tell;
    /// an error means the upstream could not be asked.
    fn keeps_assigned_ids<'a>(
        &'a self,
        _http: &'a reqwest::Client,
        _base: &'a str,
    ) -> BoxFuture<'a, Result<bool>> {
        Box::pin(async { Ok(false) })
    }

    /// Stop borrowed work this daemon admitted, named by the ids
    /// [`Adapter::tracking_id`] returned for it. The owner may use the same
    /// server directly, so an implementation must never stop work that these
    /// ids do not name. Adapters whose `can_cancel` is false return an error.
    ///
    /// `unacknowledged` lists those of `ids` that vacancyd assigned but the
    /// upstream never confirmed. An implementation reports each in
    /// [`CancelOutcome::found`] if the upstream shows it queued or running
    /// before anything is cancelled; without that, its absence afterwards
    /// proves nothing.
    ///
    /// This method, [`Adapter::completed`] and [`Adapter::queue_absent`] bound
    /// their own requests, with limits that follow the work each one waits
    /// on. Callers add only a generous backstop, so that a large but healthy
    /// upstream reply on a busy host is not cut off by a fixed budget.
    fn cancel<'a>(
        &'a self,
        _http: &'a reqwest::Client,
        _base: &'a str,
        _ids: &'a [String],
        _unacknowledged: &'a [String],
    ) -> BoxFuture<'a, Result<CancelOutcome>> {
        Box::pin(async { bail!("this upstream has no cancellation API") })
    }

    /// Whether the upstream holds a job, running or queued, that is none of
    /// `ours` (the ids of every job vacancyd admitted there). `Ok(None)`
    /// means the dialect cannot tell. The reclaim asks after cancelling its
    /// own work and before unloading, since an unload would also reach the
    /// owner's work. Like [`Adapter::cancel`], it bounds its own reads.
    fn holds_foreign_work<'a>(
        &'a self,
        _http: &'a reqwest::Client,
        _base: &'a str,
        _ours: &'a [String],
    ) -> BoxFuture<'a, Result<Option<bool>>> {
        Box::pin(async { Ok(None) })
    }

    /// The `kind` string used in config, e.g. `"ollama"`.
    fn kind(&self) -> &'static str;

    fn capabilities(&self) -> AdapterCapabilities;

    /// Side-effect-free HTTP endpoint for diagnostic reachability, not proof
    /// of job completion or resource availability. No borrower token is sent.
    fn health_path(&self) -> Option<&'static str> {
        None
    }

    /// Classify this exact method/path pair. A `GET` or `HEAD` passes through
    /// only if it is one of [`Adapter::borrower_reads`]; anything the adapter
    /// does not recognize takes the fail-closed class from
    /// [`classify_unknown`] (or [`classify_listed`], which applies both).
    fn classify(&self, method: &http::Method, path: &str) -> RequestClass;

    /// Classify a request whose query string can change what a route
    /// returns. The proxy calls this; the default ignores the query.
    fn classify_request(
        &self,
        method: &http::Method,
        path: &str,
        query: Option<&str>,
    ) -> RequestClass {
        let _ = query;
        self.classify(method, path)
    }

    /// The `GET`/`HEAD` routes a borrower may read without admission or
    /// management permission: discovery, health and the borrower's own
    /// results. Every other read is refused unless the upstream allows
    /// management.
    fn borrower_reads(&self) -> &'static [ReadRoute] {
        &[]
    }

    /// Extract the model name from a gated request body, if the dialect
    /// carries one.
    fn model_of(&self, body: &[u8]) -> Option<String> {
        let _ = body;
        None
    }

    /// The single spelling for every name the upstream resolves to the same
    /// model. Thresholds, footprint priors and residency matching compare
    /// this form, so a client's choice of spelling cannot select a different
    /// capacity budget. Dialects without aliases keep the name unchanged.
    fn canonical_model(&self, name: &str) -> String {
        name.to_owned()
    }

    /// Request release of resident models. Returns acknowledged unloads, not a
    /// measurement of recovered memory. Queue-based adapters may acknowledge
    /// one request without reporting a model count. Rejection is an error.
    /// Adapters whose `can_unload` is false should return `Ok(0)`.
    fn unload_all<'a>(
        &'a self,
        http: &'a reqwest::Client,
        base: &'a str,
    ) -> BoxFuture<'a, Result<usize>> {
        let _ = (http, base);
        Box::pin(async { Ok(0) })
    }
}

pub type AdapterFactory = fn() -> Box<dyn Adapter>;

pub struct AdapterRegistry {
    factories: HashMap<&'static str, AdapterFactory>,
}

impl AdapterRegistry {
    pub fn new() -> Self {
        Self {
            factories: HashMap::new(),
        }
    }

    pub fn with_builtins() -> Self {
        let mut r = Self::new();
        r.register("chatterbox", || {
            Box::new(crate::adapters::chatterbox::Chatterbox)
        });
        r.register("ollama", || {
            Box::new(crate::adapters::ollama::Ollama::default())
        });
        r.register("openai", || Box::new(crate::adapters::openai::OpenAi));
        r.register("comfyui", || Box::new(crate::adapters::comfyui::ComfyUi));
        r.register("llamacpp", || Box::new(crate::adapters::openai::LlamaCpp));
        r
    }

    pub fn register(&mut self, kind: &'static str, f: AdapterFactory) {
        self.factories.insert(kind, f);
    }

    pub fn build(&self, kind: &str) -> Result<Box<dyn Adapter>> {
        match self.factories.get(kind) {
            Some(f) => Ok(f()),
            None => bail!(
                "unknown upstream kind '{}' (known: {})",
                kind,
                self.kinds().join(", ")
            ),
        }
    }

    pub fn kinds(&self) -> Vec<&'static str> {
        let mut k: Vec<_> = self.factories.keys().copied().collect();
        k.sort_unstable();
        k
    }
}

impl Default for AdapterRegistry {
    fn default() -> Self {
        Self::with_builtins()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn builtins_are_registered() {
        let r = AdapterRegistry::with_builtins();
        assert_eq!(
            r.kinds(),
            vec!["chatterbox", "comfyui", "llamacpp", "ollama", "openai"]
        );
    }

    #[test]
    fn unknown_kind_is_a_helpful_error() {
        let Err(e) = AdapterRegistry::with_builtins().build("vllm") else {
            panic!("unknown kind must not build");
        };
        let err = e.to_string();
        assert!(err.contains("unknown upstream kind 'vllm'"), "{err}");
        assert!(err.contains("ollama"), "{err}");
    }

    #[test]
    fn unknown_routes_fail_closed_for_every_method() {
        for method in [
            http::Method::POST,
            http::Method::PUT,
            http::Method::PATCH,
            http::Method::DELETE,
            http::Method::OPTIONS,
            http::Method::TRACE,
            http::Method::CONNECT,
            http::Method::from_bytes(b"PROPFIND").unwrap(),
        ] {
            assert_eq!(
                classify_unknown(&method),
                RequestClass::UnknownRoute,
                "{method}"
            );
        }
        for method in [http::Method::GET, http::Method::HEAD] {
            assert_eq!(classify_unknown(&method), RequestClass::UnknownRead);
        }
        for class in [RequestClass::UnknownRead, RequestClass::UnknownRoute] {
            assert!(!class.requires_admission());
        }
    }

    #[test]
    fn listed_reads_match_exactly_in_every_form_the_upstream_sees() {
        let reads = [ReadRoute::Exact("/api/tags"), ReadRoute::Child("/history/")];
        let class = |method: http::Method, path: &str| classify_listed(&method, path, &reads);
        assert_eq!(
            class(http::Method::GET, "/api/tags"),
            RequestClass::PassThrough
        );
        assert_eq!(
            class(http::Method::HEAD, "/api/tags"),
            RequestClass::PassThrough
        );
        assert_eq!(
            class(http::Method::GET, "/history/0f2c5a1e-6b1d"),
            RequestClass::PassThrough
        );
        // Only reads pass; the same path with another method is unknown.
        assert_eq!(
            class(http::Method::OPTIONS, "/api/tags"),
            RequestClass::UnknownRoute
        );
        assert_eq!(
            class(http::Method::POST, "/api/tags"),
            RequestClass::UnknownRoute
        );
        for path in [
            "/api/tags/",
            "/api/tagsx",
            "/API/TAGS",
            "/api/%74ags",
            "/history",
            "/history/",
            "/history/a/b",
            "/history/..",
            "/history/%2e%2e",
            "/history/a%2Fb",
            "/history/a%2fb",
            "/history/a%5Cb",
            "/history/a\\b",
            "/historyx/1",
        ] {
            assert_eq!(
                class(http::Method::GET, path),
                RequestClass::UnknownRead,
                "{path}"
            );
        }
        assert_eq!(ReadRoute::Child("/history/").shown(), "/history/<id>");
    }

    #[test]
    fn segments_are_single_and_never_dot_segments() {
        for segment in ["abc", "a.b", "...", "a%20b", "KSampler (Efficient)"] {
            assert!(is_safe_segment(segment), "{segment}");
        }
        for segment in [
            "", ".", "..", "%2E", "%2e%2E", ".%2e", "%2e.", "a/b", "a\\b",
        ] {
            assert!(!is_safe_segment(segment), "{segment}");
        }
        assert_eq!(percent_decoded("/%73lots%2Fx%zz%4"), "/slots/x%zz%4");
    }
}
