//! ComfyUI adapter.
//!
//! ComfyUI's shape differs from an LLM server in two ways that matter here.
//!
//! **Work is submitted, not requested.** `POST /prompt` enqueues a workflow
//! and returns a `prompt_id` immediately; the image is produced later and
//! collected over the websocket or from `/history`. So gating `/prompt` is
//! gating the *admission of work*, which is exactly right — refuse at the
//! door and the queue never grows.
//!
//! **There is no "model" field.** A workflow graph names checkpoints inside
//! its nodes, and a single graph may load several. Per-model VRAM thresholds
//! therefore do not apply; ComfyUI upstreams gate on the resource's default
//! headroom instead. `reports_model` is false to say so explicitly.
//!
//! Unloading is supported: `POST /free` with `{"unload_models": true,
//! "free_memory": true}` releases the loaded checkpoints — the ComfyUI
//! equivalent of Ollama's `keep_alive: 0`.

use std::time::Duration;

use anyhow::Result;
use futures_util::future::BoxFuture;
use tracing::info;

use crate::plugin::adapter::{
    classify_listed, is_safe_segment, percent_decoded, Adapter, AdapterCapabilities, CancelOutcome,
    ReadRoute, RequestClass,
};

pub struct ComfyUi;

/// What a client that submits prompts reads: the queue length (`GET
/// /prompt`), progress over `/ws`, its own prompt's history and job entry by
/// ID, its images through `/view`, and the node and model catalogue and
/// device information it builds workflows from. Compared after one `/api`
/// prefix is removed, as ComfyUI mirrors every route under `/api`.
const BORROWER_READS: &[ReadRoute] = &[
    ReadRoute::Exact("/prompt"),
    ReadRoute::Exact("/ws"),
    ReadRoute::Child("/history/"),
    ReadRoute::Child("/jobs/"),
    ReadRoute::Child("/api/jobs/"),
    ReadRoute::Exact("/view"),
    ReadRoute::Exact("/object_info"),
    ReadRoute::Child("/object_info/"),
    ReadRoute::Exact("/models"),
    ReadRoute::Child("/models/"),
    ReadRoute::Exact("/embeddings"),
    ReadRoute::Exact("/system_stats"),
];

impl Adapter for ComfyUi {
    fn observes_models(&self) -> bool {
        true
    }
    fn requires_idle_observation(&self) -> bool {
        true
    }
    fn residency(&self) -> crate::plugin::adapter::Residency {
        crate::plugin::adapter::Residency::AllocatorPool
    }
    fn observe_models<'a>(
        &'a self,
        http: &'a reqwest::Client,
        base: &'a str,
    ) -> Option<BoxFuture<'a, Result<crate::lifecycle::Models>>> {
        Some(Box::pin(async move {
            let base = base.trim_end_matches('/');
            // `GET /prompt` counts the running and pending prompts in a few
            // bytes. `/queue` would make ComfyUI serialize every queued
            // workflow on its only event loop, every observation, however
            // little of the reply vacancyd reads.
            let info = crate::admission::read_json(http.get(format!("{base}/prompt"))).await?;
            let queued = info
                .pointer("/exec_info/queue_remaining")
                .and_then(|v| v.as_u64())
                .ok_or_else(|| anyhow::anyhow!("missing exec_info.queue_remaining"))?;
            // system_stats reports device memory, not which checkpoint is loaded.
            // Do not invent a per-model loaded-idle claim from that number.
            let stats =
                crate::admission::read_json(http.get(format!("{base}/system_stats"))).await?;
            let devices = stats
                .get("devices")
                .and_then(|v| v.as_array())
                .ok_or_else(|| anyhow::anyhow!("missing devices"))?;
            // Memory ComfyUI's own allocator holds, which it reuses and evicts
            // for the next job. Reported only when there is one device, so it
            // cannot be attributed to the wrong GPU.
            let allocator_bytes = match devices.as_slice() {
                [device] => device.get("torch_vram_total").and_then(|v| v.as_u64()),
                _ => None,
            };
            Ok(crate::lifecycle::Models {
                models: vec![vacancyd::status::Model {
                    name: None,
                    // Queued and running work are one count here, so both show
                    // as running.
                    state: if queued > 0 { "running" } else { "unknown" }.into(),
                    resident_bytes: allocator_bytes,
                    expires_at: None,
                    context_length: None,
                    digest: None,
                    default_context_length: None,
                }],
                busy: queued > 0,
            })
        }))
    }

    fn completion(&self, method: &http::Method, path: &str) -> crate::plugin::adapter::Completion {
        if self.classify(method, path) == RequestClass::Work {
            crate::plugin::adapter::Completion::Tracked
        } else {
            crate::plugin::adapter::Completion::Unknown
        }
    }

    fn submission_rejected(&self, status: http::StatusCode, body: &[u8]) -> bool {
        if status != http::StatusCode::BAD_REQUEST {
            return false;
        }
        let Ok(value) = serde_json::from_slice::<serde_json::Value>(body) else {
            return false;
        };
        // ComfyUI's /prompt handler returns these validation errors before
        // prompt_queue.put. Other errors, including 5xx, remain uncertain.
        value.get("prompt_id").is_none()
            && value.get("node_errors").is_some_and(|v| v.is_object())
            && value
                .pointer("/error/message")
                .is_some_and(|v| v.is_string())
            && value
                .pointer("/error/details")
                .is_some_and(|v| v.is_string())
            && value
                .pointer("/error/extra_info")
                .is_some_and(|v| v.is_object())
            && matches!(
                value.pointer("/error/type").and_then(|v| v.as_str()),
                Some(
                    "no_prompt"
                        | "invalid_prompt_id"
                        | "missing_node_type"
                        | "prompt_no_outputs"
                        | "prompt_outputs_failed_validation"
                )
            )
    }

    fn tracking_id(&self, body: &[u8]) -> Option<String> {
        let value: serde_json::Value = serde_json::from_slice(body).ok()?;
        let id = value.get("prompt_id")?.as_str()?;
        // Never allow a server-supplied id to become a URL or path traversal.
        crate::admission::is_safe_tracking_id(id).then(|| id.to_owned())
    }

    fn with_tracking_id(&self, body: &[u8], id: &str) -> Option<Result<Vec<u8>, String>> {
        Some(prompt_with_id(body, id))
    }

    fn completed<'a>(
        &'a self,
        http: &'a reqwest::Client,
        base: &'a str,
        id: &'a str,
    ) -> BoxFuture<'a, Result<bool>> {
        Box::pin(async move {
            // History membership is terminal, including execution errors. An
            // empty history may mean eviction or cancellation, never completion.
            let status = history_status(http, base.trim_end_matches('/'), id).await?;
            Ok(status.is_some_and(|s| matches!(s.as_str(), "success" | "error")))
        })
    }

    fn queue_absent<'a>(
        &'a self,
        http: &'a reqwest::Client,
        base: &'a str,
        id: &'a str,
    ) -> BoxFuture<'a, Result<bool>> {
        Box::pin(async move {
            let ours = [id.to_owned()];
            let places = locate(http, base.trim_end_matches('/'), &ours).await?;
            Ok(!matches!(places[0], Place::Pending | Place::Running))
        })
    }

    fn keeps_assigned_ids<'a>(
        &'a self,
        http: &'a reqwest::Client,
        base: &'a str,
    ) -> BoxFuture<'a, Result<bool>> {
        Box::pin(async move {
            let reported = reported_version(http, base.trim_end_matches('/')).await?;
            // An unknown version is treated as an old one.
            Ok(reported
                .as_deref()
                .and_then(parse_version)
                .is_some_and(|version| version >= ASSIGNED_ID_SINCE))
        })
    }

    fn cancel<'a>(
        &'a self,
        http: &'a reqwest::Client,
        base: &'a str,
        ids: &'a [String],
        unacknowledged: &'a [String],
    ) -> BoxFuture<'a, Result<CancelOutcome>> {
        Box::pin(async move {
            let base = base.trim_end_matches('/');
            // An id vacancyd assigned whose submission reply never arrived is
            // shown to exist only by ComfyUI itself, so look before deleting.
            // A failed look only loses that proof; deletion goes ahead.
            let before = if unacknowledged.is_empty() {
                Vec::new()
            } else {
                locate(http, base, unacknowledged).await.unwrap_or_default()
            };
            // Deletion by id touches only pending prompts with these ids. The
            // running prompt and the owner's own queued work are unaffected.
            let resp = http
                .post(format!("{base}/queue"))
                .json(&serde_json::json!({ "delete": ids }))
                .timeout(STATE_REQUEST_TIMEOUT)
                .send()
                .await?;
            anyhow::ensure!(
                resp.status().is_success(),
                "ComfyUI queue deletion returned HTTP {}",
                resp.status()
            );
            // Look again after deleting, so a prompt that started running in
            // between is seen as running rather than assumed removed.
            let after = locate(http, base, ids).await?;
            let mut outcome = CancelOutcome::default();
            let mut interrupt = None;
            for (id, place) in ids.iter().zip(&after) {
                // Queued, running or finished: each shows ComfyUI accepted it.
                let seen_before = unacknowledged
                    .iter()
                    .position(|u| u == id)
                    .and_then(|i| before.get(i))
                    .is_some_and(|p| *p != Place::Unknown);
                if unacknowledged.contains(id) && (seen_before || *place != Place::Unknown) {
                    outcome.found.push(id.clone());
                }
                if *place == Place::Pending {
                    outcome
                        .failed
                        .push((id.clone(), "still queued after deletion".into()));
                } else if *place == Place::Running {
                    // Only a prompt of ours that the queue shows running is
                    // stopped, and only by a request that names it.
                    match stop_running(http, base, id, &mut interrupt).await {
                        Ok(Stop::Requested) => outcome.interrupted.push(id.clone()),
                        Ok(Stop::Unsafe(reason)) => outcome.left_running.push((id.clone(), reason)),
                        Err(error) => outcome
                            .failed
                            .push((id.clone(), format!("interrupt failed: {error:#}"))),
                    }
                } else {
                    outcome.stopped.push(id.clone());
                }
            }
            Ok(outcome)
        })
    }

    fn holds_foreign_work<'a>(
        &'a self,
        http: &'a reqwest::Client,
        base: &'a str,
        ours: &'a [String],
    ) -> BoxFuture<'a, Result<Option<bool>>> {
        Box::pin(async move {
            // `/free` takes effect once the running prompt ends and unloads
            // every model, so any prompt the owner queued here would start
            // over from cold. The read stops at the first such prompt.
            let scan = scan_queue_within(
                http,
                base.trim_end_matches('/'),
                ours,
                Until::Foreign,
                FOREIGN_CHECK,
            )
            .await?;
            let ours = scan.ours_running.len() + scan.ours_pending.len();
            Ok(Some(scan.running + scan.pending > ours))
        })
    }

    fn health_path(&self) -> Option<&'static str> {
        Some("/system_stats")
    }
    fn kind(&self) -> &'static str {
        "comfyui"
    }

    fn capabilities(&self) -> AdapterCapabilities {
        AdapterCapabilities {
            can_unload: true,
            // Checkpoints live inside the workflow graph, not a `model` field.
            reports_model: false,
            // `/queue` deletes pending prompts and `/interrupt` stops a running one.
            can_cancel: true,
        }
    }

    fn borrower_reads(&self) -> &'static [ReadRoute] {
        BORROWER_READS
    }

    fn classify(&self, method: &http::Method, path: &str) -> RequestClass {
        let original = path;
        let path = strip_api_prefix(path);
        if *method == http::Method::POST && path == "/prompt" {
            return RequestClass::Work;
        }

        // Operations that change the instance rather than submit work:
        // freeing or interrupting whatever runs, clearing or deleting queue
        // and history entries, uploading files, and writing users, settings
        // and user data (saved workflows). The web UI uses them; a borrower
        // that only submits prompts does not.
        let mutation = matches!(
            (method, path),
            (&http::Method::POST, "/free")
                | (&http::Method::POST, "/interrupt")
                | (&http::Method::POST, "/queue")
                | (&http::Method::POST, "/history")
                | (&http::Method::POST, "/users")
                | (&http::Method::POST, "/settings")
                | (&http::Method::POST, "/jobs/cancel")
                | (&http::Method::POST, "/api/jobs/cancel")
        ) || (*method == http::Method::POST
            && (is_single_child(path, "/settings/")
                || is_single_child(path, "/upload/")
                || is_job_cancel(path)))
            || (matches!(*method, http::Method::POST | http::Method::DELETE)
                && is_single_child(path, "/userdata/"))
            || (*method == http::Method::POST && is_userdata_move(path));
        if mutation || is_owner_data(method, original) {
            return RequestClass::Management;
        }
        // A listed read must also be listed in the form ComfyUI's router
        // resolves, so no encoding reaches another handler through it.
        let class = classify_listed(method, path, BORROWER_READS);
        if class == RequestClass::PassThrough
            && classify_listed(
                method,
                strip_api_prefix(&router_path(original)),
                BORROWER_READS,
            ) != RequestClass::PassThrough
        {
            return RequestClass::UnknownRead;
        }
        class
    }

    fn unload_all<'a>(
        &'a self,
        http: &'a reqwest::Client,
        base: &'a str,
    ) -> BoxFuture<'a, Result<usize>> {
        Box::pin(async move {
            let base = base.trim_end_matches('/');
            let body = serde_json::json!({ "unload_models": true, "free_memory": true });
            let resp = http.post(format!("{base}/free")).json(&body).send().await?;
            anyhow::ensure!(
                resp.status().is_success(),
                "ComfyUI unload returned HTTP {}",
                resp.status()
            );
            // /free accepts a flag for the worker; it does not confirm that a
            // model was resident or that device memory has already been freed.
            info!("ComfyUI accepted the model-unload request");
            Ok(1)
        })
    }
}

