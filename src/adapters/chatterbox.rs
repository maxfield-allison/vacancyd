//! Chatterbox TTS adapter.
//!
//! The workstation service keeps its HTTP listener on loopback. Borrowers
//! reach `POST /v1/audio/speech` through vacancyd, while voice-library
//! mutations remain owner-only management routes. On a veto, `/admin/unload`
//! drops the model and empties the CUDA allocator. The supervised service may
//! restart itself to interrupt an in-flight Torch generation safely.

use anyhow::{anyhow, ensure, Result};
use futures_util::future::BoxFuture;
use serde_json::Value;

use crate::plugin::adapter::{
    classify_listed, Adapter, AdapterCapabilities, Completion, ReadRoute, RequestClass, Residency,
};

pub struct Chatterbox;

/// Liveness, readiness and the voice catalogue a speech request names a
/// voice from.
const BORROWER_READS: &[ReadRoute] = &[
    ReadRoute::Exact("/healthz"),
    ReadRoute::Exact("/readyz"),
    ReadRoute::Exact("/v1/audio/voices"),
    ReadRoute::Exact("/v1/audio/execution-capabilities"),
    ReadRoute::Child("/v1/audio/executions/"),
    ReadRoute::Child("/v1/audio/execution-audio/"),
];

impl Adapter for Chatterbox {
    fn observes_models(&self) -> bool {
        true
    }

    fn observe_models<'a>(
        &'a self,
        http: &'a reqwest::Client,
        base: &'a str,
    ) -> Option<BoxFuture<'a, Result<crate::lifecycle::Models>>> {
        Some(Box::pin(async move {
            let value = crate::admission::read_json(
                http.get(format!("{}/status", base.trim_end_matches('/'))),
            )
            .await?;
            let loaded = value
                .get("loaded")
                .and_then(Value::as_bool)
                .ok_or_else(|| anyhow!("Chatterbox status is missing loaded"))?;
            let busy = value
                .get("busy")
                .and_then(Value::as_bool)
                .ok_or_else(|| anyhow!("Chatterbox status is missing busy"))?;
            let resident_bytes = value.get("resident_bytes").and_then(Value::as_u64);
            // Legacy Chatterbox serves one fixed model. Compatible voice
            // servers can switch engines; preserve their identity so a
            // loaded Qwen engine cannot fund a cold Breeze load.
            let engine = if loaded {
                value
                    .get("engine")
                    .map(|engine| {
                        engine
                            .as_str()
                            .filter(|name| !name.trim().is_empty())
                            .map(str::to_owned)
                            .ok_or_else(|| {
                                anyhow!("loaded voice status has no valid engine identity")
                            })
                    })
                    .transpose()?
            } else {
                None
            };
            let models = loaded
                .then(|| vacancyd::status::Model {
                    name: engine,
                    state: if busy { "running" } else { "loaded_idle" }.into(),
                    resident_bytes,
                    expires_at: None,
                    context_length: None,
                    digest: None,
                    default_context_length: None,
                })
                .into_iter()
                .collect();
            Ok(crate::lifecycle::Models { busy, models })
        }))
    }

    fn residency(&self) -> Residency {
        Residency::SingleModel
    }

    fn completion(&self, method: &http::Method, path: &str) -> Completion {
        if self.classify(method, path) == RequestClass::Work {
            Completion::ResponseClose
        } else {
            Completion::Unknown
        }
    }

    fn health_path(&self) -> Option<&'static str> {
        Some("/healthz")
    }

    fn kind(&self) -> &'static str {
        "chatterbox"
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
        match (method, path) {
            (&http::Method::POST, "/v1/audio/speech" | "/v1/audio/executions") => RequestClass::Work,
            (&http::Method::POST, "/v1/audio/voices")
            | (&http::Method::DELETE, "/v1/audio/voices")
            | (&http::Method::POST, "/admin/unload")
            // Whether the model is loaded and generating right now, and the
            // memory it holds: the owner's activity, which vacancyd's own
            // observation of it shows only to `read` scope.
            | (&http::Method::GET | &http::Method::HEAD, "/status") => RequestClass::Management,
            _ if execution_read_id(path).is_some() => classify_listed(method, path, BORROWER_READS),
            _ if path.starts_with("/v1/audio/executions/")
                || path.starts_with("/v1/audio/execution-audio/") => {
                    crate::plugin::adapter::classify_unknown(method)
                }
            _ => classify_listed(method, path, BORROWER_READS),
        }
    }

    fn model_of(&self, body: &[u8]) -> Option<String> {
        serde_json::from_slice::<Value>(body)
            .ok()?
            .get("model")?
            .as_str()
            .map(str::to_owned)
    }

    fn canonical_model(&self, name: &str) -> String {
        name.into()
    }

    fn unload_all<'a>(
        &'a self,
        http: &'a reqwest::Client,
        base: &'a str,
    ) -> BoxFuture<'a, Result<usize>> {
        Box::pin(async move {
            let reply = crate::admission::read_json(
                http.post(format!("{}/admin/unload", base.trim_end_matches('/')))
                    .json(&serde_json::json!({})),
            )
            .await?;
            ensure!(
                reply.get("unloaded").and_then(Value::as_bool) == Some(true),
                "Chatterbox did not acknowledge unload"
            );
            let released = reply
                .get("released_models")
                .and_then(Value::as_u64)
                .ok_or_else(|| anyhow!("Chatterbox unload is missing released_models"))?;
            usize::try_from(released).map_err(|_| anyhow!("invalid released_models"))
        })
    }
}

