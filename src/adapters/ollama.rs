//! Ollama adapter — native `/api/*` plus the OpenAI-compatible `/v1/*`.

use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use anyhow::Result;
use futures_util::future::BoxFuture;
use serde_json::Value;
use tracing::info;

use crate::plugin::adapter::{
    classify_listed, is_read, Adapter, AdapterCapabilities, ReadRoute, RequestClass, RouteFamily,
    RunnerRequest,
};

#[derive(Default)]
pub struct Ollama {
    /// What each model digest's own definition fixes its unset-`num_ctx`
    /// context to, read once per digest from `/api/show`.
    definitions: Arc<Mutex<HashMap<String, Definition>>>,
}

/// One digest's `/api/show` lookup.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Definition {
    /// A lookup is running.
    Pending,
    /// The Modelfile's `num_ctx`, or `None` when it sets none.
    Read(Option<u64>),
    /// The lookup failed at this time; it is retried after a minute.
    Failed(Instant),
}

/// Digests whose definitions are remembered per upstream.
const DEFINITIONS_MAX: usize = 256;
const DEFINITION_RETRY: Duration = Duration::from_secs(60);

/// The `num_ctx` a Modelfile fixes, from `/api/show`'s `parameters` text:
/// one `name value` pair per line. `None` when no line sets it, or when
/// lines disagree or cannot be read, so an ambiguous definition proves
/// nothing.
fn modelfile_num_ctx(parameters: &str) -> Option<u64> {
    let mut found = None;
    for line in parameters.lines() {
        let mut parts = line.split_whitespace();
        if parts.next() != Some("num_ctx") {
            continue;
        }
        let value = parts
            .next()
            .and_then(|value| value.parse::<u64>().ok())
            .filter(|n| *n > 0 && parts.next().is_none())?;
        if found.is_some_and(|seen| seen != value) {
            return None;
        }
        found = Some(value);
    }
    found
}

impl Ollama {
    /// The definition read for `digest`, starting a lookup of `name` when
    /// none is known. Lookups run beside the observation, never inside it,
    /// so a slow `/api/show` cannot fail a model observation.
    fn definition(
        &self,
        http: &reqwest::Client,
        base: &str,
        name: &str,
        digest: &str,
    ) -> Option<u64> {
        let mut definitions = self.definitions.lock().unwrap();
        match definitions.get(digest) {
            Some(Definition::Read(num_ctx)) => return *num_ctx,
            Some(Definition::Pending) => return None,
            Some(Definition::Failed(at)) if at.elapsed() < DEFINITION_RETRY => return None,
            _ => {}
        }
        if definitions.len() >= DEFINITIONS_MAX && !definitions.contains_key(digest) {
            definitions.retain(|_, definition| *definition == Definition::Pending);
        }
        definitions.insert(digest.to_owned(), Definition::Pending);
        drop(definitions);
        let (definitions, http, url, name, digest) = (
            self.definitions.clone(),
            http.clone(),
            format!("{}/api/show", base.trim_end_matches('/')),
            name.to_owned(),
            digest.to_owned(),
        );
        tokio::spawn(async move {
            let read = crate::admission::read_json(
                http.post(url).json(&serde_json::json!({ "model": name })),
            )
            .await;
            let definition = match read {
                Ok(show) => Definition::Read(
                    show.get("parameters")
                        .and_then(Value::as_str)
                        .and_then(modelfile_num_ctx),
                ),
                Err(error) => {
                    tracing::debug!(model = name, error = %format!("{error:#}"), "cannot read the model's definition");
                    Definition::Failed(Instant::now())
                }
            };
            definitions.lock().unwrap().insert(digest, definition);
        });
        None
    }
}

/// Health, version and the model catalogue, native and OpenAI-compatible.
/// `HEAD /` is the Ollama CLI's own liveness check. A borrower needs the
/// catalogue to name a model; an owner who keeps private models should lend
/// a separate instance.
const BORROWER_READS: &[ReadRoute] = &[
    ReadRoute::Exact("/"),
    ReadRoute::Exact("/api/version"),
    ReadRoute::Exact("/api/tags"),
    ReadRoute::Exact("/v1/models"),
    ReadRoute::Child("/v1/models/"),
];