/// The body to forward for a `/prompt` submission, with `id` as its
/// `prompt_id`.
///
/// The id is appended as the object's last member rather than by
/// re-serializing the body, so the borrower's workflow reaches ComfyUI byte
/// for byte (numbers too large for a 64-bit integer keep their exact text).
/// Python's JSON decoder keeps the last of repeated names, so a `prompt_id`
/// the borrower set is replaced. ComfyUI 0.3.45 and later use a submitted
/// `prompt_id` (comfyanonymous/ComfyUI#8189; 0.25.0 and later require a
/// canonical lowercase UUID); earlier releases ignore it and generate their
/// own, which the reply then reports.
fn prompt_with_id(body: &[u8], id: &str) -> Result<Vec<u8>, String> {
    const NOT_AN_OBJECT: &str = "a ComfyUI prompt submission must be a JSON object";
    let is_space = |b: &u8| matches!(b, b' ' | b'\t' | b'\n' | b'\r');
    serde_json::from_slice::<serde::de::IgnoredAny>(body)
        .map_err(|error| format!("{NOT_AN_OBJECT}: {error}"))?;
    let (Some(first), Some(last)) = (
        body.iter().position(|b| !is_space(b)),
        body.iter().rposition(|b| !is_space(b)),
    ) else {
        return Err(NOT_AN_OBJECT.into());
    };
    // Valid JSON that opens with `{` is exactly one object, closed by `last`.
    if body[first] != b'{' {
        return Err(NOT_AN_OBJECT.into());
    }
    let empty = body[first + 1..last].iter().all(is_space);
    let mut forwarded = Vec::with_capacity(last + id.len() + 16);
    forwarded.extend_from_slice(&body[..last]);
    if !empty {
        forwarded.push(b',');
    }
    forwarded.extend_from_slice(b"\"prompt_id\":\"");
    forwarded.extend_from_slice(id.as_bytes());
    forwarded.extend_from_slice(b"\"}");
    Ok(forwarded)
}

/// Bounds on one streamed read. Its time bounds follow the reply's progress
/// rather than its size. A fixed limit on the whole read would fail a large
/// but healthy reply on a busy host: ComfyUI 0.37.2 took 2.4 to 3.1 seconds
/// to start sending a 102 MiB queue on the development VM, and parsing adds
/// about a second per 170 MiB in a release build, more on a loaded host.
#[derive(Debug, Clone, Copy)]
struct ReadBounds {
    /// The largest reply read. A queue listing or history entry carries
    /// whole workflow graphs, and the queue carries every one the owner has
    /// queued, so a long queue runs to tens of megabytes. Replies are parsed
    /// as they arrive and only prompt ids and statuses are kept, so memory
    /// stays bounded by the read buffer, not by this limit.
    max_bytes: u64,
    /// Until the reply's headers. ComfyUI builds the whole JSON body before
    /// it sends them, so this covers its serialization of the listing.
    headers: Duration,
    /// Longest silence between two chunks of the body.
    stall: Duration,
    /// The whole read, parsing included: the backstop for a reply that
    /// trickles without ever stalling.
    total: Duration,
}

/// The same in every build, tests included, so that a test against a real
/// ComfyUI exercises the limits a user gets.
const LISTING: ReadBounds = ReadBounds {
    max_bytes: 256 * 1024 * 1024,
    headers: Duration::from_secs(60),
    stall: Duration::from_secs(10),
    total: Duration::from_secs(300),
};

/// Bounds on the queue read that decides whether ComfyUI holds a prompt
/// vacancyd did not admit. It stops at the end of the first such prompt, so
/// it reads only that one and those ahead of it: the running one and
/// whatever of vacancyd's own outlived cancellation, whose pending prompts
/// were just deleted. 16 MiB holds several of the largest workflows seen in practice;
/// a queue that needs more ahead of its first foreign prompt fails the
/// check, and the unload goes ahead as it would without one. ComfyUI
/// serializes the whole queue before it sends headers, so their bound stays
/// as for [`LISTING`]; the total allows that plus the short read.
const FOREIGN_CHECK: ReadBounds = ReadBounds {
    max_bytes: 16 * 1024 * 1024,
    headers: Duration::from_secs(60),
    stall: Duration::from_secs(10),
    total: Duration::from_secs(90),
};

/// Bound on one request that changes or inspects ComfyUI's queue during
/// cancellation. Its reply is small, but ComfyUI's work is not: 0.37.2
/// deep-copies the pending queue under its queue lock to answer the job
/// cancel and the prompt-scoped interrupt, which took 38 to 42 seconds with
/// about 100 MiB of workflows pending on the development VM. ComfyUI
/// finishes such a request even after the client gives up.
pub(crate) const STATE_REQUEST_TIMEOUT: Duration = Duration::from_secs(120);

/// Upstream reply bytes handed from the async reader to a blocking parser.
struct Chunks {
    receive: tokio::sync::mpsc::Receiver<axum::body::Bytes>,
    current: axum::body::Bytes,
    offset: usize,
}