/// Native durable reads name one ASCII operation, never an encoded path or suffix.
fn execution_read_id(path: &str) -> Option<&str> {
    let id = path
        .strip_prefix("/v1/audio/executions/")
        .or_else(|| path.strip_prefix("/v1/audio/execution-audio/"))?;
    (id.len() >= 8
        && id.len() <= 80
        && id
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'_' | b'-')))
    .then_some(id)
}

#[cfg(test)]
mod tests {
    use super::*;
    use http::Method;

    #[test]
    fn durable_voice_work_keeps_response_close_and_only_fixed_result_reads_are_lent() {
        let adapter = Chatterbox;
        assert_eq!(
            adapter.classify(&Method::POST, "/v1/audio/executions"),
            RequestClass::Work
        );
        assert_eq!(
            adapter.completion(&Method::POST, "/v1/audio/executions"),
            Completion::ResponseClose
        );
        assert_eq!(
            adapter
                .model_of(br#"{"model":"breeze","input":{"engine":"breeze"}}"#)
                .as_deref(),
            Some("breeze")
        );
        for path in [
            "/v1/audio/execution-capabilities",
            "/v1/audio/executions/voice-fixture-01",
            "/v1/audio/execution-audio/voice-fixture-01",
        ] {
            for method in [Method::GET, Method::HEAD] {
                assert_eq!(
                    adapter.classify(&method, path),
                    RequestClass::PassThrough,
                    "{method} {path}"
                );
            }
        }
        for path in [
            "/v1/audio/executions/short",
            "/v1/audio/executions/",
            "/v1/audio/executions/voice-fixture-01/audio",
            "/v1/audio/executions/voice%2dfixture-01",
            "/v1/audio/executions/voice-fixture-01%2fextra",
            "/v1/audio/execution-audio/../voice-fixture",
            "/v1/audio/execution-audio/voice.fixture",
            "/v1/audio/execution-capabilities/",
        ] {
            assert_eq!(
                adapter.classify(&Method::GET, path),
                RequestClass::UnknownRead,
                "{path}"
            );
            assert_eq!(
                adapter.classify(&Method::POST, path),
                RequestClass::UnknownRoute,
                "{path}"
            );
        }
    }

    #[test]
    fn classifies_only_speech_as_work() {
        let adapter = Chatterbox;
        assert_eq!(
            adapter.classify(&Method::POST, "/v1/audio/speech"),
            RequestClass::Work
        );
        let listed: Vec<String> = adapter
            .borrower_reads()
            .iter()
            .map(ReadRoute::shown)
            .collect();
        assert_eq!(
            listed,
            [
                "/healthz",
                "/readyz",
                "/v1/audio/voices",
                "/v1/audio/execution-capabilities",
                "/v1/audio/executions/<id>",
                "/v1/audio/execution-audio/<id>"
            ]
        );
        for path in ["/v1/audio/voices", "/healthz", "/readyz"] {
            for method in [Method::GET, Method::HEAD] {
                assert_eq!(
                    adapter.classify(&method, path),
                    RequestClass::PassThrough,
                    "{method} {path}"
                );
            }
        }
        assert_eq!(
            adapter.classify(&Method::GET, adapter.health_path().unwrap()),
            RequestClass::PassThrough
        );
        for (method, path) in [
            (Method::POST, "/v1/audio/voices"),
            (Method::DELETE, "/v1/audio/voices"),
            (Method::POST, "/admin/unload"),
            (Method::GET, "/status"),
            (Method::HEAD, "/status"),
        ] {
            assert_eq!(
                adapter.classify(&method, path),
                RequestClass::Management,
                "{method} {path}"
            );
        }
    }

    #[test]
    fn unlisted_reads_and_other_methods_fail_closed() {
        let adapter = Chatterbox;
        for path in [
            "/v1/audio/voices/owner",
            "/v1/audio/voices/owner.wav",
            "/v1/audio/voices/",
            "/v1/audio/%76oices",
            "/v1/audio/speech",
            "/v1/models",
            "/docs",
            "/openapi.json",
            "/admin/unload",
            "/status/",
        ] {
            for method in [Method::GET, Method::HEAD] {
                assert_eq!(
                    adapter.classify(&method, path),
                    RequestClass::UnknownRead,
                    "{method} {path}"
                );
            }
        }
        for method in [Method::OPTIONS, Method::TRACE, Method::CONNECT] {
            for path in ["/healthz", "/v1/audio/voices", "/v1/audio/speech"] {
                assert_eq!(
                    adapter.classify(&method, path),
                    RequestClass::UnknownRoute,
                    "{method} {path}"
                );
            }
        }
    }

    #[test]
    fn unknown_mutations_and_lookalikes_fail_closed() {
        let adapter = Chatterbox;
        for path in [
            "/v1/audio/speech/extra",
            "/v1/audio/voices/owner",
            "/admin/unload/extra",
        ] {
            assert_eq!(
                adapter.classify(&Method::POST, path),
                RequestClass::UnknownRoute,
                "{path}"
            );
        }
        assert_eq!(
            adapter.classify(&Method::PUT, "/v1/audio/speech"),
            RequestClass::UnknownRoute
        );
    }

    #[test]
    fn extracts_model_without_aliasing_distinct_engines() {
        let adapter = Chatterbox;
        assert_eq!(
            adapter.model_of(br#"{"model":"chatterbox"}"#).as_deref(),
            Some("chatterbox")
        );
        assert_eq!(adapter.canonical_model("breeze"), "breeze");
        assert!(adapter.capabilities().can_unload);
        assert!(adapter.capabilities().reports_model);
    }

    async fn serve_json(body: Value) -> (String, tokio::task::JoinHandle<()>) {
        let app = axum::Router::new().fallback(move || {
            let body = body.clone();
            async move { axum::Json(body) }
        });
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let base = format!("http://{}", listener.local_addr().unwrap());
        (
            base,
            tokio::spawn(async move { axum::serve(listener, app).await.unwrap() }),
        )
    }

    #[tokio::test]
    async fn unload_requires_an_explicit_acknowledgement() {
        let http = reqwest::Client::new();
        let (base, server) =
            serve_json(serde_json::json!({"unloaded": true, "released_models": 1})).await;
        assert_eq!(Chatterbox.unload_all(&http, &base).await.unwrap(), 1);
        server.abort();

        let (base, server) =
            serve_json(serde_json::json!({"unloaded": false, "released_models": 1})).await;
        assert!(Chatterbox.unload_all(&http, &base).await.is_err());
        server.abort();
    }

    #[tokio::test]
    async fn status_reports_single_model_residency() {
        let http = reqwest::Client::new();
        let (base, server) = serve_json(serde_json::json!({
            "loaded": true,
            "busy": false,
            "resident_bytes": 1610612736u64
        }))
        .await;
        let observed = Chatterbox
            .observe_models(&http, &base)
            .unwrap()
            .await
            .unwrap();
        assert!(!observed.busy);
        assert_eq!(observed.models.len(), 1);
        assert_eq!(observed.models[0].name, None);
        assert_eq!(observed.models[0].state, "loaded_idle");
        assert_eq!(observed.models[0].resident_bytes, Some(1610612736));
        server.abort();
    }

    #[tokio::test]
    async fn voice_status_preserves_the_loaded_engine_identity() {
        let http = reqwest::Client::new();
        let (base, server) = serve_json(serde_json::json!({
            "loaded": true, "busy": false, "engine": "qwen", "resident_bytes": 1610612736u64
        }))
        .await;
        let observed = Chatterbox
            .observe_models(&http, &base)
            .unwrap()
            .await
            .unwrap();
        assert_eq!(observed.models[0].name.as_deref(), Some("qwen"));
        assert_ne!(
            Chatterbox.canonical_model("qwen"),
            Chatterbox.canonical_model("breeze")
        );
        server.abort();
    }

    #[tokio::test]
    async fn loaded_voice_with_an_invalid_engine_cannot_claim_legacy_residency() {
        let http = reqwest::Client::new();
        for engine in [
            Value::Null,
            serde_json::json!(""),
            serde_json::json!(" "),
            serde_json::json!(1),
        ] {
            let (base, server) = serve_json(serde_json::json!({
                "loaded": true, "busy": false, "engine": engine
            }))
            .await;
            assert!(Chatterbox
                .observe_models(&http, &base)
                .unwrap()
                .await
                .is_err());
            server.abort();
        }
    }
}
