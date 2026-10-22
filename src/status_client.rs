//! Bounded local console transport. No URL, credentials or configuration from the
//! renderer is accepted. A failed poll replaces, rather than revives, old data.
use std::time::{Duration, Instant};

use futures_util::StreamExt;
use serde::{Deserialize, Serialize};
use tokio::sync::Mutex;

use crate::settings::{SettingsSnapshot, SettingsUpdate};
use crate::status::{decode, DecodeError, Status, MAX_STATUS_BYTES};

const STATUS_URL: &str = "http://127.0.0.1:9811/status";
const REQUEST_TIMEOUT: Duration = Duration::from_secs(2);
const MIN_POLL_INTERVAL: Duration = Duration::from_secs(1);
/// Largest diagnostics export read. A tracked GPU contributes about 1 MiB
/// of recent pressure samples at the default poll cadence and about 5 MiB
/// at the fastest.
pub const MAX_DIAGNOSTICS_BYTES: usize = 64 * 1024 * 1024;
/// Assembling an export reads files and copies the sample history.
pub const DIAGNOSTICS_TIMEOUT: Duration = Duration::from_secs(30);

#[derive(Debug, Clone, Serialize)]
#[serde(tag = "state", rename_all = "snake_case")]
pub enum Poll {
    Online {
        status: Box<Status>,
        console_version: &'static str,
    },
    Unavailable {
        reason: &'static str,
    },
    Incompatible {
        schema_version: u32,
    },
}

/// The console can select only an existing lending switch, never a URL or verb.
#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct ControlRequest {
    pub enabled: bool,
    pub resource: Option<String>,
}

/// The console's debug logging switch: open a window of
/// [`CONSOLE_DEBUG_MINUTES`], or end the open one. The length is fixed here,
/// not chosen by the renderer.
#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct DebugLoggingRequest {
    pub enabled: bool,
}

/// How long a debug window the console opens lasts.
pub const CONSOLE_DEBUG_MINUTES: u64 = 30;

/// What happened to a diagnostics export the owner asked the console for.
#[derive(Debug, Serialize)]
#[serde(tag = "state", rename_all = "snake_case")]
pub enum ExportResult {
    Saved {
        path: String,
    },
    /// The owner closed the save dialog.
    Cancelled,
    Failed {
        reason: String,
    },
}

#[derive(Debug, Serialize)]
#[serde(tag = "state", rename_all = "snake_case")]
pub enum ControlResult {
    Applied { enabled: bool },
    Failed { reason: &'static str },
}

/// A settings read has no retained fallback: unavailable means there is no
/// document safe for the renderer to edit.
#[derive(Debug, Clone, Serialize)]
#[serde(tag = "state", rename_all = "snake_case")]
pub enum SettingsPoll {
    Online { settings: Box<SettingsSnapshot> },
    Unavailable { reason: String },
}

#[derive(Debug, Clone, Serialize)]
#[serde(tag = "state", rename_all = "snake_case")]
pub enum SettingsSaveResult {
    Saved {
        settings: Box<SettingsSnapshot>,
        restart_required: bool,
    },
    Failed {
        reason: String,
    },
}

#[derive(Deserialize)]
struct SavedSettingsResponse {
    #[serde(flatten)]
    settings: SettingsSnapshot,
    restart_required: bool,
}

pub struct StatusClient {
    http: reqwest::Client,
    endpoint: String,
    cache: Mutex<Option<(Instant, Poll)>>,
    mutation: Mutex<()>,
}

impl StatusClient {
    pub fn new() -> Result<Self, reqwest::Error> {
        Ok(Self {
            http: reqwest::Client::builder()
                .no_proxy()
                .redirect(reqwest::redirect::Policy::none())
                .timeout(REQUEST_TIMEOUT)
                .build()?,
            endpoint: STATUS_URL.to_owned(),
            cache: Mutex::new(None),
            mutation: Mutex::new(()),
        })
    }