/// `options` keys that tune sampling or output length. Ollama applies them
/// per request without reloading the runner. Every other key, including the
/// runner parameters it reloads for (`num_batch`, `num_gpu`, `main_gpu`,
/// `use_mmap`, `num_thread` and older flags such as `low_vram`) and any key
/// this list does not know, is treated as able to force a reload.
const PER_REQUEST_OPTIONS: &[&str] = &[
    "num_keep",
    "seed",
    "num_predict",
    "top_k",
    "top_p",
    "min_p",
    "typical_p",
    "repeat_last_n",
    "temperature",
    "repeat_penalty",
    "presence_penalty",
    "frequency_penalty",
    "stop",
    "tfs_z",
    "mirostat",
    "mirostat_tau",
    "mirostat_eta",
    "penalize_newline",
];

/// The runner parameters a request sets: an explicit `options.num_ctx`, or
/// none, which leaves the context to Ollama's default for the model. Any
/// other runner option, an `options` value on an OpenAI-compatible route, or
/// an unreadable body returns `None`, because the request may then reload a
/// resident model.
fn runner_request(path: &str, body: &[u8]) -> Option<RunnerRequest> {
    let family = match path {
        "/api/chat" | "/api/generate" | "/api/embed" | "/api/embeddings" => RouteFamily::Native,
        "/v1/chat/completions" | "/v1/completions" | "/v1/embeddings" => RouteFamily::OpenAi,
        _ => return None,
    };
    let body = serde_json::from_slice::<Value>(body).ok()?;
    let body = body.as_object()?;
    let Some(options) = body.get("options") else {
        return Some(RunnerRequest {
            family,
            num_ctx: None,
        });
    };
    if family == RouteFamily::OpenAi {
        return None;
    }
    let mut num_ctx = None;
    for (key, value) in options.as_object()? {
        if key == "num_ctx" {
            num_ctx = Some(value.as_u64().filter(|n| *n > 0)?);
        } else if !PER_REQUEST_OPTIONS.contains(&key.as_str()) {
            return None;
        }
    }
    Some(RunnerRequest { family, num_ctx })
}

/// Ollama fills in a default registry host, namespace and tag, so
/// `registry.ollama.ai/library/gemma3:12b`, `library/gemma3:12b` and
/// `gemma3:12b` name one model, as do `gemma3` and `gemma3:latest`. The short
/// tagged form is canonical because it is the name `/api/ps` reports. Other
/// hosts and namespaces are distinct models and keep their full name.
pub(crate) fn canonical_model_name(name: &str) -> String {
    const HOST: &str = "registry.ollama.ai/";
    let mut rest = name.trim();
    if rest.len() >= HOST.len()
        && rest.is_char_boundary(HOST.len())
        && rest[..HOST.len()].eq_ignore_ascii_case(HOST)
    {
        rest = &rest[HOST.len()..];
    }
    if let Some((namespace, model)) = rest.split_once('/') {
        if namespace.eq_ignore_ascii_case("library") && !model.contains('/') {
            rest = model;
        }
    }
    let last = rest.rsplit('/').next().unwrap_or(rest);
    if last.is_empty() || last.contains(':') || last.contains('@') {
        rest.to_owned()
    } else {
        format!("{rest}:latest")
    }
}