impl std::io::Read for Chunks {
    fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
        while self.offset == self.current.len() {
            match self.receive.blocking_recv() {
                Some(chunk) => {
                    self.current = chunk;
                    self.offset = 0;
                }
                // The sender stops at the end of the reply, at the size
                // limit, or when the caller gave up: all read as the end.
                None => return Ok(0),
            }
        }
        let n = buf.len().min(self.current.len() - self.offset);
        buf[..n].copy_from_slice(&self.current[self.offset..self.offset + n]);
        self.offset += n;
        Ok(n)
    }
}

type Listing = std::io::BufReader<Chunks>;

/// Parse a JSON reply while it streams in, without holding all of it.
/// `parse` runs on a blocking thread and may return before the end of the
/// reply; the rest is then not read.
async fn stream_json<T: Send + 'static>(
    request: reqwest::RequestBuilder,
    parse: impl FnOnce(Listing) -> Result<T> + Send + 'static,
) -> Result<T> {
    stream_json_within(request, parse, LISTING).await
}

async fn stream_json_within<T: Send + 'static>(
    request: reqwest::RequestBuilder,
    parse: impl FnOnce(Listing) -> Result<T> + Send + 'static,
    bounds: ReadBounds,
) -> Result<T> {
    tokio::time::timeout(bounds.total, async move {
        let mut response = tokio::time::timeout(bounds.headers, request.send())
            .await
            .map_err(|_| anyhow::anyhow!("ComfyUI sent no reply within {:?}", bounds.headers))??
            .error_for_status()?;
        let (send, receive) = tokio::sync::mpsc::channel(4);
        let parser = tokio::task::spawn_blocking(move || {
            parse(std::io::BufReader::with_capacity(
                64 * 1024,
                Chunks {
                    receive,
                    current: axum::body::Bytes::new(),
                    offset: 0,
                },
            ))
        });
        let fed = async {
            let mut received = 0u64;
            // Only the upstream's silence counts, not time spent waiting for
            // the parser to take the previous chunk.
            while let Some(chunk) = tokio::time::timeout(bounds.stall, response.chunk())
                .await
                .map_err(|_| anyhow::anyhow!("ComfyUI reply stalled for {:?}", bounds.stall))??
            {
                received = received.saturating_add(chunk.len() as u64);
                anyhow::ensure!(
                    received <= bounds.max_bytes,
                    "ComfyUI reply exceeds {} MiB",
                    bounds.max_bytes / (1024 * 1024)
                );
                if send.send(chunk).await.is_err() {
                    // The parser already has its answer.
                    break;
                }
            }
            anyhow::Ok(())
        }
        .await;
        drop(send);
        let parsed = parser
            .await
            .map_err(|error| anyhow::anyhow!("ComfyUI reply parser failed: {error}"))?;
        // A read failure explains a parse error better than the parse error.
        fed?;
        parsed
    })
    .await
    .map_err(|_| anyhow::anyhow!("ComfyUI reply took longer than {:?}", bounds.total))?
}

/// When a `/queue` read may stop before the end of the reply.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Until {
    /// Read everything: counts and every one of our ids.
    End,
    /// Stop at the first of our ids, in either list.
    Found,
    /// Stop at the first id that is not one of ours, in either list.
    Foreign,
}

/// What one read of `/queue` established.
#[derive(Debug, Default)]
struct QueueScan {
    /// Items in each list; lower bounds when the read stopped early.
    running: usize,
    pending: usize,
    /// The requested ids found in each list.
    ours_running: Vec<String>,
    ours_pending: Vec<String>,
}

/// Read `/queue`, keeping only counts and which of `ours` each list holds.
/// Each item is an array whose second element is the prompt id; a malformed
/// item is an error, because a cancellation decision must not guess which
/// prompt is running. `until` can stop reading once the answer is known.
async fn scan_queue(
    http: &reqwest::Client,
    base: &str,
    ours: &[String],
    until: Until,
) -> Result<QueueScan> {
    scan_queue_within(http, base, ours, until, LISTING).await
}

/// [`scan_queue`] within `bounds`.
async fn scan_queue_within(
    http: &reqwest::Client,
    base: &str,
    ours: &[String],
    until: Until,
    bounds: ReadBounds,
) -> Result<QueueScan> {
    let ours = ours.to_vec();
    stream_json_within(
        http.get(format!("{base}/queue")),
        move |listing| {
            let mut scan = QueueScan::default();
            let mut settled = false;
            let mut de = serde_json::Deserializer::from_reader(listing);
            let result = serde::de::Deserializer::deserialize_map(
                &mut de,
                queue_scan::Queue {
                    scan: &mut scan,
                    ours: &ours,
                    until,
                    settled: &mut settled,
                },
            )
            .and_then(|()| de.end());
            match result {
                Ok(()) => Ok(scan),
                Err(_) if settled => Ok(scan),
                Err(error) => Err(anyhow::anyhow!("ComfyUI queue: {error}")),
            }
        },
        bounds,
    )
    .await
}

/// `status.status_str` of `id`'s entry in its `/history/<id>` reply, if it has one.
async fn history_status(http: &reqwest::Client, base: &str, id: &str) -> Result<Option<String>> {
    let wanted = id.to_owned();
    stream_json(http.get(format!("{base}/history/{id}")), move |listing| {
        let mut status = None;
        let mut de = serde_json::Deserializer::from_reader(listing);
        serde::de::Deserializer::deserialize_map(
            &mut de,
            history_scan::History {
                id: &wanted,
                status: &mut status,
            },
        )
        .and_then(|()| de.end())
        .map_err(|error| anyhow::anyhow!("ComfyUI history: {error}"))?;
        Ok(status)
    })
    .await
}

/// Where ComfyUI holds one prompt.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Place {
    Pending,
    Running,
    /// In the history: it ran, failed or was stopped.
    Finished,
    /// Neither queued nor running, and not known to have finished: deleted,
    /// evicted from history, never queued, or on a release that cannot say.
    Unknown,
}

/// Where ComfyUI holds each of `ids`, in order.
///
/// ComfyUI 0.6.0 and later (comfyanonymous/ComfyUI#11054) answer
/// `GET /api/jobs/<id>` from one snapshot of the queue, taken under the lock
/// that moves prompts from pending to running and from running to history,
/// then the history; the reply is a few hundred bytes whatever the queue
/// holds. It answers an unknown id with a JSON 404. Earlier releases have no
/// such route, which aiohttp answers with a plain-text 404; only then is
/// `/queue` read, the whole of it unless there is a single id to stop at.
async fn locate(http: &reqwest::Client, base: &str, ids: &[String]) -> Result<Vec<Place>> {
    let mut places = Vec::with_capacity(ids.len());
    for id in ids {
        match job_place(http, base, id).await? {
            Some(place) => places.push(place),
            None => return locate_in_queue(http, base, ids).await,
        }
    }
    Ok(places)
}

/// `Some` place from the jobs route, or `None` when the route is missing.
async fn job_place(http: &reqwest::Client, base: &str, id: &str) -> Result<Option<Place>> {
    let resp = http
        .get(format!("{base}/api/jobs/{id}"))
        .timeout(STATE_REQUEST_TIMEOUT)
        .send()
        .await?;
    let status = resp.status();
    let json_reply = resp
        .headers()
        .get(http::header::CONTENT_TYPE)
        .and_then(|v| v.to_str().ok())
        .is_some_and(|v| v.starts_with("application/json"));
    if status == http::StatusCode::NOT_FOUND {
        if !json_reply {
            return Ok(None);
        }
        let reply = read_small(resp).await?;
        anyhow::ensure!(
            reply.get("error").is_some_and(|v| v.is_string()),
            "ComfyUI job lookup answered 404 with an unrecognized reply"
        );
        return Ok(Some(Place::Unknown));
    }
    anyhow::ensure!(
        status.is_success() && json_reply,
        "ComfyUI job lookup returned HTTP {status}"
    );
    let reply = read_small(resp).await?;
    Ok(Some(match reply.get("status").and_then(|v| v.as_str()) {
        Some("pending") => Place::Pending,
        Some("in_progress") => Place::Running,
        Some("completed" | "failed" | "cancelled") => Place::Finished,
        other => anyhow::bail!("ComfyUI job lookup reported an unrecognized status {other:?}"),
    }))
}

/// [`locate`] for releases before the jobs route, from one `/queue` read.
/// The queue cannot tell a finished prompt from a deleted one.
async fn locate_in_queue(http: &reqwest::Client, base: &str, ids: &[String]) -> Result<Vec<Place>> {
    let until = if ids.len() == 1 {
        Until::Found
    } else {
        Until::End
    };
    let scan = scan_queue(http, base, ids, until).await?;
    Ok(ids
        .iter()
        .map(|id| {
            if scan.ours_running.contains(id) {
                Place::Running
            } else if scan.ours_pending.contains(id) {
                Place::Pending
            } else {
                Place::Unknown
            }
        })
        .collect())
}

/// What a request to stop one running prompt achieved.
#[derive(Debug, PartialEq, Eq)]
enum Stop {
    /// ComfyUI was asked to stop that prompt, and only that prompt.
    Requested,
    /// Not attempted, because this ComfyUI could stop a different prompt.
    Unsafe(String),
}