    pub async fn set_lending(&self, request: ControlRequest) -> ControlResult {
        let Ok(_mutation) = self.mutation.try_lock() else {
            return ControlResult::Failed {
                reason: "Another change is in progress. Wait for its result.",
            };
        };
        let result = match tokio::time::timeout(REQUEST_TIMEOUT, self.change(&request)).await {
            Ok(Ok(())) => ControlResult::Applied { enabled: request.enabled },
            Ok(Err(reason)) => ControlResult::Failed { reason },
            Err(_) => ControlResult::Failed { reason: "The change timed out. Its outcome is unknown; check current status before trying again." },
        };
        // Never revive pre-mutation observations, including after an ambiguous timeout.
        *self.cache.lock().await = None;
        result
    }

    pub async fn read_settings(&self) -> SettingsPoll {
        let Ok(_mutation) = self.mutation.try_lock() else {
            return SettingsPoll::Unavailable {
                reason: "Another local change is in progress.".into(),
            };
        };
        match tokio::time::timeout(REQUEST_TIMEOUT, self.read_settings_once()).await {
            Ok(Ok(settings)) => SettingsPoll::Online {
                settings: Box::new(settings),
            },
            Ok(Err(reason)) => SettingsPoll::Unavailable {
                reason: reason.into(),
            },
            Err(_) => SettingsPoll::Unavailable {
                reason: "The local service did not respond within two seconds.".into(),
            },
        }
    }

    pub async fn save_settings(&self, settings: SettingsUpdate) -> SettingsSaveResult {
        let Ok(_mutation) = self.mutation.try_lock() else {
            return SettingsSaveResult::Failed {
                reason: "Another change is in progress. Wait for its result.".into(),
            };
        };
        let result = match tokio::time::timeout(REQUEST_TIMEOUT, self.save_settings_once(&settings)).await {
            Ok(Ok(reply)) if reply.restart_required == reply.settings.application.restart_required => SettingsSaveResult::Saved { restart_required: reply.restart_required, settings: Box::new(reply.settings) },
            Ok(Ok(_)) => SettingsSaveResult::Failed { reason: "The local service returned an invalid save acknowledgement.".into() },
            Ok(Err(reason)) => SettingsSaveResult::Failed { reason: reason.into() },
            Err(_) => SettingsSaveResult::Failed { reason: "The save timed out. Its outcome is unknown; refresh settings before trying again.".into() },
        };
        *self.cache.lock().await = None;
        result
    }

    /// Run a change made over another local channel (the Windows admin
    /// pipe) under the same single-flight rule as this client's own changes,
    /// then drop the cached status so no pre-change observation is shown.
    pub async fn exclusive<T>(&self, busy: T, change: impl std::future::Future<Output = T>) -> T {
        let Ok(_mutation) = self.mutation.try_lock() else {
            return busy;
        };
        let result = change.await;
        *self.cache.lock().await = None;
        result
    }

    /// The daemon's diagnostics export, JSON ready to save.
    pub async fn read_diagnostics(&self) -> Result<Vec<u8>, &'static str> {
        let mut url = reqwest::Url::parse(&self.endpoint).map_err(|_| "Invalid local endpoint.")?;
        url.set_path("/admin/diagnostics");
        fetch_diagnostics(&self.http, url).await
    }

