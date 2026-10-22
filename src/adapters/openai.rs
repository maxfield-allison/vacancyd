//! Generic OpenAI-compatible adapter — llama-server, vLLM, TGI, LocalAI.
//!
//! No unload capability: these servers hold their model by design. The gate
//! still refuses new work during owner presence, and the OS pages the model
//! out and faults it back in on demand (measured on llama-server: ~2.7 GiB
//! evicted, ~2.6 s to return).

use crate::plugin::adapter::{
    classify_listed, classify_unknown, is_read, Adapter, AdapterCapabilities, ReadRoute,
    RequestClass,
};
use serde_json::Value;

pub struct OpenAi;

/// Model discovery and health, the reads an OpenAI-compatible client makes
/// before it sends work. llama-server serves each under both spellings;
/// vLLM and TGI serve `/health` and `/v1/models`.
const BORROWER_READS: &[ReadRoute] = &[
    ReadRoute::Exact("/v1/models"),
    ReadRoute::Exact("/models"),
    ReadRoute::Exact("/health"),
    ReadRoute::Exact("/v1/health"),
];

impl Adapter for OpenAi {
    fn completion(&self, method: &http::Method, path: &str) -> crate::plugin::adapter::Completion {
        if self.classify(method, path) == RequestClass::Work {
            crate::plugin::adapter::Completion::ResponseClose
        } else {
            crate::plugin::adapter::Completion::Unknown
        }
    }

    fn health_path(&self) -> Option<&'static str> {
        Some("/v1/models")
    }
    fn kind(&self) -> &'static str {
        "openai"
    }

    fn capabilities(&self) -> AdapterCapabilities {
        AdapterCapabilities {
            can_unload: false,
            reports_model: true,
            can_cancel: false,
        }
    }

    fn borrower_reads(&self) -> &'static [ReadRoute] {
        BORROWER_READS
    }

    fn classify_request(
        &self,
        method: &http::Method,
        path: &str,
        query: Option<&str>,
    ) -> RequestClass {
        // A health check needs no parameters, and the ones llama-server's
        // health route has taken read slot state: `include_slots` returned
        // the slots, prompts included, and `fail_on_no_slot` reports whether
        // every slot is busy.
        if is_read(method) && matches!(path, "/health" | "/v1/health") && query.is_some() {
            return RequestClass::Management;
        }
        self.classify(method, path)
    }

    fn classify(&self, method: &http::Method, path: &str) -> RequestClass {
        if is_read(method) {
            // What the server is doing and how it is configured: `/slots`
            // returns each active slot's prompt, `/metrics` counts the
            // owner's requests and tokens, `/props` exposes the model's file
            // path, chat template and server settings, and `/lora-adapters`
            // lists adapter files. These are the owner's.
            if matches!(path, "/slots" | "/metrics" | "/props" | "/lora-adapters") {
                return RequestClass::Management;
            }
            return classify_listed(method, path, BORROWER_READS);
        }
        if *method != http::Method::POST {
            return classify_unknown(method);
        }
        // One table serves generic OpenAI-compatible servers and llama.cpp's
        // llama-server, which is often configured as `openai`. Inference:
        // the OpenAI routes, llama-server's unprefixed aliases and native
        // routes (tools/server/README.md), reranking, and the Anthropic
        // Messages API that llama-server and vLLM implement. Each answers in
        // one response.
        if matches!(
            path,
            "/v1/chat/completions"
                | "/chat/completions"
                | "/v1/completions"
                | "/completions"
                | "/v1/responses"
                | "/v1/embeddings"
                | "/embeddings"
                | "/v1/messages"
                // llama.cpp's native routes
                | "/completion"
                | "/infill"
                | "/embedding"
                | "/rerank"
                | "/reranking"
                | "/v1/rerank"
                | "/v1/reranking"
        ) {
            return RequestClass::Work;
        }
        // Tokenizer and chat-template utilities. They run no model and
        // answer in milliseconds, so they neither need nor should hold the
        // resource that a running job owns.
        if matches!(
            path,
            "/tokenize" | "/detokenize" | "/apply-template" | "/v1/messages/count_tokens"
        ) {
            return RequestClass::PassThrough;
        }
        // Server state: llama-server's runtime properties, slot save,
        // restore and erase, LoRA scales, and router-mode model loading, and
        // vLLM's LoRA loading. These change what the server does for the
        // owner, so they are the owner's to allow per upstream.
        if matches!(
            path,
            "/props"
                | "/lora-adapters"
                | "/models/load"
                | "/models/unload"
                | "/v1/load_lora_adapter"
                | "/v1/unload_lora_adapter"
        ) || path
            .strip_prefix("/slots/")
            .is_some_and(|id| !id.is_empty() && id.bytes().all(|b| b.is_ascii_digit()))
        {
            return RequestClass::Management;
        }
        classify_unknown(method)
    }

    fn model_of(&self, body: &[u8]) -> Option<String> {
        serde_json::from_slice::<Value>(body)
            .ok()?
            .get("model")?
            .as_str()
            .map(String::from)
    }

    fn unsupported_request(&self, path: &str, body: &[u8]) -> Option<String> {
        crate::plugin::adapter::responses_background(path, body)
    }
}