impl Adapter for Ollama {
    fn observes_models(&self) -> bool {
        true
    }
    fn residency(&self) -> crate::plugin::adapter::Residency {
        crate::plugin::adapter::Residency::NamedModels
    }
    fn runner_request(&self, path: &str, body: &[u8]) -> Option<RunnerRequest> {
        runner_request(path, body)
    }
    fn observe_models<'a>(
        &'a self,
        http: &'a reqwest::Client,
        base: &'a str,
    ) -> Option<BoxFuture<'a, Result<crate::lifecycle::Models>>> {
        Some(Box::pin(async move {
            let value = crate::admission::read_json(
                http.get(format!("{}/api/ps", base.trim_end_matches('/'))),
            )
            .await?;
            let rows = value
                .get("models")
                .and_then(|v| v.as_array())
                .ok_or_else(|| anyhow::anyhow!("missing models"))?;
            anyhow::ensure!(rows.len() <= 128, "too many resident models");
            let mut models = Vec::new();
            for row in rows {
                let name = row
                    .get("name")
                    .and_then(|v| v.as_str())
                    .ok_or_else(|| anyhow::anyhow!("missing model name"))?;
                anyhow::ensure!(name.len() <= 256, "model name too long");
                let digest: Option<String> = row
                    .get("digest")
                    .and_then(|v| v.as_str())
                    .filter(|v| !v.is_empty())
                    .map(|v| v.chars().take(128).collect());
                models.push(vacancyd::status::Model {
                    name: Some(canonical_model_name(name)),
                    state: "loaded_idle".into(),
                    resident_bytes: row.get("size_vram").and_then(|v| v.as_u64()),
                    expires_at: row
                        .get("expires_at")
                        .and_then(|v| v.as_str())
                        .map(|v| v.chars().take(64).collect()),
                    context_length: row.get("context_length").and_then(|v| v.as_u64()),
                    // Only a digest identifies which definition was read.
                    default_context_length: digest
                        .as_deref()
                        .and_then(|digest| self.definition(http, base, name, digest)),
                    digest,
                });
            }
            Ok(crate::lifecycle::Models {
                models,
                busy: false,
            })
        }))
    }

    fn completion(&self, method: &http::Method, path: &str) -> crate::plugin::adapter::Completion {
        if self.classify(method, path) == RequestClass::Work {
            crate::plugin::adapter::Completion::ResponseClose
        } else {
            crate::plugin::adapter::Completion::Unknown
        }
    }

    fn health_path(&self) -> Option<&'static str> {
        Some("/api/version")
    }
    fn kind(&self) -> &'static str {
        "ollama"
    }

    fn capabilities(&self) -> AdapterCapabilities {
        AdapterCapabilities {
            can_unload: true,
            reports_model: true,
            can_cancel: false,
        }
    }

    fn borrower_reads(&self) -> &'static [ReadRoute] {
        BORROWER_READS
    }

    fn classify(&self, method: &http::Method, path: &str) -> RequestClass {
        // Inference, per Ollama's API reference (docs/api.md) and its
        // OpenAI and Anthropic compatibility pages: the native routes, the
        // OpenAI-compatible routes including the stateless Responses API, and
        // the Anthropic-compatible Messages API. Each answers in one response.
        if *method == http::Method::POST
            && matches!(
                path,
                "/api/chat"
                    | "/api/generate"
                    | "/api/embed"
                    | "/api/embeddings"
                    | "/v1/chat/completions"
                    | "/v1/completions"
                    | "/v1/embeddings"
                    | "/v1/responses"
                    | "/v1/messages"
            )
        {
            return RequestClass::Work;
        }

        // Reads a model's metadata; runs no model.
        if (method, path) == (&http::Method::POST, "/api/show") {
            return RequestClass::PassThrough;
        }

        // The model store. These add, replace, publish or remove the owner's
        // models, and a pull or create can fill the disk. `HEAD` on a blob
        // is the existence check that precedes an upload, and would tell a
        // borrower which files the owner holds. They are the owner's to
        // allow per upstream.
        if matches!(
            (method, path),
            (&http::Method::POST, "/api/create")
                | (&http::Method::POST, "/api/copy")
                | (&http::Method::POST, "/api/pull")
                | (&http::Method::POST, "/api/push")
                | (&http::Method::DELETE, "/api/delete")
        ) || ((*method == http::Method::POST || is_read(method)) && is_blob_path(path))
        {
            return RequestClass::Management;
        }

        // The models loaded now, when each unloads and how much memory it
        // holds: what the owner is running, and from the unload time, when
        // they last used it. vacancyd shows its own reading of this only to
        // `read` scope.
        if is_read(method) && path == "/api/ps" {
            return RequestClass::Management;
        }
        classify_listed(method, path, BORROWER_READS)
    }

    fn model_of(&self, body: &[u8]) -> Option<String> {
        serde_json::from_slice::<Value>(body)
            .ok()?
            .get("model")?
            .as_str()
            .map(canonical_model_name)
    }

    fn canonical_model(&self, name: &str) -> String {
        canonical_model_name(name)
    }

    /// Ollama answers `/v1/responses` synchronously and ignores
    /// `background`, but a client that asks for it expects to poll for a
    /// result Ollama never stores. Refusing it keeps the route's contract
    /// the same on every upstream kind and stays safe if a later Ollama
    /// queues it.
    fn unsupported_request(&self, path: &str, body: &[u8]) -> Option<String> {
        crate::plugin::adapter::responses_background(path, body)
    }

    fn unload_all<'a>(
        &'a self,
        http: &'a reqwest::Client,
        base: &'a str,
    ) -> BoxFuture<'a, Result<usize>> {
        Box::pin(async move {
            let base = base.trim_end_matches('/');
            let ps = crate::admission::read_json(http.get(format!("{base}/api/ps"))).await?;
            let rows = ps["models"]
                .as_array()
                .ok_or_else(|| anyhow::anyhow!("unload inventory is missing models"))?;
            anyhow::ensure!(rows.len() <= 128, "too many models in unload inventory");
            // Validate the whole inventory before making any mutation. A partial
            // or malformed inventory must not masquerade as nothing to unload.
            let models: Vec<&str> = rows
                .iter()
                .map(|row| {
                    row["model"]
                        .as_str()
                        .or(row["name"].as_str())
                        .filter(|name| !name.is_empty() && name.len() <= 256)
                        .ok_or_else(|| anyhow::anyhow!("invalid model in unload inventory"))
                })
                .collect::<Result<_>>()?;
            let n = models.len();
            for model in models {
                // keep_alive 0 is Ollama's documented immediate-unload.
                let body = serde_json::json!({ "model": model, "keep_alive": 0, "stream": false });
                let reply = crate::admission::read_json(
                    http.post(format!("{base}/api/generate")).json(&body),
                )
                .await?;
                anyhow::ensure!(
                    reply.get("done").and_then(Value::as_bool) == Some(true)
                        && reply.get("done_reason").and_then(Value::as_str) == Some("unload")
                        && reply.get("model").and_then(Value::as_str) == Some(model)
                        && reply.get("error").is_none(),
                    "upstream did not acknowledge model unload"
                );
                info!(model, "upstream acknowledged model unload");
            }
            Ok(n)
        })
    }
}