    fn settings_endpoint(&self) -> Result<reqwest::Url, &'static str> {
        let mut url = reqwest::Url::parse(&self.endpoint).map_err(|_| "Invalid local endpoint.")?;
        let mut path = url
            .path_segments_mut()
            .map_err(|_| "Invalid local endpoint.")?;
        path.clear().push("admin").push("settings");
        drop(path);
        Ok(url)
    }

    pub(crate) async fn read_settings_once(&self) -> Result<SettingsSnapshot, &'static str> {
        let response = self
            .http
            .get(self.settings_endpoint()?)
            .header("x-vacancyd-admin", "1")
            .send()
            .await
            .map_err(|_| {
                "Cannot reach the local service. Check that vacancyd is running on port 9811."
            })?;
        if response.status() == reqwest::StatusCode::INTERNAL_SERVER_ERROR {
            // Distinct from an authorization refusal: the service reached
            // the request but could not read its own saved configuration
            // (for example a hand edit made it invalid). Retrying after
            // fixing the file on disk can succeed; retrying an authorization
            // refusal cannot.
            return Err(
                "The local service could not read the saved settings. Check the service log.",
            );
        }
        if !response.status().is_success() {
            return Err("The local service refused the settings read.");
        }
        decode_settings_response(response).await
    }

    async fn save_settings_once(
        &self,
        settings: &SettingsUpdate,
    ) -> Result<SavedSettingsResponse, &'static str> {
        let response = self
            .http
            .post(self.settings_endpoint()?)
            .header("x-vacancyd-admin", "1")
            .json(settings)
            .send()
            .await
            .map_err(|_| "Cannot confirm the save. Refresh settings before trying again.")?;
        match response.status().as_u16() {
            200 => (),
            400 => return Err("The service rejected these settings."),
            401 | 403 => {
                return Err(
                    "Local settings changes are denied by the service's authorization policy.",
                )
            }
            409 => {
                return Err("Settings changed since this screen was opened. Refresh and try again.")
            }
            _ => return Err("The service could not save settings. Refresh before trying again."),
        }
        let bytes = bounded_bytes(response, MAX_STATUS_BYTES).await?;
        serde_json::from_slice(&bytes)
            .map_err(|_| "The local service returned an invalid save acknowledgement.")
    }

    async fn change(&self, request: &ControlRequest) -> Result<(), &'static str> {
        if request.resource.as_ref().is_some_and(|id| {
            id.is_empty()
                || id.len() > 256
                || id == "."
                || id == ".."
                || id.chars().any(char::is_control)
        }) {
            return Err("Invalid resource identifier.");
        }
        let mut url = reqwest::Url::parse(&self.endpoint).map_err(|_| "Invalid local endpoint.")?;
        {
            let mut path = url
                .path_segments_mut()
                .map_err(|_| "Invalid local endpoint.")?;
            path.clear().push("admin");
            if let Some(id) = &request.resource {
                path.push("resources").push(id);
            } else {
                path.push("lending");
            }
            path.push(if request.enabled { "enable" } else { "disable" });
        }
        let response = self
            .http
            .post(url)
            .header("x-vacancyd-admin", "1")
            .send()
            .await
            .map_err(|_| {
                "Cannot confirm the change. Check current service status before trying again."
            })?;
        match response.status().as_u16() {
            401 | 403 => {
                return Err("Local controls are denied by the service's authorization policy.")
            }
            404 => return Err("The service no longer recognizes this resource or control."),
            200 => (),
            _ => return Err("The service refused the change. Refresh status before trying again."),
        }
        let mut body = Vec::new();
        let mut stream = response.bytes_stream();
        while let Some(chunk) = stream.next().await {
            let chunk =
                chunk.map_err(|_| "The acknowledgement was interrupted. Check current status.")?;
            if body.len().saturating_add(chunk.len()) > 4096 {
                return Err("Invalid acknowledgement. Check current status.");
            }
            body.extend_from_slice(&chunk);
        }
        let reply: serde_json::Value = serde_json::from_slice(&body)
            .map_err(|_| "Invalid acknowledgement. Check current status.")?;
        let matches = if let Some(id) = &request.resource {
            reply.get("resource").and_then(|v| v.as_str()) == Some(id.as_str())
                && reply.get("enabled").and_then(|v| v.as_bool()) == Some(request.enabled)
        } else {
            reply.get("lending_enabled").and_then(|v| v.as_bool()) == Some(request.enabled)
        };
        if !matches {
            return Err("The acknowledgement did not match the change. Check current status.");
        }
        Ok(())
    }

    pub async fn poll(&self) -> Poll {
        // A compromised renderer cannot queue arbitrary numbers of fetches.
        let Ok(mut cache) = self.cache.try_lock() else {
            return Poll::Unavailable {
                reason: "A status read is already in progress.",
            };
        };
        if let Some((at, previous)) = cache.as_ref() {
            if at.elapsed() < MIN_POLL_INTERVAL {
                return previous.clone();
            }
        }
        let observed = self.fetch().await;
        *cache = Some((Instant::now(), observed.clone()));
        observed
    }

    async fn fetch(&self) -> Poll {
        // Includes connection, headers and streamed body in one wall-time bound.
        match tokio::time::timeout(REQUEST_TIMEOUT, self.read()).await {
            Ok(result) => result,
            Err(_) => Poll::Unavailable {
                reason: "The local service did not respond within two seconds.",
            },
        }
    }

    async fn read(&self) -> Poll {
        let Ok(response) = self.http.get(&self.endpoint).send().await else {
            return Poll::Unavailable {
                reason:
                    "Cannot reach the local service. Check that vacancyd is running on port 9811.",
            };
        };
        if !response.status().is_success() {
            return Poll::Unavailable {
                reason:
                    "The local service refused the status read. Check its loopback read policy.",
            };
        }
        let mut body = Vec::new();
        let mut stream = response.bytes_stream();
        while let Some(chunk) = stream.next().await {
            let Ok(chunk) = chunk else {
                return Poll::Unavailable {
                    reason: "The local service response was interrupted.",
                };
            };
            if body.len().saturating_add(chunk.len()) > MAX_STATUS_BYTES {
                return Poll::Unavailable {
                    reason: "The local service response exceeded the size limit.",
                };
            }
            body.extend_from_slice(&chunk);
        }
        match decode(&body) {
            Ok(status) => Poll::Online { status: Box::new(status), console_version: env!("CARGO_PKG_VERSION") },
            Err(DecodeError::Incompatible(schema_version)) => Poll::Incompatible { schema_version },
            // A momentary mix of reads (for example the owner veto clearing
            // mid-response) fails the same consistency check a genuine
            // version mismatch does. It resolves itself on the next poll, so
            // say that instead of sending the owner to check versions.
            Err(DecodeError::Inconsistent) => Poll::Unavailable { reason: "The service reported a changing state. Retrying." },
            Err(_) => Poll::Unavailable { reason: "The local service returned an invalid status response. Check service and console versions." },
        }
    }
}