/// The first ComfyUI release whose `/interrupt` honors a `prompt_id`,
/// skipping the interrupt when that prompt is not the one running
/// (comfyanonymous/ComfyUI#9607, merged 2025-09-02). Earlier releases ignore
/// the id and interrupt whatever is running.
const SCOPED_INTERRUPT_SINCE: [u64; 3] = [0, 3, 57];

/// The first ComfyUI release that queues a prompt under the `prompt_id` it
/// was submitted with (comfyanonymous/ComfyUI#8189). Earlier releases ignore
/// it and choose their own.
const ASSIGNED_ID_SINCE: [u64; 3] = [0, 3, 45];

/// How `/interrupt` may be used on this ComfyUI, decided once per
/// cancellation from its reported version.
#[derive(Debug, Clone, PartialEq, Eq)]
enum Interrupt {
    Scoped,
    Unsafe(String),
}

/// Stop the running prompt `id`, choosing the narrowest request this ComfyUI
/// supports.
///
/// ComfyUI 0.26.0 and later (comfyanonymous/ComfyUI#14493) cancel one job by
/// id, checking that it runs and interrupting it under the queue lock, so the
/// interrupt cannot reach a prompt that started after the check. From 0.3.57,
/// `/interrupt` with a `prompt_id` checks the running prompt first but
/// interrupts outside that lock. Before 0.3.57 it would interrupt whatever
/// runs, which in the moment after this prompt ends is the owner's next one,
/// so it is never sent: the prompt keeps running and keeps its reservation
/// until its completion is observed.
async fn stop_running(
    http: &reqwest::Client,
    base: &str,
    id: &str,
    interrupt: &mut Option<Interrupt>,
) -> Result<Stop> {
    let resp = http
        .post(format!("{base}/api/jobs/{id}/cancel"))
        .timeout(STATE_REQUEST_TIMEOUT)
        .send()
        .await?;
    let status = resp.status();
    if status.is_success() {
        // `cancelled` is false when the prompt ended in the meantime; either
        // way, only its history can confirm that it has stopped.
        let reply = read_small(resp).await?;
        anyhow::ensure!(
            reply.get("cancelled").is_some_and(|v| v.is_boolean()),
            "ComfyUI job cancellation reply was not recognized"
        );
        return Ok(Stop::Requested);
    }
    // A missing route is the only answer that shows the endpoint is absent.
    // Any other failure may have come from the endpoint itself.
    anyhow::ensure!(
        matches!(
            status,
            http::StatusCode::NOT_FOUND | http::StatusCode::METHOD_NOT_ALLOWED
        ),
        "ComfyUI job cancellation returned HTTP {status}"
    );
    let interrupt = match interrupt {
        Some(known) => known.clone(),
        None => interrupt
            .insert(interrupt_support(http, base).await)
            .clone(),
    };
    match interrupt {
        Interrupt::Unsafe(reason) => Ok(Stop::Unsafe(reason)),
        Interrupt::Scoped => {
            let resp = http
                .post(format!("{base}/interrupt"))
                .json(&serde_json::json!({ "prompt_id": id }))
                .timeout(STATE_REQUEST_TIMEOUT)
                .send()
                .await?;
            anyhow::ensure!(
                resp.status().is_success(),
                "interrupt returned HTTP {}",
                resp.status()
            );
            Ok(Stop::Requested)
        }
    }
}

/// Whether `/interrupt` on this ComfyUI honors a `prompt_id`, from the
/// version `/system_stats` reports. An unknown version is treated as an old
/// one.
async fn interrupt_support(http: &reqwest::Client, base: &str) -> Interrupt {
    let reported = reported_version(http, base).await.ok().flatten();
    let parsed = reported.as_deref().and_then(parse_version);
    let [major, minor, patch] = SCOPED_INTERRUPT_SINCE;
    match parsed {
        Some(version) if version >= SCOPED_INTERRUPT_SINCE => Interrupt::Scoped,
        _ => Interrupt::Unsafe(format!(
            "ComfyUI {} cannot interrupt one prompt without risking the next, which may be the \
             owner's; {major}.{minor}.{patch} or later is needed, and 0.26.0 or later closes the race entirely",
            match &reported {
                Some(version) => format!("reports version {version:?}, which"),
                None => "does not report its version, so it".into(),
            }
        )),
    }
}

/// The version text `/system_stats` reports, bounded in length; `None` when
/// it reports none.
async fn reported_version(http: &reqwest::Client, base: &str) -> Result<Option<String>> {
    let stats = crate::admission::read_json(http.get(format!("{base}/system_stats"))).await?;
    Ok(stats
        .pointer("/system/comfyui_version")
        .and_then(|v| v.as_str())
        .map(|v| v.chars().take(64).collect::<String>()))
}

/// `major.minor.patch` from a ComfyUI version: `0.3.57` from its package
/// version, or from a `git describe` form such as `v0.3.57-5-gabc1234`.
/// A missing patch reads as 0. Anything else is unknown.
fn parse_version(text: &str) -> Option<[u64; 3]> {
    let text = text.trim();
    let text = text.strip_prefix(['v', 'V']).unwrap_or(text);
    let mut parts = text.split('.');
    let leading = |part: Option<&str>| -> Option<u64> {
        let part = part?;
        let digits = part.len() - part.trim_start_matches(|c: char| c.is_ascii_digit()).len();
        part[..digits].parse().ok()
    };
    let major = parts.next()?;
    if major.is_empty() || !major.bytes().all(|b| b.is_ascii_digit()) {
        return None;
    }
    let major = major.parse().ok()?;
    let minor = leading(parts.next())?;
    let patch = leading(parts.next()).unwrap_or(0);
    Some([major, minor, patch])
}

/// A small JSON reply from a request whose status was already checked.
async fn read_small(mut resp: reqwest::Response) -> Result<serde_json::Value> {
    let mut bytes = Vec::new();
    while let Some(chunk) = resp.chunk().await? {
        anyhow::ensure!(
            bytes.len() + chunk.len() <= 64 * 1024,
            "ComfyUI reply exceeds 64 KiB"
        );
        bytes.extend_from_slice(&chunk);
    }
    Ok(serde_json::from_slice(&bytes)?)
}

/// Streaming visitors for `/queue`. Only prompt ids are materialized; the
/// workflow graphs are skipped as they are read.
mod queue_scan {
    use serde::de::{DeserializeSeed, Error, IgnoredAny, MapAccess, SeqAccess, Visitor};
    use std::fmt;

    use super::{QueueScan, Until};

    pub(super) struct Queue<'a> {
        pub scan: &'a mut QueueScan,
        pub ours: &'a [String],
        pub until: Until,
        /// Set when reading stopped because the answer was known.
        pub settled: &'a mut bool,
    }

    impl<'de> Visitor<'de> for Queue<'_> {
        type Value = ();
        fn expecting(&self, f: &mut fmt::Formatter) -> fmt::Result {
            f.write_str("a ComfyUI queue object")
        }
        fn visit_map<A: MapAccess<'de>>(self, mut map: A) -> Result<(), A::Error> {
            let (mut running, mut pending) = (false, false);
            while let Some(key) = map.next_key::<String>()? {
                let is_running = match key.as_str() {
                    "queue_running" => true,
                    "queue_pending" => false,
                    _ => {
                        map.next_value::<IgnoredAny>()?;
                        continue;
                    }
                };
                map.next_value_seed(List {
                    scan: &mut *self.scan,
                    ours: self.ours,
                    is_running,
                    stop_on_ours: self.until == Until::Found,
                    stop_on_foreign: self.until == Until::Foreign,
                    settled: &mut *self.settled,
                })?;
                if is_running {
                    running = true;
                } else {
                    pending = true;
                }
            }
            if !running {
                return Err(A::Error::custom("missing queue_running"));
            }
            if !pending {
                return Err(A::Error::custom("missing queue_pending"));
            }
            Ok(())
        }
    }

    struct List<'a> {
        scan: &'a mut QueueScan,
        ours: &'a [String],
        is_running: bool,
        stop_on_ours: bool,
        stop_on_foreign: bool,
        settled: &'a mut bool,
    }

    impl<'de> DeserializeSeed<'de> for List<'_> {
        type Value = ();
        fn deserialize<D: serde::Deserializer<'de>>(self, de: D) -> Result<(), D::Error> {
            de.deserialize_seq(self)
        }
    }

    impl<'de> Visitor<'de> for List<'_> {
        type Value = ();
        fn expecting(&self, f: &mut fmt::Formatter) -> fmt::Result {
            f.write_str("a list of ComfyUI queue items")
        }
        fn visit_seq<A: SeqAccess<'de>>(self, mut seq: A) -> Result<(), A::Error> {
            while let Some(id) = seq.next_element_seed(Item)? {
                let (count, ours) = if self.is_running {
                    (&mut self.scan.running, &mut self.scan.ours_running)
                } else {
                    (&mut self.scan.pending, &mut self.scan.ours_pending)
                };
                *count += 1;
                let found = self.ours.contains(&id);
                if found {
                    ours.push(id);
                }
                if (self.stop_on_ours && found) || (self.stop_on_foreign && !found) {
                    *self.settled = true;
                    return Err(A::Error::custom("found"));
                }
            }
            Ok(())
        }
    }

    /// One queue item: `[number, prompt_id, prompt, extra_data, outputs]`.
    struct Item;

    impl<'de> DeserializeSeed<'de> for Item {
        type Value = String;
        fn deserialize<D: serde::Deserializer<'de>>(self, de: D) -> Result<String, D::Error> {
            de.deserialize_seq(self)
        }
    }

    impl<'de> Visitor<'de> for Item {
        type Value = String;
        fn expecting(&self, f: &mut fmt::Formatter) -> fmt::Result {
            f.write_str("a ComfyUI queue item")
        }
        fn visit_seq<A: SeqAccess<'de>>(self, mut seq: A) -> Result<String, A::Error> {
            let missing = || A::Error::custom("queue item has no prompt id");
            seq.next_element::<IgnoredAny>()?.ok_or_else(missing)?;
            let id = seq.next_element::<String>()?.ok_or_else(missing)?;
            while seq.next_element::<IgnoredAny>()?.is_some() {}
            Ok(id)
        }
    }
}