/// `/api/blobs/sha256:<digest>`: `POST` uploads a blob for `/api/create`,
/// `HEAD` checks that it exists.
fn is_blob_path(path: &str) -> bool {
    path.strip_prefix("/api/blobs/").is_some_and(|digest| {
        !digest.is_empty()
            && digest.len() <= 128
            && digest
                .bytes()
                .all(|b| b.is_ascii_alphanumeric() || b == b':' || b == b'-')
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use http::Method;

    #[test]
    fn a_modelfile_num_ctx_is_read_only_when_it_is_unambiguous() {
        assert_eq!(
            modelfile_num_ctx("num_ctx                        32768\nstop \"<|im_end|>\""),
            Some(32768)
        );
        assert_eq!(modelfile_num_ctx("num_keep 24\ntemperature 0.7"), None);
        assert_eq!(modelfile_num_ctx(""), None);
        assert_eq!(modelfile_num_ctx("num_ctx 4096\nnum_ctx 4096"), Some(4096));
        for ambiguous in [
            "num_ctx 4096\nnum_ctx 8192",
            "num_ctx 0",
            "num_ctx -1",
            "num_ctx 4096.5",
            "num_ctx",
            "num_ctx 4096 extra",
            "num_ctx 1\nnum_ctx x",
        ] {
            assert_eq!(modelfile_num_ctx(ambiguous), None, "{ambiguous:?}");
        }
        // A longer key that begins with num_ctx is another parameter.
        assert_eq!(modelfile_num_ctx("num_ctx_max 4096"), None);
    }

    #[test]
    fn background_responses_are_refused_and_foreground_ones_admitted() {
        let a = Ollama::default();
        for body in [
            r#"{"model":"m","input":"hi"}"#,
            r#"{"model":"m","input":"hi","background":false}"#,
            r#"{"model":"m","input":"hi","background":null,"stream":true}"#,
        ] {
            assert_eq!(
                a.unsupported_request("/v1/responses", body.as_bytes()),
                None,
                "{body}"
            );
            assert_eq!(
                crate::adapters::openai::OpenAi
                    .unsupported_request("/v1/responses", body.as_bytes()),
                None
            );
        }
        for body in [
            r#"{"model":"m","background":true}"#,
            r#"{"model":"m","background":true,"stream":true,"store":true}"#,
            r#"{"model":"m","background":"true"}"#,
            r#"{"model":"m","background":1}"#,
            r#"{"model":"m","background":false,"background":true}"#,
            "not json",
            "[]",
            "",
        ] {
            let refused = a.unsupported_request("/v1/responses", body.as_bytes());
            assert!(refused.is_some(), "{body}");
            assert!(crate::adapters::openai::OpenAi
                .unsupported_request("/v1/responses", body.as_bytes())
                .is_some());
            assert!(crate::adapters::openai::LlamaCpp
                .unsupported_request("/v1/responses", body.as_bytes())
                .is_some());
        }
        // Other routes are not the Responses API's to judge.
        assert_eq!(
            a.unsupported_request("/v1/chat/completions", br#"{"background":true}"#),
            None
        );
        assert_eq!(a.unsupported_request("/api/chat", b"not json"), None);
    }

    #[test]
    fn classifies_known_routes_exactly() {
        let a = Ollama::default();
        for path in [
            "/api/chat",
            "/api/generate",
            "/api/embed",
            "/api/embeddings",
            "/v1/chat/completions",
            "/v1/completions",
            "/v1/embeddings",
            "/v1/responses",
            "/v1/messages",
        ] {
            assert_eq!(
                a.classify(&Method::POST, path),
                RequestClass::Work,
                "{path}"
            );
        }
        assert_eq!(
            a.classify(&Method::POST, "/api/show"),
            RequestClass::PassThrough
        );
        for (method, path) in [
            (Method::POST, "/api/create"),
            (Method::POST, "/api/copy"),
            (Method::POST, "/api/pull"),
            (Method::POST, "/api/push"),
            (Method::DELETE, "/api/delete"),
            (Method::POST, "/api/blobs/sha256:29fdb92e57cf0827ded04ae6461b5931d01fa595843f55d36f5b275a52087dd2"),
        ] {
            assert_eq!(
                a.classify(&method, path),
                RequestClass::Management,
                "{method} {path}"
            );
            assert!(!a.classify(&method, path).requires_admission(), "{method} {path}");
        }
        for (method, path) in [
            (Method::HEAD, "/api/blobs/sha256:29fdb92e"),
            (Method::GET, "/api/ps"),
            (Method::HEAD, "/api/ps"),
        ] {
            assert_eq!(
                a.classify(&method, path),
                RequestClass::Management,
                "{method} {path}"
            );
        }
    }

    #[test]
    fn borrowers_read_only_health_and_the_catalogue() {
        let a = Ollama::default();
        let listed: Vec<String> = a.borrower_reads().iter().map(ReadRoute::shown).collect();
        assert_eq!(
            listed,
            [
                "/",
                "/api/version",
                "/api/tags",
                "/v1/models",
                "/v1/models/<id>"
            ]
        );
        for path in [
            "/",
            "/api/version",
            "/api/tags",
            "/v1/models",
            "/v1/models/gemma3:12b",
            a.health_path().unwrap(),
        ] {
            for method in [Method::GET, Method::HEAD] {
                assert_eq!(
                    a.classify(&method, path),
                    RequestClass::PassThrough,
                    "{method} {path}"
                );
            }
        }
    }

    #[test]
    fn unlisted_reads_and_other_methods_fail_closed() {
        let a = Ollama::default();
        for path in [
            "/api/ps/",
            "/api/%70s",
            "/api/tags/",
            "/api/tagsx",
            "/api/show",
            "/api/chat",
            "/api/experimental/future",
            "/v1/models/",
            "/v1/models/a/b",
            "/v1/models/%2e%2e",
            "/v1/models/..%2Fapi%2Fps",
            "/metrics",
            "/slots",
            "/index.html",
        ] {
            for method in [Method::GET, Method::HEAD] {
                assert_eq!(
                    a.classify(&method, path),
                    RequestClass::UnknownRead,
                    "{method} {path}"
                );
            }
        }
        for method in [Method::OPTIONS, Method::TRACE, Method::CONNECT] {
            for path in ["/", "/api/tags", "/api/ps", "/api/chat"] {
                assert_eq!(
                    a.classify(&method, path),
                    RequestClass::UnknownRoute,
                    "{method} {path}"
                );
            }
        }
    }

    #[test]
    fn unknown_mutations_and_route_lookalikes_are_gated() {
        let a = Ollama::default();
        for method in [Method::POST, Method::PUT, Method::PATCH, Method::DELETE] {
            assert_eq!(
                a.classify(&method, "/api/future-inference"),
                RequestClass::UnknownRoute
            );
        }
        assert_eq!(
            a.classify(&Method::POST, "/v1/chat/completions-evil"),
            RequestClass::UnknownRoute
        );
        for path in [
            "/api/pull/extra",
            "/api/blobs/",
            "/api/blobs/sha256:abc/extra",
            "/api/blobs/..",
        ] {
            assert_eq!(
                a.classify(&Method::POST, path),
                RequestClass::UnknownRoute,
                "{path}"
            );
        }
    }

    #[test]
    fn reads_the_model_name() {
        let a = Ollama::default();
        assert_eq!(
            a.model_of(br#"{"model":"gemma3:12b","prompt":"hi"}"#)
                .as_deref(),
            Some("gemma3:12b")
        );
        assert_eq!(a.model_of(b"not json"), None);
    }

    #[test]
    fn runner_requests_distinguish_explicit_default_and_reloading_contexts() {
        let a = Ollama::default();
        let native = |num_ctx| {
            Some(RunnerRequest {
                family: RouteFamily::Native,
                num_ctx,
            })
        };
        let body = |options: &str| format!(r#"{{"model":"gemma3:12b","options":{options}}}"#);
        assert_eq!(
            a.runner_request("/api/chat", body(r#"{"num_ctx":65536}"#).as_bytes()),
            native(Some(65536))
        );
        assert_eq!(
            a.runner_request(
                "/api/generate",
                body(r#"{"num_ctx":8192,"temperature":0.2,"seed":7,"stop":["x"]}"#).as_bytes()
            ),
            native(Some(8192))
        );
        // No num_ctx means Ollama's default for the model.
        for request in [
            body(r#"{}"#),
            body(r#"{"temperature":0.2}"#),
            r#"{"model":"gemma3:12b"}"#.to_owned(),
        ] {
            assert_eq!(
                a.runner_request("/api/chat", request.as_bytes()),
                native(None),
                "{request}"
            );
        }
        for options in [
            r#"{"num_ctx":65536,"num_gpu":99}"#,
            r#"{"num_ctx":65536,"num_batch":1024}"#,
            r#"{"use_mmap":false}"#,
            r#"{"num_ctx":65536,"some_future_runner_option":1}"#,
            r#"{"num_ctx":"65536"}"#,
            r#"{"num_ctx":0}"#,
            r#"[]"#,
        ] {
            assert_eq!(
                a.runner_request("/api/chat", body(options).as_bytes()),
                None,
                "{options}"
            );
        }
        assert_eq!(a.runner_request("/api/chat", b"not json"), None);
        assert_eq!(a.runner_request("/api/show", body("{}").as_bytes()), None);
        // OpenAI-compatible routes use the default context and cannot carry
        // Ollama options; one that tries is not trusted.
        assert_eq!(
            a.runner_request(
                "/v1/chat/completions",
                br#"{"model":"gemma3:12b","temperature":0.2,"max_tokens":64}"#
            ),
            Some(RunnerRequest {
                family: RouteFamily::OpenAi,
                num_ctx: None,
            })
        );
        assert_eq!(
            a.runner_request(
                "/v1/chat/completions",
                body(r#"{"num_ctx":65536}"#).as_bytes()
            ),
            None
        );
    }

    #[test]
    fn model_names_take_the_form_api_ps_reports() {
        for (given, canonical) in [
            ("gemma3:12b", "gemma3:12b"),
            ("registry.ollama.ai/library/gemma3:12b", "gemma3:12b"),
            ("Registry.Ollama.AI/library/gpt-oss:20b", "gpt-oss:20b"),
            ("library/qwen2.5-coder:14b", "qwen2.5-coder:14b"),
            ("gemma3", "gemma3:latest"),
            ("registry.ollama.ai/library/gemma3", "gemma3:latest"),
            ("registry.ollama.ai/someone/model:q4", "someone/model:q4"),
            (
                "hf.co/unsloth/Qwen3.5-4B-GGUF:Q6_K",
                "hf.co/unsloth/Qwen3.5-4B-GGUF:Q6_K",
            ),
            ("hf.co/library/model:q4", "hf.co/library/model:q4"),
            (
                "localhost:5000/team/model",
                "localhost:5000/team/model:latest",
            ),
            ("gemma3@sha256:abc", "gemma3@sha256:abc"),
        ] {
            assert_eq!(canonical_model_name(given), canonical, "{given}");
            assert_eq!(canonical_model_name(canonical), canonical, "{canonical}");
        }
        assert_eq!(
            Ollama::default()
                .model_of(br#"{"model":"registry.ollama.ai/library/gpt-oss:20b"}"#)
                .as_deref(),
            Some("gpt-oss:20b")
        );
    }
}