/// Explicit llama.cpp profile. Generic OpenAI servers are never assumed to
/// implement /slots, and an unavailable slots endpoint cannot imply vacancy.
pub struct LlamaCpp;
impl Adapter for LlamaCpp {
    fn observes_models(&self) -> bool {
        true
    }
    fn kind(&self) -> &'static str {
        "llamacpp"
    }
    fn health_path(&self) -> Option<&'static str> {
        Some("/health")
    }
    fn capabilities(&self) -> AdapterCapabilities {
        OpenAi.capabilities()
    }
    fn classify(&self, method: &http::Method, path: &str) -> RequestClass {
        OpenAi.classify(method, path)
    }
    fn classify_request(
        &self,
        method: &http::Method,
        path: &str,
        query: Option<&str>,
    ) -> RequestClass {
        OpenAi.classify_request(method, path, query)
    }
    fn borrower_reads(&self) -> &'static [ReadRoute] {
        OpenAi.borrower_reads()
    }
    fn model_of(&self, body: &[u8]) -> Option<String> {
        OpenAi.model_of(body)
    }
    fn unsupported_request(&self, path: &str, body: &[u8]) -> Option<String> {
        OpenAi.unsupported_request(path, body)
    }
    fn completion(&self, method: &http::Method, path: &str) -> crate::plugin::adapter::Completion {
        OpenAi.completion(method, path)
    }
    fn requires_idle_observation(&self) -> bool {
        true
    }
    fn residency(&self) -> crate::plugin::adapter::Residency {
        crate::plugin::adapter::Residency::SingleModel
    }
    fn observe_models<'a>(
        &'a self,
        http: &'a reqwest::Client,
        base: &'a str,
    ) -> Option<futures_util::future::BoxFuture<'a, anyhow::Result<crate::lifecycle::Models>>> {
        Some(Box::pin(async move {
            let value = crate::admission::read_json(
                http.get(format!("{}/slots", base.trim_end_matches('/'))),
            )
            .await?;
            let slots = value
                .as_array()
                .ok_or_else(|| anyhow::anyhow!("missing slots"))?;
            anyhow::ensure!(
                !slots.is_empty() && slots.len() <= 128,
                "invalid slot count"
            );
            let mut busy = false;
            for slot in slots {
                busy |= slot
                    .get("is_processing")
                    .and_then(|v| v.as_bool())
                    .ok_or_else(|| anyhow::anyhow!("missing slot state"))?;
            }
            Ok(crate::lifecycle::Models {
                busy,
                models: vec![vacancyd::status::Model {
                    name: None,
                    state: if busy { "running" } else { "loaded_idle" }.into(),
                    resident_bytes: None,
                    expires_at: None,
                    context_length: None,
                    digest: None,
                    default_context_length: None,
                }],
            })
        }))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use http::Method;

    #[test]
    fn classifies_known_routes_exactly() {
        let a = OpenAi;
        for path in [
            "/v1/chat/completions",
            "/chat/completions",
            "/v1/completions",
            "/completions",
            "/v1/responses",
            "/v1/embeddings",
            "/embeddings",
            "/embedding",
            "/v1/messages",
            "/completion",
            "/infill",
            "/rerank",
            "/reranking",
            "/v1/rerank",
            "/v1/reranking",
        ] {
            assert_eq!(
                a.classify(&Method::POST, path),
                RequestClass::Work,
                "{path}"
            );
            assert_eq!(LlamaCpp.classify(&Method::POST, path), RequestClass::Work);
        }
        for path in [
            "/tokenize",
            "/detokenize",
            "/apply-template",
            "/v1/messages/count_tokens",
        ] {
            assert_eq!(
                a.classify(&Method::POST, path),
                RequestClass::PassThrough,
                "{path}"
            );
        }
        for path in [
            "/props",
            "/slots/0",
            "/slots/12",
            "/lora-adapters",
            "/models/load",
            "/models/unload",
            "/v1/load_lora_adapter",
            "/v1/unload_lora_adapter",
        ] {
            assert_eq!(
                a.classify(&Method::POST, path),
                RequestClass::Management,
                "{path}"
            );
        }
    }

    #[test]
    fn borrowers_read_only_discovery_and_health() {
        for adapter in [&OpenAi as &dyn Adapter, &LlamaCpp] {
            let listed: Vec<String> = adapter
                .borrower_reads()
                .iter()
                .map(ReadRoute::shown)
                .collect();
            assert_eq!(listed, ["/v1/models", "/models", "/health", "/v1/health"]);
            for path in ["/v1/models", "/models", "/health", "/v1/health"] {
                for method in [Method::GET, Method::HEAD] {
                    assert_eq!(
                        adapter.classify_request(&method, path, None),
                        RequestClass::PassThrough,
                        "{} {method} {path}",
                        adapter.kind()
                    );
                }
            }
            assert_eq!(
                adapter.classify(&Method::GET, adapter.health_path().unwrap()),
                RequestClass::PassThrough
            );
        }
    }

    #[test]
    fn the_owners_server_state_is_management_on_every_read() {
        for adapter in [&OpenAi as &dyn Adapter, &LlamaCpp] {
            for path in ["/slots", "/metrics", "/props", "/lora-adapters"] {
                for method in [Method::GET, Method::HEAD] {
                    assert_eq!(
                        adapter.classify_request(&method, path, Some("fail_on_no_slot=1")),
                        RequestClass::Management,
                        "{method} {path}"
                    );
                    assert_eq!(
                        adapter.classify(&method, path),
                        RequestClass::Management,
                        "{method} {path}"
                    );
                }
            }
            // Health with slot parameters reads slot state.
            for query in ["include_slots", "fail_on_no_slot=1", ""] {
                assert_eq!(
                    adapter.classify_request(&Method::GET, "/health", Some(query)),
                    RequestClass::Management,
                    "{query}"
                );
            }
        }
    }

    #[test]
    fn unlisted_reads_and_other_methods_fail_closed() {
        for adapter in [&OpenAi as &dyn Adapter, &LlamaCpp] {
            for path in [
                "/",
                "/index.html",
                "/slots/0",
                "/%73lots",
                "/slots/",
                "/v1/models/",
                "/v1/models/x",
                "/version",
                "/tokenize",
                "/v1/chat/completions",
                "/api/tags",
                "/health/",
                "/Health",
            ] {
                for method in [Method::GET, Method::HEAD] {
                    assert_eq!(
                        adapter.classify_request(&method, path, None),
                        RequestClass::UnknownRead,
                        "{method} {path}"
                    );
                }
            }
            for method in [Method::OPTIONS, Method::TRACE, Method::CONNECT] {
                for path in ["/slots", "/v1/models", "/health", "/v1/chat/completions"] {
                    assert_eq!(
                        adapter.classify_request(&method, path, None),
                        RequestClass::UnknownRoute,
                        "{method} {path}"
                    );
                }
            }
        }
    }

    #[test]
    fn unknown_mutations_and_route_lookalikes_are_gated() {
        let a = OpenAi;
        for method in [Method::POST, Method::PUT, Method::PATCH, Method::DELETE] {
            assert_eq!(
                a.classify(&method, "/v1/future-inference"),
                RequestClass::UnknownRoute
            );
        }
        for path in [
            "/v1/chat/completions/bypass",
            "/tokenize/extra",
            "/slots/",
            "/slots/0/extra",
            "/slots/..",
        ] {
            assert_eq!(
                a.classify(&Method::POST, path),
                RequestClass::UnknownRoute,
                "{path}"
            );
        }
        assert_eq!(
            a.classify(&Method::PUT, "/v1/chat/completions"),
            RequestClass::UnknownRoute
        );
    }
}