/// Streaming visitors for `/history/<id>`: only `status.status_str` of the
/// requested entry is kept; outputs and the workflow are skipped.
mod history_scan {
    use serde::de::{DeserializeSeed, IgnoredAny, MapAccess, Visitor};
    use std::fmt;

    /// `{ "<id>": { ..., "status": { "status_str": "success", ... } } }`
    pub(super) struct History<'a> {
        pub id: &'a str,
        pub status: &'a mut Option<String>,
    }

    impl<'de> Visitor<'de> for History<'_> {
        type Value = ();
        fn expecting(&self, f: &mut fmt::Formatter) -> fmt::Result {
            f.write_str("a ComfyUI history object")
        }
        fn visit_map<A: MapAccess<'de>>(self, mut map: A) -> Result<(), A::Error> {
            while let Some(key) = map.next_key::<String>()? {
                if key == self.id {
                    map.next_value_seed(Member {
                        name: "status",
                        found: &mut *self.status,
                        then: Some("status_str"),
                    })?;
                } else {
                    map.next_value::<IgnoredAny>()?;
                }
            }
            Ok(())
        }
    }

    /// An object whose member `name` is either the string sought (`then` is
    /// `None`) or another object to search for member `then`. Any other
    /// shape leaves `found` unset, as a missing status does.
    struct Member<'a> {
        name: &'static str,
        found: &'a mut Option<String>,
        then: Option<&'static str>,
    }

    impl<'de> DeserializeSeed<'de> for Member<'_> {
        type Value = ();
        fn deserialize<D: serde::Deserializer<'de>>(self, de: D) -> Result<(), D::Error> {
            de.deserialize_any(self)
        }
    }

    impl<'de> Visitor<'de> for Member<'_> {
        type Value = ();
        fn expecting(&self, f: &mut fmt::Formatter) -> fmt::Result {
            f.write_str("a ComfyUI history entry")
        }
        fn visit_map<A: MapAccess<'de>>(self, mut map: A) -> Result<(), A::Error> {
            while let Some(key) = map.next_key::<String>()? {
                if key != self.name {
                    map.next_value::<IgnoredAny>()?;
                    continue;
                }
                match self.then {
                    Some(inner) => map.next_value_seed(Member {
                        name: inner,
                        found: &mut *self.found,
                        then: None,
                    })?,
                    None => {
                        *self.found = map
                            .next_value::<serde_json::Value>()?
                            .as_str()
                            .map(str::to_owned);
                    }
                }
            }
            Ok(())
        }
        fn visit_seq<A: serde::de::SeqAccess<'de>>(self, mut seq: A) -> Result<(), A::Error> {
            while seq.next_element::<IgnoredAny>()?.is_some() {}
            Ok(())
        }
        fn visit_str<E: serde::de::Error>(self, _: &str) -> Result<(), E> {
            Ok(())
        }
        fn visit_bool<E: serde::de::Error>(self, _: bool) -> Result<(), E> {
            Ok(())
        }
        fn visit_i64<E: serde::de::Error>(self, _: i64) -> Result<(), E> {
            Ok(())
        }
        fn visit_u64<E: serde::de::Error>(self, _: u64) -> Result<(), E> {
            Ok(())
        }
        fn visit_f64<E: serde::de::Error>(self, _: f64) -> Result<(), E> {
            Ok(())
        }
        fn visit_unit<E: serde::de::Error>(self) -> Result<(), E> {
            Ok(())
        }
    }
}

/// ComfyUI mirrors its routes under /api. Normalize exactly one prefix;
/// this also handles current /api/jobs routes mirrored as /api/api/jobs.
fn strip_api_prefix(path: &str) -> &str {
    path.strip_prefix("/api")
        .filter(|suffix| suffix.starts_with('/'))
        .unwrap_or(path)
}

/// The path as ComfyUI's router sees it: aiohttp matches routes against the
/// percent-decoded path, so `/%68istory` reaches `/history`. Dot segments
/// that decoding exposes are then resolved. The forwarded request keeps its
/// original bytes; this form is only compared against, and errs toward
/// recognizing an owner-data route.
fn router_path(path: &str) -> String {
    let decoded = percent_decoded(path);
    reqwest::Url::parse(&format!("http://comfyui.invalid{decoded}"))
        .map(|url| url.path().to_owned())
        .unwrap_or(decoded)
}

/// Reads of what the instance holds for whoever else uses it: the full
/// history, queue and job lists (every prompt and its outputs), users, settings,
/// saved workflows, logs and file listings, plus ComfyUI-Manager, whose
/// routes install, update and remove custom nodes and restart the server,
/// some of them on `GET`. A borrower still reads its own job by ID
/// (`/history/<id>`, `/jobs/<id>`), its images through `/view`, and the
/// node and model catalogue.
fn is_owner_data(method: &http::Method, path: &str) -> bool {
    let path = router_path(path);
    let path = strip_api_prefix(&path);
    // ComfyUI-Manager's route families, unversioned and under `/v2`. Every
    // method: some of these act on `GET`.
    const MANAGER: [&str; 5] = [
        "/manager/",
        "/customnode/",
        "/snapshot/",
        "/externalmodel/",
        "/comfyui_manager/",
    ];
    let unversioned = path.strip_prefix("/v2").unwrap_or(path);
    if MANAGER.iter().any(|prefix| unversioned.starts_with(prefix)) {
        return true;
    }
    if !matches!(*method, http::Method::GET | http::Method::HEAD) {
        return false;
    }
    // `/api/jobs` is the canonical job list; `/jobs` is its one-prefix
    // normalization.
    matches!(
        path,
        "/history"
            | "/queue"
            | "/jobs"
            | "/api/jobs"
            | "/users"
            | "/settings"
            | "/userdata"
            | "/v2/userdata"
    ) || ["/settings/", "/userdata/", "/v2/userdata/", "/internal/"]
        .iter()
        .any(|prefix| path.starts_with(prefix))
}

fn is_job_cancel(path: &str) -> bool {
    path.strip_prefix("/jobs/")
        .or_else(|| path.strip_prefix("/api/jobs/"))
        .and_then(|rest| rest.strip_suffix("/cancel"))
        .is_some_and(is_safe_segment)
}

fn is_single_child(path: &str, prefix: &str) -> bool {
    path.strip_prefix(prefix).is_some_and(is_safe_segment)
}

fn is_userdata_move(path: &str) -> bool {
    path.strip_prefix("/userdata/")
        .and_then(|rest| rest.split_once("/move/"))
        .is_some_and(|(source, dest)| is_safe_segment(source) && is_safe_segment(dest))
}

#[cfg(test)]
mod tests {
    use super::*;
    use http::Method;

    #[test]
    fn classifies_work_and_ui_management_routes() {
        let a = ComfyUi;
        assert_eq!(a.classify(&Method::POST, "/prompt"), RequestClass::Work);
        assert_eq!(a.classify(&Method::POST, "/api/prompt"), RequestClass::Work);
        // A borrower's own results, progress and the catalogue stay
        // reachable during a veto, and need no management permission.
        for path in [
            "/view",
            "/api/view",
            "/history/0f2c5a1e-6b1d-4d6b-9d7e-3b1f0c2a9e11",
            "/api/jobs/0f2c5a1e-6b1d-4d6b-9d7e-3b1f0c2a9e11",
            "/system_stats",
            "/object_info",
            "/ws",
            "/models/checkpoints",
        ] {
            assert_eq!(
                a.classify(&Method::GET, path),
                RequestClass::PassThrough,
                "{path}"
            );
        }
        for path in [
            "/free",
            "/api/free",
            "/interrupt",
            "/queue",
            "/history",
            "/upload/image",
            "/upload/mask",
            "/users",
            "/settings",
            "/settings/Comfy.UseNewMenu",
            "/userdata/workflows%2Fexample.json",
            "/userdata/old.json/move/new.json",
            "/api/jobs/cancel",
            "/api/jobs/123/cancel",
            "/api/api/jobs/123/cancel",
        ] {
            assert_eq!(
                a.classify(&Method::POST, path),
                RequestClass::Management,
                "{path}"
            );
            assert!(
                !a.classify(&Method::POST, path).requires_admission(),
                "{path}"
            );
        }
        assert_eq!(
            a.classify(&Method::DELETE, "/api/userdata/workflows%2Fexample.json"),
            RequestClass::Management
        );
    }