async fn bounded_bytes(
    response: reqwest::Response,
    maximum: usize,
) -> Result<Vec<u8>, &'static str> {
    let mut body = Vec::new();
    let mut stream = response.bytes_stream();
    while let Some(chunk) = stream.next().await {
        let chunk = chunk.map_err(|_| "The local service response was interrupted.")?;
        if body.len().saturating_add(chunk.len()) > maximum {
            return Err("The local service response exceeded the size limit.");
        }
        body.extend_from_slice(&chunk);
    }
    Ok(body)
}

/// Read a diagnostics export from `url` with the admin intent header,
/// within [`DIAGNOSTICS_TIMEOUT`] and [`MAX_DIAGNOSTICS_BYTES`]. The bytes
/// are returned as the daemon formatted them, ending in a newline; anything
/// but one JSON object is refused rather than saved.
pub async fn fetch_diagnostics(
    http: &reqwest::Client,
    url: reqwest::Url,
) -> Result<Vec<u8>, &'static str> {
    let read = async {
        let response = http
            .get(url)
            .header("x-vacancyd-admin", "1")
            .timeout(DIAGNOSTICS_TIMEOUT)
            .send()
            .await
            .map_err(|_| "Cannot reach the local service. Check that vacancyd is running.")?;
        match response.status().as_u16() {
            200 => {}
            401 | 403 => {
                return Err(
                    "The service refused the diagnostics read. Its authorization policy denies this connection.",
                )
            }
            404 => {
                return Err("This service version has no diagnostics export. Update vacancyd.")
            }
            _ => return Err("The service could not assemble diagnostics. Check the service log."),
        }
        bounded_bytes(response, MAX_DIAGNOSTICS_BYTES).await
    };
    let bytes = tokio::time::timeout(DIAGNOSTICS_TIMEOUT, read)
        .await
        .map_err(|_| "The service did not finish the diagnostics export in time.")??;
    let invalid = "The service returned an invalid diagnostics document.";
    let is_object = bytes.iter().find(|byte| !byte.is_ascii_whitespace()) == Some(&b'{');
    if !is_object || serde_json::from_slice::<serde::de::IgnoredAny>(&bytes).is_err() {
        return Err(invalid);
    }
    let mut document = bytes;
    if document.last() != Some(&b'\n') {
        document.push(b'\n');
    }
    Ok(document)
}