    #[test]
    fn reads_of_the_owners_data_are_management() {
        let a = ComfyUi;
        for path in [
            "/history",
            "/api/history",
            "/queue",
            "/api/jobs",
            "/api/api/jobs",
            "/users",
            "/settings",
            "/settings/Comfy.UseNewMenu",
            "/userdata",
            "/userdata/workflows%2Fexample.json",
            "/v2/userdata",
            "/api/v2/userdata",
            "/internal/logs",
            "/internal/files/output",
            // ComfyUI-Manager, which also acts on GET.
            "/manager/reboot",
            "/customnode/installed",
            "/snapshot/restore",
            "/externalmodel/getlist",
            "/comfyui_manager/update_all",
            "/v2/manager/queue/start",
            "/api/v2/customnode/getmappings",
            // The same routes as the upstream router resolves them.
            "/%68istory",
            "/api/%68istory",
            "/%61pi/history",
            "/view/../history",
            "/view/%2e%2e/history",
            "/view%2F..%2Fhistory",
            "/history%3Fmax_items=1",
            "/user%64ata/workflows%2Fexample.json",
        ] {
            for method in [Method::GET, Method::HEAD] {
                assert_eq!(
                    a.classify(&method, path),
                    RequestClass::Management,
                    "{method} {path}"
                );
            }
        }
        assert_eq!(
            a.classify(&Method::POST, "/manager/queue/install"),
            RequestClass::Management
        );
        // Lookalikes are not owner data, and not listed reads either.
        for path in ["/historyx", "/viewer/history", "/userdatas"] {
            assert_eq!(
                a.classify(&Method::GET, path),
                RequestClass::UnknownRead,
                "{path}"
            );
        }
    }

    #[test]
    fn borrowers_read_only_their_results_and_the_catalogue() {
        let a = ComfyUi;
        let listed: Vec<String> = a.borrower_reads().iter().map(ReadRoute::shown).collect();
        assert_eq!(
            listed,
            [
                "/prompt",
                "/ws",
                "/history/<id>",
                "/jobs/<id>",
                "/api/jobs/<id>",
                "/view",
                "/object_info",
                "/object_info/<id>",
                "/models",
                "/models/<id>",
                "/embeddings",
                "/system_stats",
            ]
        );
        for path in [
            "/prompt",
            "/api/prompt",
            "/ws",
            "/api/ws",
            "/history/0f2c5a1e-6b1d-4d6b-9d7e-3b1f0c2a9e11",
            "/api/history/0f2c5a1e-6b1d-4d6b-9d7e-3b1f0c2a9e11",
            "/api/jobs/0f2c5a1e-6b1d-4d6b-9d7e-3b1f0c2a9e11",
            "/api/api/jobs/0f2c5a1e-6b1d-4d6b-9d7e-3b1f0c2a9e11",
            "/view",
            "/api/view",
            "/object_info",
            "/object_info/KSampler",
            "/object_info/KSampler%20(Efficient)",
            "/models",
            "/models/checkpoints",
            "/embeddings",
            "/system_stats",
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
        let a = ComfyUi;
        for path in [
            "/",
            "/index.html",
            "/assets/index.js",
            "/extensions",
            "/features",
            "/i18n",
            "/view_metadata/checkpoints",
            "/experiment/models",
            "/api/assets",
            "/templates/default.json",
            "/workflow_templates",
            "/custom_node/route",
            "/history/",
            "/history/a/b",
            "/history/..",
            "/history/%2e%2e",
            "/object_info/x%2F..%2F..%2Fhistory",
            "/models/..%2Fhistory",
            "/%76iew",
            "/api/jobs/one/cancel",
            "/prompt/extra",
        ] {
            for method in [Method::GET, Method::HEAD] {
                assert_ne!(
                    a.classify(&method, path),
                    RequestClass::PassThrough,
                    "{method} {path}"
                );
                assert!(
                    matches!(
                        a.classify(&method, path),
                        RequestClass::UnknownRead | RequestClass::Management
                    ),
                    "{method} {path}"
                );
            }
        }
        for method in [Method::OPTIONS, Method::TRACE, Method::CONNECT] {
            for path in ["/prompt", "/view", "/ws", "/system_stats"] {
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
        let a = ComfyUi;
        for method in [Method::POST, Method::PUT, Method::PATCH, Method::DELETE] {
            assert_eq!(
                a.classify(&method, "/future-inference"),
                RequestClass::UnknownRoute
            );
        }
        for path in [
            "/prompt/extra",
            "/api/promptly",
            "/free/extra",
            "/settings/one/two",
            "/settings/%2E%2e",
            "/userdata/",
            "/userdata/../prompt",
            "/userdata/%2E%2e",
            "/userdata/old.json/move/../prompt",
            "/userdatas/example.json",
            "/api/jobs/one/two/cancel",
            "/api/jobs/%2e%2e/cancel",
        ] {
            assert_eq!(
                a.classify(&Method::POST, path),
                RequestClass::UnknownRoute,
                "{path}"
            );
        }
    }

    #[test]
    fn declares_no_per_model_gating() {
        assert!(!ComfyUi.capabilities().reports_model);
        assert!(ComfyUi.capabilities().can_unload);
    }

    #[test]
    fn versions_parse_from_package_and_git_forms() {
        for (text, expected) in [
            ("0.3.57", Some([0, 3, 57])),
            ("v0.3.57-5-gabc1234", Some([0, 3, 57])),
            ("0.26.0", Some([0, 26, 0])),
            (" 0.4 ", Some([0, 4, 0])),
            ("1.2.3rc1", Some([1, 2, 3])),
            ("unknown", None),
            ("", None),
            ("v", None),
            ("0", None),
            ("x0.3.57", None),
        ] {
            assert_eq!(parse_version(text), expected, "{text:?}");
        }
        assert!(parse_version("0.3.56").unwrap() < SCOPED_INTERRUPT_SINCE);
        assert!(parse_version("0.4.0").unwrap() > SCOPED_INTERRUPT_SINCE);
        assert!(parse_version("0.10.0").unwrap() > SCOPED_INTERRUPT_SINCE);
    }

    #[test]
    fn the_assigned_id_is_appended_without_rewriting_the_workflow() {
        let id = "0f2c5a1e-6b1d-4d6b-9d7e-3b1f0c2a9e11";
        let forwarded =
            |body: &str| String::from_utf8(prompt_with_id(body.as_bytes(), id).unwrap()).unwrap();
        assert_eq!(forwarded("{}"), format!("{{\"prompt_id\":\"{id}\"}}"));
        assert_eq!(
            forwarded("\r\n{\"prompt\":{\"1\":1e400}}\t"),
            format!("\r\n{{\"prompt\":{{\"1\":1e400}},\"prompt_id\":\"{id}\"}}")
        );
        let replaced: serde_json::Value =
            serde_json::from_str(&forwarded(r#"{"prompt_id":"owner-1","prompt":{}}"#)).unwrap();
        assert_eq!(replaced["prompt_id"], id);
        for bad in ["", "[]", "1", "null", "{", "{}}", "{}x", "\u{feff}{}"] {
            assert!(prompt_with_id(bad.as_bytes(), id).is_err(), "{bad:?}");
        }
    }

    /// Serve `body` on every path, as a release before the jobs route
    /// would: `/api/jobs/...` is aiohttp's plain-text 404.
    async fn serve_json(body: String) -> (String, tokio::task::JoinHandle<()>) {
        let app = axum::Router::new().fallback(move |uri: http::Uri| {
            let body = body.clone();
            async move {
                if uri.path().starts_with("/api/jobs/") {
                    return (
                        http::StatusCode::NOT_FOUND,
                        [("content-type", "text/plain; charset=utf-8")],
                        "404: Not Found".to_owned(),
                    );
                }
                (
                    http::StatusCode::OK,
                    [("content-type", "application/json")],
                    body,
                )
            }
        });
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let base = format!("http://{}", listener.local_addr().unwrap());
        (
            base,
            tokio::spawn(async move { axum::serve(listener, app).await.unwrap() }),
        )
    }

    /// A `/queue` reply with `queue_running` first, in ComfyUI's order.
    fn queue_json(running: &[&str], pending: &[&str], padding: usize) -> String {
        let list = |ids: &[&str]| {
            serde_json::Value::from(
                ids.iter()
                    .map(|id| serde_json::json!([1, id, {"w": "p".repeat(padding)}, {}, []]))
                    .collect::<Vec<_>>(),
            )
        };
        format!(
            "{{\"queue_running\": {}, \"queue_pending\": {}}}",
            list(running),
            list(pending)
        )
    }

    #[tokio::test]
    async fn the_queue_is_scanned_for_our_ids_without_holding_the_workflows() {
        let http = reqwest::Client::new();
        // 3 MiB of workflows, well past the old 1 MiB read cap.
        let (base, server) = serve_json(queue_json(
            &["owner-1"],
            &["owner-2", "job-a", "owner-3"],
            1 << 20,
        ))
        .await;
        let ours = vec!["job-a".to_owned(), "job-b".to_owned()];
        let scan = scan_queue(&http, &base, &ours, Until::End).await.unwrap();
        assert_eq!((scan.running, scan.pending), (1, 3));
        assert!(scan.ours_running.is_empty());
        assert_eq!(scan.ours_pending, vec!["job-a".to_owned()]);
        // A single id stops the read where it is found.
        let found = scan_queue(&http, &base, &["job-a".to_owned()], Until::Found)
            .await
            .unwrap();
        assert_eq!((found.running, found.pending), (1, 2));
        assert_eq!(found.ours_pending, vec!["job-a".to_owned()]);
        server.abort();
    }

    #[tokio::test]
    async fn the_foreign_work_check_stops_at_the_first_prompt_that_is_not_ours() {
        let http = reqwest::Client::new();
        let ours = vec!["job-a".to_owned()];
        // 20 MiB of queue, past the check's 16 MiB bound: it is answered at
        // the end of the first owner prompt without reading the rest.
        let (base, server) = serve_json(queue_json(
            &["job-a"],
            &["owner-1", "owner-2", "owner-3", "owner-4"],
            4 << 20,
        ))
        .await;
        assert_eq!(
            ComfyUi
                .holds_foreign_work(&http, &base, &ours)
                .await
                .unwrap(),
            Some(true)
        );
        server.abort();
        // Only vacancyd's own prompts: none foreign, read to the end.
        let (base, server) = serve_json(queue_json(&["job-a"], &[], 1 << 10)).await;
        assert_eq!(
            ComfyUi
                .holds_foreign_work(&http, &base, &ours)
                .await
                .unwrap(),
            Some(false)
        );
        server.abort();
        // A queue that would need more than the bound to decide is an error,
        // and the caller unloads as it would without the check.
        let big = vec!["job-a".to_owned(), "job-b".to_owned(), "job-c".to_owned()];
        let (base, server) = serve_json(queue_json(&["job-a"], &["job-b", "job-c"], 8 << 20)).await;
        assert!(ComfyUi
            .holds_foreign_work(&http, &base, &big)
            .await
            .is_err());
        server.abort();
    }

    #[tokio::test]
    async fn a_malformed_or_oversized_queue_is_an_error_not_an_empty_queue() {
        let http = reqwest::Client::new();
        for body in [
            r#"{"queue_running": []}"#.to_owned(),
            r#"{"queue_pending": []}"#.to_owned(),
            r#"{"queue_running": [[0]], "queue_pending": []}"#.to_owned(),
            r#"{"queue_running": [[0, 7]], "queue_pending": []}"#.to_owned(),
            r#"{"queue_running": [], "queue_pending": []} trailing"#.to_owned(),
            r#"{"queue_running": [], "queue_pending": [["#.to_owned(),
            "[]".to_owned(),
        ] {
            let (base, server) = serve_json(body.clone()).await;
            let shown: String = body.chars().take(60).collect();
            assert!(
                scan_queue(&http, &base, &["job-a".to_owned()], Until::End)
                    .await
                    .is_err(),
                "{shown}"
            );
            server.abort();
        }
    }

    #[tokio::test]
    async fn history_status_reads_only_the_named_entry() {
        let http = reqwest::Client::new();
        let id = "0f2c5a1e-6b1d-4d6b-9d7e-3b1f0c2a9e11";
        let entry = |status: serde_json::Value| {
            serde_json::json!({
                "other": {"status": {"status_str": "success"}},
                id: {"prompt": [0, id, {"w": "p".repeat(2 << 20)}, {}, []], "outputs": {}, "status": status},
            })
            .to_string()
        };
        for (body, expected) in [
            (
                entry(
                    serde_json::json!({"status_str": "success", "completed": true, "messages": []}),
                ),
                Some("success"),
            ),
            (
                entry(serde_json::json!({"completed": false, "status_str": "error"})),
                Some("error"),
            ),
            (entry(serde_json::json!({"status_str": 3})), None),
            (entry(serde_json::json!("done")), None),
            (entry(serde_json::json!(null)), None),
            ("{}".to_owned(), None),
            (
                serde_json::json!({"other": {"status": {"status_str": "success"}}}).to_string(),
                None,
            ),
        ] {
            let (base, server) = serve_json(body).await;
            assert_eq!(
                history_status(&http, &base, id).await.unwrap().as_deref(),
                expected
            );
            assert_eq!(
                ComfyUi.completed(&http, &base, id).await.unwrap(),
                expected.is_some()
            );
            server.abort();
        }
    }

    #[tokio::test]
    async fn queue_absence_reads_only_until_the_prompt_is_found() {
        let http = reqwest::Client::new();
        // The prompt is running, first in the reply; the rest of a queue of
        // truncated JSON is never read, so the truncation cannot matter.
        let mut body = queue_json(&["job-a"], &["owner-1"], 16);
        body.truncate(body.find("owner-1").unwrap());
        let (base, server) = serve_json(body).await;
        assert!(!ComfyUi.queue_absent(&http, &base, "job-a").await.unwrap());
        let scan = scan_queue(&http, &base, &["job-a".to_owned()], Until::Found)
            .await
            .unwrap();
        assert_eq!((scan.running, scan.pending), (1, 0));
        server.abort();
        for (running, pending, absent) in [
            (vec![], vec!["owner-1", "job-a"], false),
            (vec!["owner-1"], vec!["owner-2"], true),
            (vec![], vec![], true),
        ] {
            let (base, server) = serve_json(queue_json(&running, &pending, 1 << 10)).await;
            assert_eq!(
                ComfyUi.queue_absent(&http, &base, "job-a").await.unwrap(),
                absent,
                "{running:?} {pending:?}"
            );
            server.abort();
        }
        // A reply that is not a queue is an error, never an absence.
        let (base, server) = serve_json(r#"{"queue_running": []}"#.to_owned()).await;
        assert!(ComfyUi.queue_absent(&http, &base, "job-a").await.is_err());
        server.abort();
    }

    /// Serve one raw HTTP reply: headers after `before_headers`, then the
    /// body in `chunks` with `gap` between them, then stall forever if `hang`.
    async fn paced(
        before_headers: Duration,
        chunks: Vec<String>,
        gap: Duration,
        hang: bool,
    ) -> (String, tokio::task::JoinHandle<()>) {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let base = format!("http://{}", listener.local_addr().unwrap());
        let server = tokio::spawn(async move {
            let (mut socket, _) = listener.accept().await.unwrap();
            let mut request = [0u8; 4096];
            let _ = socket.read(&mut request).await;
            tokio::time::sleep(before_headers).await;
            let length: usize = chunks.iter().map(String::len).sum();
            let length = if hang { length + 1 } else { length };
            let head = format!(
                "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {length}\r\n\r\n"
            );
            socket.write_all(head.as_bytes()).await.unwrap();
            for chunk in chunks {
                socket.write_all(chunk.as_bytes()).await.unwrap();
                socket.flush().await.unwrap();
                tokio::time::sleep(gap).await;
            }
            if hang {
                std::future::pending::<()>().await;
            }
        });
        (base, server)
    }

    /// The bounds on a streamed read follow its progress. A reply that
    /// keeps arriving is read however long it takes in total; one that stops
    /// arriving fails after the stall bound; one that never starts fails
    /// after the header bound.
    #[tokio::test]
    async fn streamed_reads_are_bounded_by_progress_not_by_their_length() {
        let http = reqwest::Client::new();
        let bounds = ReadBounds {
            max_bytes: 1 << 20,
            headers: Duration::from_millis(400),
            stall: Duration::from_millis(400),
            total: Duration::from_secs(30),
        };
        let parse = |listing: Listing| -> Result<serde_json::Value> {
            Ok(serde_json::from_reader(listing)?)
        };
        // Twelve chunks 150 ms apart: 1.8 s in all, far beyond the stall
        // bound, and each gap well inside it.
        let body = queue_json(&["owner-1"], &["owner-2", "job-a"], 1 << 10);
        let chunks: Vec<String> = body
            .as_bytes()
            .chunks(body.len().div_ceil(12))
            .map(|c| String::from_utf8(c.to_vec()).unwrap())
            .collect();
        let (base, server) = paced(
            Duration::from_millis(300),
            chunks,
            Duration::from_millis(150),
            false,
        )
        .await;
        let started = std::time::Instant::now();
        let value = stream_json_within(http.get(format!("{base}/queue")), parse, bounds)
            .await
            .unwrap();
        assert!(started.elapsed() > Duration::from_millis(1500));
        assert_eq!(value["queue_pending"][1][1], "job-a");
        server.abort();

        // A body that stops arriving.
        let (base, server) = paced(
            Duration::ZERO,
            vec!["{\"queue_running\": [".into()],
            Duration::ZERO,
            true,
        )
        .await;
        let error = stream_json_within(http.get(format!("{base}/queue")), parse, bounds)
            .await
            .unwrap_err()
            .to_string();
        assert!(error.contains("stalled"), "{error}");
        server.abort();

        // Headers that never come.
        let (base, server) = paced(Duration::from_secs(3600), vec![], Duration::ZERO, false).await;
        let error = stream_json_within(http.get(format!("{base}/queue")), parse, bounds)
            .await
            .unwrap_err()
            .to_string();
        assert!(error.contains("no reply within"), "{error}");
        server.abort();

        // A reply beyond the size limit, however quickly it arrives.
        let (base, server) = serve_json(queue_json(&[], &["owner-1"; 3], 1 << 19)).await;
        let error = stream_json_within(http.get(format!("{base}/queue")), parse, bounds)
            .await
            .unwrap_err()
            .to_string();
        assert!(error.contains("exceeds 1 MiB"), "{error}");
        server.abort();

        // A reply that trickles without stalling still ends at the total bound.
        let trickle: Vec<String> = std::iter::repeat_n(" ".to_owned(), 100).collect();
        let (base, server) =
            paced(Duration::ZERO, trickle, Duration::from_millis(100), false).await;
        let tight = ReadBounds {
            total: Duration::from_millis(900),
            ..bounds
        };
        let error = stream_json_within(http.get(format!("{base}/queue")), parse, tight)
            .await
            .unwrap_err()
            .to_string();
        assert!(error.contains("longer than"), "{error}");
        server.abort();
    }

    /// A release with the jobs route: `(id, status)` pairs, and a JSON 404
    /// for any other id. `/queue` answers 500, so a test fails if it is read.
    async fn serve_jobs(
        jobs: &'static [(&'static str, &'static str)],
    ) -> (
        String,
        tokio::task::JoinHandle<()>,
        Arc<std::sync::atomic::AtomicUsize>,
    ) {
        use axum::extract::Path;
        let lookups = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let counted = lookups.clone();
        let app = axum::Router::new()
            .route(
                "/api/jobs/{id}",
                axum::routing::get(move |Path(id): Path<String>| {
                    let counted = counted.clone();
                    async move {
                        counted.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
                        match jobs.iter().find(|(job, _)| *job == id) {
                            Some((_, status)) => (
                                http::StatusCode::OK,
                                axum::Json(
                                    serde_json::json!({"id": id, "status": status, "priority": 3}),
                                ),
                            ),
                            None => (
                                http::StatusCode::NOT_FOUND,
                                axum::Json(serde_json::json!({"error": "Job not found"})),
                            ),
                        }
                    }
                }),
            )
            .route(
                "/queue",
                axum::routing::get(|| async { http::StatusCode::INTERNAL_SERVER_ERROR }),
            );
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let base = format!("http://{}", listener.local_addr().unwrap());
        (
            base,
            tokio::spawn(async move { axum::serve(listener, app).await.unwrap() }),
            lookups,
        )
    }

    use std::sync::Arc;

    /// ComfyUI 0.6.0 and later answer one job lookup per id, whatever the
    /// queue holds; `/queue` is never read.
    #[tokio::test]
    async fn prompts_are_located_through_the_jobs_route_where_it_exists() {
        let http = reqwest::Client::new();
        let (base, server, lookups) = serve_jobs(&[
            ("job-p", "pending"),
            ("job-r", "in_progress"),
            ("job-c", "completed"),
            ("job-f", "failed"),
            ("job-x", "cancelled"),
        ])
        .await;
        let ids: Vec<String> = ["job-p", "job-r", "job-c", "job-f", "job-x", "job-gone"]
            .iter()
            .map(|s| s.to_string())
            .collect();
        assert_eq!(
            locate(&http, &base, &ids).await.unwrap(),
            vec![
                Place::Pending,
                Place::Running,
                Place::Finished,
                Place::Finished,
                Place::Finished,
                Place::Unknown
            ]
        );
        assert_eq!(lookups.load(std::sync::atomic::Ordering::SeqCst), 6);
        assert!(!ComfyUi.queue_absent(&http, &base, "job-p").await.unwrap());
        assert!(!ComfyUi.queue_absent(&http, &base, "job-r").await.unwrap());
        assert!(ComfyUi.queue_absent(&http, &base, "job-c").await.unwrap());
        assert!(ComfyUi
            .queue_absent(&http, &base, "job-gone")
            .await
            .unwrap());
        server.abort();
    }

    /// A reply the lookup does not recognize is an error, never a place.
    #[tokio::test]
    async fn an_unrecognized_job_reply_is_an_error() {
        let http = reqwest::Client::new();
        for (status, content_type, body) in [
            (
                200,
                "application/json",
                r#"{"id": "job-a", "status": "paused"}"#,
            ),
            (200, "application/json", r#"{"id": "job-a"}"#),
            (200, "text/html", "<html></html>"),
            (404, "application/json", r#"{"detail": "nope"}"#),
            (500, "application/json", r#"{"error": "boom"}"#),
        ] {
            let app = axum::Router::new().fallback(move || async move {
                (
                    http::StatusCode::from_u16(status).unwrap(),
                    [("content-type", content_type)],
                    body,
                )
            });
            let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
            let base = format!("http://{}", listener.local_addr().unwrap());
            let server = tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
            assert!(
                locate(&http, &base, &["job-a".to_owned()]).await.is_err(),
                "{status} {body}"
            );
            server.abort();
        }
    }

    /// Observation counts queued work from `GET /prompt`, never `/queue`.
    #[tokio::test]
    async fn observation_reads_the_queue_count_not_the_queue() {
        let http = reqwest::Client::new();
        for (remaining, busy, state) in [(0, false, "unknown"), (3, true, "running")] {
            let app = axum::Router::new()
                .route(
                    "/prompt",
                    axum::routing::get(move || async move {
                        axum::Json(serde_json::json!({"exec_info": {"queue_remaining": remaining}}))
                    }),
                )
                .route(
                    "/system_stats",
                    axum::routing::get(|| async { axum::Json(serde_json::json!({"devices": []})) }),
                )
                .route(
                    "/queue",
                    axum::routing::get(|| async { http::StatusCode::INTERNAL_SERVER_ERROR }),
                );
            let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
            let base = format!("http://{}", listener.local_addr().unwrap());
            let server = tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
            let models = ComfyUi.observe_models(&http, &base).unwrap().await.unwrap();
            assert_eq!(models.busy, busy);
            assert_eq!(models.models[0].state, state);
            server.abort();
        }
    }

    /// The observation and the job lookups against a real ComfyUI, for
    /// checking a release by hand (a throwaway instance; stock CPU nodes):
    ///
    /// ```text
    /// VACANCYD_TEST_COMFYUI=http://127.0.0.1:8188 cargo test --locked -- --ignored real_comfyui_observation
    /// ```
    ///
    /// ComfyUI 0.6.0 and later answer through the jobs route; earlier
    /// releases through the `/queue` fallback, where a finished prompt reads
    /// as unknown.
    #[tokio::test]
    #[ignore = "needs a throwaway ComfyUI at VACANCYD_TEST_COMFYUI"]
    async fn real_comfyui_observation_and_job_lookups() {
        let base = std::env::var("VACANCYD_TEST_COMFYUI").expect("VACANCYD_TEST_COMFYUI");
        let base = base.trim_end_matches('/').to_owned();
        let http = reqwest::Client::new();
        let jobs_route = http
            .get(format!(
                "{base}/api/jobs/{}",
                "0f2c5a1e-6b1d-4d6b-9d7e-3b1f0c2a9e11"
            ))
            .send()
            .await
            .unwrap()
            .headers()
            .get(http::header::CONTENT_TYPE)
            .is_some_and(|v| v.to_str().unwrap_or("").starts_with("application/json"));
        let idle = ComfyUi.observe_models(&http, &base).unwrap().await.unwrap();
        assert!(!idle.busy, "the test ComfyUI must start idle");
        let color = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .subsec_nanos()
            % 0xFF_FFFF;
        let graph = |blurs: usize, color: u32| {
            let mut graph = serde_json::json!({"1": {"class_type": "EmptyImage",
                "inputs": {"width": 1024, "height": 1024, "batch_size": 1, "color": color}}});
            let mut previous = "1".to_owned();
            for n in 0..blurs {
                let id = (n + 2).to_string();
                graph[&id] = serde_json::json!({"class_type": "ImageBlur",
                    "inputs": {"image": [previous, 0], "blur_radius": 31, "sigma": 10.0}});
                previous = id;
            }
            graph["999"] = serde_json::json!({"class_type": "PreviewImage", "inputs": {"images": [previous, 0]}});
            graph
        };
        let submit = |blurs: usize, color: u32| {
            let request = http
                .post(format!("{base}/prompt"))
                .json(&serde_json::json!({"prompt": graph(blurs, color)}));
            async move {
                let reply: serde_json::Value = request.send().await.unwrap().json().await.unwrap();
                reply["prompt_id"].as_str().unwrap().to_owned()
            }
        };
        // One quick prompt to finish, then a long one running and one waiting.
        let finished = submit(1, color).await;
        for _ in 0..100 {
            if history_status(&http, &base, &finished)
                .await
                .unwrap()
                .is_some()
            {
                break;
            }
            tokio::time::sleep(Duration::from_millis(200)).await;
        }
        let running = submit(40, color + 1).await;
        let pending = submit(1, color + 2).await;
        tokio::time::sleep(Duration::from_secs(2)).await;
        let busy = ComfyUi.observe_models(&http, &base).unwrap().await.unwrap();
        assert!(busy.busy);
        assert_eq!(busy.models[0].state, "running");
        let ids = vec![
            running.clone(),
            pending.clone(),
            finished.clone(),
            "0f2c5a1e-6b1d-4d6b-9d7e-3b1f0c2a9e11".to_owned(),
        ];
        let places = locate(&http, &base, &ids).await.unwrap();
        let finished_place = if jobs_route {
            Place::Finished
        } else {
            Place::Unknown
        };
        assert_eq!(
            places,
            vec![
                Place::Running,
                Place::Pending,
                finished_place,
                Place::Unknown
            ],
            "jobs route {jobs_route}"
        );
        eprintln!("jobs route {jobs_route}: {places:?}");
        let _ = http
            .post(format!("{base}/queue"))
            .json(&serde_json::json!({"clear": true}))
            .send()
            .await;
        let _ = http
            .post(format!("{base}/interrupt"))
            .json(&serde_json::json!({}))
            .send()
            .await;
        // Leave it idle for the next test.
        for _ in 0..300 {
            if !ComfyUi
                .observe_models(&http, &base)
                .unwrap()
                .await
                .unwrap()
                .busy
            {
                return;
            }
            tokio::time::sleep(Duration::from_millis(200)).await;
        }
        panic!("the test ComfyUI did not return to idle");
    }
}