async fn decode_settings_response(
    response: reqwest::Response,
) -> Result<SettingsSnapshot, &'static str> {
    let bytes = bounded_bytes(response, MAX_STATUS_BYTES).await?;
    serde_json::from_slice(&bytes).map_err(|_| "The local service returned invalid settings.")
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::{response::IntoResponse, routing::get, Router};
    use std::sync::{
        atomic::{AtomicUsize, Ordering},
        Arc,
    };

    async fn server(router: Router) -> (StatusClient, tokio::task::JoinHandle<()>) {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let mut client = StatusClient::new().unwrap();
        // Test-only endpoint injection. Production construction has no setter.
        client.endpoint = format!("http://{address}/status");
        let task = tokio::spawn(async move {
            axum::serve(listener, router).await.unwrap();
        });
        (client, task)
    }

    #[tokio::test]
    async fn outage_clears_success_and_calls_are_rate_limited() {
        let hits = Arc::new(AtomicUsize::new(0));
        let route_hits = hits.clone();
        let (client, task) = server(Router::new().route(
            "/status",
            get(move || {
                route_hits.fetch_add(1, Ordering::SeqCst);
                async { include_str!("../tests/fixtures/status/lending.json") }
            }),
        ))
        .await;
        assert!(matches!(client.poll().await, Poll::Online { .. }));
        for _ in 0..10 {
            assert!(matches!(client.poll().await, Poll::Online { .. }));
        }
        assert_eq!(hits.load(Ordering::SeqCst), 1);
        task.abort();
        let _ = task.await;
        // Expire only the rate-limit cache; don't delay the test by a second.
        client.cache.lock().await.as_mut().unwrap().0 = Instant::now() - MIN_POLL_INTERVAL;
        assert!(matches!(client.poll().await, Poll::Unavailable { .. }));
        assert!(matches!(client.poll().await, Poll::Unavailable { .. }));
    }

    #[tokio::test]
    async fn inconsistent_snapshot_reports_a_retry_hint_not_a_version_mismatch() {
        // A momentary mix of reads (for example lending toggling between
        // building an upstream's refusal and the top-level flag) fails the
        // same consistency check a genuine schema mismatch does. It must
        // not send the owner to check versions for a self-correcting state.
        let mut body: serde_json::Value =
            serde_json::from_str(include_str!("../tests/fixtures/status/lending.json")).unwrap();
        body["lending_enabled"] = serde_json::json!(false);
        let bytes = serde_json::to_string(&body).unwrap();
        let (client, task) = server(Router::new().route(
            "/status",
            get(move || {
                let bytes = bytes.clone();
                async move { bytes }
            }),
        ))
        .await;
        let result = client.poll().await;
        assert!(
            matches!(&result, Poll::Unavailable { reason } if reason.contains("changing state")),
            "{result:?}"
        );
        task.abort();
    }

    #[tokio::test]
    async fn redirects_incompatible_and_oversized_responses_never_become_online() {
        for (code, bytes) in [
            (
                302,
                include_str!("../tests/fixtures/status/lending.json").to_owned(),
            ),
            (200, "{\"schema_version\":999}".to_owned()),
            (200, "x".repeat(MAX_STATUS_BYTES + 1)),
        ] {
            let (client, task) = server(
                Router::new()
                    .route(
                        "/status",
                        get(move || {
                            let bytes = bytes.clone();
                            async move {
                                (
                                    http::StatusCode::from_u16(code).unwrap(),
                                    [("location", "/other")],
                                    bytes,
                                )
                                    .into_response()
                            }
                        }),
                    )
                    .route(
                        "/other",
                        get(|| async { include_str!("../tests/fixtures/status/lending.json") }),
                    ),
            )
            .await;
            let result = client.poll().await;
            assert!(!matches!(result, Poll::Online { .. }));
            if code == 200 && matches!(result, Poll::Incompatible { .. }) {
                assert!(matches!(
                    result,
                    Poll::Incompatible {
                        schema_version: 999
                    }
                ));
            }
            task.abort();
        }
    }

    #[tokio::test]
    async fn stalled_body_is_bounded_and_simultaneous_reads_do_not_queue() {
        let (client, task) = server(Router::new().route(
            "/status",
            get(|| async {
                axum::body::Body::from_stream(futures_util::stream::pending::<
                    Result<axum::body::Bytes, std::io::Error>,
                >())
            }),
        ))
        .await;
        let started = Instant::now();
        let first = client.poll();
        let second = async {
            tokio::task::yield_now().await;
            client.poll().await
        };
        let (a, b) = tokio::join!(first, second);
        assert!(matches!(a, Poll::Unavailable { .. }));
        assert!(matches!(
            b,
            Poll::Unavailable {
                reason: "A status read is already in progress."
            }
        ));
        assert!(started.elapsed() < Duration::from_secs(4));
        task.abort();
    }
    #[tokio::test]
    async fn owner_controls_require_matching_acknowledgement_and_intent() {
        use axum::http::HeaderMap;
        use axum::routing::post;
        let (client, task) = server(Router::new().route(
            "/admin/lending/disable",
            post(|headers: HeaderMap| async move {
                assert_eq!(headers.get("x-vacancyd-admin").unwrap(), "1");
                axum::Json(serde_json::json!({"lending_enabled":false}))
            }),
        ))
        .await;
        assert!(matches!(
            client
                .set_lending(ControlRequest {
                    enabled: false,
                    resource: None
                })
                .await,
            ControlResult::Applied { enabled: false }
        ));
        task.abort();
        for (code, body) in [
            (403, "{}"),
            (302, "{}"),
            (200, "{}"),
            (200, r#"{"lending_enabled":true}"#),
            (200, "broken"),
        ] {
            let (client, task) = server(Router::new().route(
                "/admin/lending/disable",
                post(move || async move { (http::StatusCode::from_u16(code).unwrap(), body) }),
            ))
            .await;
            assert!(matches!(
                client
                    .set_lending(ControlRequest {
                        enabled: false,
                        resource: None
                    })
                    .await,
                ControlResult::Failed { .. }
            ));
            task.abort();
        }
    }

    #[tokio::test]
    async fn resource_controls_are_one_segment_and_reject_unbounded_identifiers() {
        use axum::{extract::Path, routing::post};
        let (client, task) = server(Router::new().route(
            "/admin/resources/{id}/enable",
            post(|Path(id): Path<String>| async move {
                axum::Json(serde_json::json!({"resource":id,"enabled":true}))
            }),
        ))
        .await;
        for id in ["gpu0", "gpu with spaces", "x/admin/work/7/release"] {
            assert!(matches!(
                client
                    .set_lending(ControlRequest {
                        enabled: true,
                        resource: Some(id.into())
                    })
                    .await,
                ControlResult::Applied { .. }
            ));
        }
        for id in ["".to_owned(), "..".to_owned(), "x".repeat(257)] {
            assert!(matches!(
                client
                    .set_lending(ControlRequest {
                        enabled: true,
                        resource: Some(id)
                    })
                    .await,
                ControlResult::Failed { .. }
            ));
        }
        assert!(serde_json::from_str::<ControlRequest>(
            r#"{"enabled":true,"resource":null,"url":"http://example.com"}"#
        )
        .is_err());
        task.abort();
    }

    #[tokio::test]
    async fn simultaneous_controls_do_not_queue_and_stalled_ack_is_bounded() {
        use axum::routing::post;
        let (client, task) = server(Router::new().route(
            "/admin/lending/disable",
            post(|| async {
                axum::body::Body::from_stream(futures_util::stream::pending::<
                    Result<axum::body::Bytes, std::io::Error>,
                >())
            }),
        ))
        .await;
        let request = ControlRequest {
            enabled: false,
            resource: None,
        };
        let start = Instant::now();
        let (a, b) = tokio::join!(
            client.set_lending(request.clone()),
            client.set_lending(request)
        );
        assert!(matches!(a, ControlResult::Failed { .. }));
        assert!(matches!(
            b,
            ControlResult::Failed {
                reason: "Another change is in progress. Wait for its result."
            }
        ));
        assert!(start.elapsed() < Duration::from_secs(4));
        task.abort();
    }

    #[tokio::test]
    async fn settings_reads_and_saves_only_the_fixed_admin_endpoint() {
        use axum::{http::HeaderMap, routing::post, Json};
        let snapshot: SettingsSnapshot =
            serde_json::from_str(include_str!("../tests/fixtures/settings/default.json")).unwrap();
        let update = SettingsUpdate::from(snapshot.clone());
        let get_snapshot = snapshot.clone();
        let post_snapshot = snapshot.clone();
        let (client, task) = server(
            Router::new().route(
                "/admin/settings",
                get(move |headers: HeaderMap| {
                    let snapshot = get_snapshot.clone();
                    async move {
                        assert_eq!(headers.get("x-vacancyd-admin").unwrap(), "1");
                        Json(snapshot)
                    }
                })
                .post(post(
                    move |headers: HeaderMap, Json(update): Json<SettingsUpdate>| {
                        let snapshot = post_snapshot.clone();
                        async move {
                            assert_eq!(headers.get("x-vacancyd-admin").unwrap(), "1");
                            assert_eq!(update.revision, snapshot.revision);
                            let mut reply = serde_json::to_value(snapshot).unwrap();
                            reply["restart_required"] = true.into();
                            Json(reply)
                        }
                    },
                )),
            ),
        )
        .await;
        assert!(matches!(
            client.read_settings().await,
            SettingsPoll::Online { .. }
        ));
        assert!(matches!(
            client.save_settings(update).await,
            SettingsSaveResult::Saved {
                restart_required: true,
                ..
            }
        ));
        task.abort();
    }

    #[tokio::test]
    async fn diagnostics_are_read_with_intent_bounded_and_saved_as_sent() {
        use axum::http::{HeaderMap, StatusCode};
        let (client, task) = server(Router::new().route(
            "/admin/diagnostics",
            get(|headers: HeaderMap| async move {
                assert_eq!(headers.get("x-vacancyd-admin").unwrap(), "1");
                r#"{"schema_version":1,"logging":{"level":"info"}}"#
            }),
        ))
        .await;
        let saved = client.read_diagnostics().await.unwrap();
        assert_eq!(
            String::from_utf8(saved).unwrap(),
            "{\"schema_version\":1,\"logging\":{\"level\":\"info\"}}\n"
        );
        task.abort();

        for (status, body, reason) in [
            (StatusCode::FORBIDDEN, "{}", "authorization policy"),
            (StatusCode::NOT_FOUND, "", "no diagnostics export"),
            (StatusCode::INTERNAL_SERVER_ERROR, "", "could not assemble"),
            (StatusCode::OK, "[1, 2]", "invalid diagnostics"),
            (StatusCode::OK, "{truncated", "invalid diagnostics"),
        ] {
            let (client, task) = server(Router::new().route(
                "/admin/diagnostics",
                get(move || async move { (status, body) }),
            ))
            .await;
            let error = client.read_diagnostics().await.unwrap_err();
            assert!(error.contains(reason), "{status}: {error}");
            task.abort();
        }
    }

    #[test]
    fn console_debug_and_export_contracts_are_strict_and_tagged() {
        let request: DebugLoggingRequest = serde_json::from_str(r#"{"enabled":true}"#).unwrap();
        assert!(request.enabled);
        // The renderer cannot choose the window's length or anything else.
        for refused in [r#"{"enabled":true,"minutes":240}"#, r#"{}"#] {
            assert!(serde_json::from_str::<DebugLoggingRequest>(refused).is_err());
        }
        for (result, expected) in [
            (
                ExportResult::Saved {
                    path: r"D:\exports\vacancyd-diagnostics.json".into(),
                },
                serde_json::json!({"state": "saved", "path": r"D:\exports\vacancyd-diagnostics.json"}),
            ),
            (
                ExportResult::Cancelled,
                serde_json::json!({"state": "cancelled"}),
            ),
            (
                ExportResult::Failed {
                    reason: "no".into(),
                },
                serde_json::json!({"state": "failed", "reason": "no"}),
            ),
        ] {
            assert_eq!(serde_json::to_value(result).unwrap(), expected);
        }
    }
}
