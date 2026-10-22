//! `GET /admin/diagnostics`: one JSON document an owner can save and attach
//! to a report, capturing what the daemon saw and decided without needing
//! debug logging to have been on beforehand.
//!
//! It holds the status document, the running settings, the log level, the
//! saved pressure calibration, each tracked resource's recent pressure
//! samples and the recent reclaims with their causes. Credentials never appear: settings carry only how many are
//! configured, and upstream targets are shown through
//! [`vacancyd::status::RedactedUrl`] like everywhere else.

use std::collections::BTreeMap;
use std::sync::Arc;

use axum::extract::State;
use axum::http::StatusCode;
use axum::response::IntoResponse;
use axum::Json;
use serde::Serialize;
use vacancyd::settings::SettingsSnapshot;
use vacancyd::status as wire;

use crate::plugin::pressure::{PressureStore, StoreRead};
use crate::pressure_history::PressureRecord;
use crate::state::AppState;

/// Version of this document's layout. Fields may be added within a version.
pub const SCHEMA_VERSION: u32 = 1;

#[derive(Debug, Serialize)]
pub struct Diagnostics {
    pub schema_version: u32,
    /// When the document was assembled, RFC 3339 UTC.
    pub generated_at: String,
    pub daemon: wire::DaemonIdentity,
    pub platform: Platform,
    pub uptime_s: u64,
    pub logging: wire::Logging,
    /// The same document `GET /status` returns.
    pub status: wire::Status,
    /// The running configuration as the settings API shapes it. Its
    /// `revision` is the configuration this process loaded.
    pub settings: SettingsSnapshot,
    /// Revision of the configuration file on disk now. It differs from
    /// `settings.revision` while a saved change waits for a restart.
    pub saved_settings_revision: Option<String>,
    pub pressure_cache: PressureCache,
    /// Recent pressure samples per tracked resource, oldest first.
    pub pressure_history: BTreeMap<String, PressureHistory>,
    /// Recent reclaims, oldest first: what triggered each, which resources
    /// were refused and why, and what happened to the borrowed work.
    pub reclaims: Vec<crate::reclaim_log::ReclaimEvent>,
}

#[derive(Debug, Serialize)]
pub struct Platform {
    pub os: &'static str,
    pub arch: &'static str,
}

/// The saved calibration file, as read at export time.
#[derive(Debug, Serialize)]
pub struct PressureCache {
    /// `loaded`, `absent`, `unreadable`, `too_large` or `invalid`.
    pub state: &'static str,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub content: Option<PressureStore>,
}

#[derive(Debug, Serialize)]
pub struct PressureHistory {
    /// Samples kept before the oldest is dropped.
    pub limit: usize,
    /// The pressure poll interval the limit was sized for.
    pub poll_ms: u64,
    pub samples: Vec<PressureRecord>,
    /// The busiest processes counted as non-lender work in the latest
    /// sample, busiest first, at most five. Names are the owner's activity
    /// and appear nowhere else.
    pub top_processes: Vec<crate::pressure_history::NonLenderProcess>,
    /// Each attendance profile's learned histogram as of the latest sample:
    /// quantiles, total weight, sample count, target and threshold in force.
    /// Absent before the first sample.
    pub learned: Option<crate::pressure_history::LearnedProfiles>,
}

/// Serve the export, indented so the saved file reads well as it is.
pub async fn export(State(state): State<Arc<AppState>>) -> axum::response::Response {
    // File reads and a document of a few megabytes stay off the runtime.
    let assembled =
        tokio::task::spawn_blocking(move || serde_json::to_vec_pretty(&collect(&state))).await;
    match assembled {
        Ok(Ok(document)) => (
            [(axum::http::header::CONTENT_TYPE, "application/json")],
            document,
        )
            .into_response(),
        _ => (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(serde_json::json!({
                "error": {
                    "type": "diagnostics",
                    "message": "The diagnostics export failed. Check the service log.",
                }
            })),
        )
            .into_response(),
    }
}

pub fn collect(state: &AppState) -> Diagnostics {
    let status = crate::api::status_body(state);
    Diagnostics {
        schema_version: SCHEMA_VERSION,
        generated_at: crate::log_control::rfc3339_utc(std::time::SystemTime::now()),
        daemon: status.daemon.clone(),
        platform: Platform {
            os: std::env::consts::OS,
            arch: std::env::consts::ARCH,
        },
        uptime_s: status.uptime_s,
        logging: state.log.status(),
        settings: running_settings(state),
        saved_settings_revision: crate::api::saved_settings_revision(&state.config_path),
        pressure_cache: pressure_cache(state),
        pressure_history: pressure_history(state),
        reclaims: state.reclaim_log.lock().unwrap().events(),
        status,
    }
}

/// The running configuration's settings, with every upstream target
/// formatted through the redaction used by logs and `/status`.
fn running_settings(state: &AppState) -> SettingsSnapshot {
    let mut settings = crate::api::snapshot_from_config(
        &state.config,
        state.active_config_revision.clone().unwrap_or_default(),
    );
    settings.application = vacancyd::settings::SettingsApplication {
        capacity_revision: state
            .admission
            .lock()
            .unwrap()
            .capacity_revision()
            .map(str::to_owned)
            .or_else(|| state.active_config_revision.clone()),
        restart_required: false,
    };
    for upstream in &mut settings.upstreams {
        upstream.target = vacancyd::status::RedactedUrl(&upstream.target).to_string();
    }
    settings
}

fn pressure_cache(state: &AppState) -> PressureCache {
    let Some(path) = crate::collect::pressure::readable_cache_path(
        &state.config.owner.gpu_pressure,
        &state.config_path,
    ) else {
        return PressureCache {
            state: "unreadable",
            content: None,
        };
    };
    let read = PressureStore::read(&path);
    let state = read.as_str();
    match read {
        StoreRead::Loaded(store) => PressureCache {
            state,
            content: Some(store),
        },
        _ => PressureCache {
            state,
            content: None,
        },
    }
}

fn pressure_history(state: &AppState) -> BTreeMap<String, PressureHistory> {
    let pressure = &state.config.owner.gpu_pressure;
    if !pressure.enabled {
        return BTreeMap::new();
    }
    crate::collect::pressure::lendable_gpu_ids(&state.config)
        .into_iter()
        .map(|id| {
            let history = state.resources[&id].pressure_history.lock().unwrap();
            let export = PressureHistory {
                limit: history.limit(),
                poll_ms: pressure.poll_ms,
                samples: history.records(),
                top_processes: history.top_processes(),
                learned: history.learned(),
            };
            drop(history);
            (id, export)
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::pressure_history::Profile;

    /// A directory removed when the test ends.
    struct Scratch(std::path::PathBuf);

    impl Scratch {
        fn new() -> Self {
            let path = std::env::temp_dir().join(format!(
                "vacancyd-diagnostics-test-{}-{}",
                std::process::id(),
                std::time::SystemTime::now()
                    .duration_since(std::time::UNIX_EPOCH)
                    .unwrap()
                    .as_nanos()
            ));
            std::fs::create_dir(&path).unwrap();
            Self(path)
        }
    }

    impl Drop for Scratch {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    const FAKE_TOKEN: &str = "fake-secret-token-6f1d2c";
    const FAKE_UPSTREAM_SECRET: &str = "fake-upstream-secret-93b7";
    const FAKE_URL_SECRET: &str = "fake-url-secret-41aa";

    #[cfg(unix)]
    #[test]
    fn diagnostics_refuses_pressure_cache_in_shared_directory() {
        use std::os::unix::fs::PermissionsExt;
        let scratch = Scratch::new();
        let directory = scratch.0.join("shared-cache");
        std::fs::create_dir(&directory).unwrap();
        std::fs::set_permissions(&directory, std::fs::Permissions::from_mode(0o777)).unwrap();
        let path = directory.join("pressure.json");
        std::fs::write(&path, br#"{"schema":4,"devices":{}}"#).unwrap();
        let mut state = state_with_secrets(&scratch);
        state.config.owner.gpu_pressure.state_file = path.to_string_lossy().into_owned();
        let cache = pressure_cache(&state);
        assert_eq!(cache.state, "unreadable");
        assert!(cache.content.is_none());
    }

    /// A running daemon with an inline token, an upstream credential file
    /// and, bypassing validation, a target that carries credentials.
    fn state_with_secrets(scratch: &Scratch) -> AppState {
        let secret = scratch.0.join("upstream.secret");
        std::fs::write(&secret, FAKE_UPSTREAM_SECRET).unwrap();
        let config_path = scratch.0.join("vacancyd.toml");
        let source = format!(
            r#"
[server]
listen = "127.0.0.1:0"

[auth]
[[auth.token]]
name = "cluster"
scopes = ["read"]
value = "{FAKE_TOKEN}"

[[resource]]
id = "gpu0"
kind = "nvidia-gpu"
selector = "GPU-aaaa"

[[upstream]]
id = "llama"
listen = "127.0.0.1:0"
target = "http://owner:{FAKE_URL_SECRET}@127.0.0.1:8080/?key={FAKE_URL_SECRET}"
kind = "openai"
resource = "gpu0"

[upstream.credential]
scheme = "bearer"
value_file = "{}"
"#,
            secret.display().to_string().replace('\\', "\\\\")
        );
        std::fs::write(&config_path, &source).unwrap();
        let config: crate::config::Config = toml::from_str(&source).unwrap();
        AppState::new_with_config_path(config, &config_path).unwrap()
    }

    #[test]
    fn no_credential_appears_anywhere_in_the_export() {
        let scratch = Scratch::new();
        let state = state_with_secrets(&scratch);
        let document = serde_json::to_string(&collect(&state)).unwrap();
        for secret in [FAKE_TOKEN, FAKE_UPSTREAM_SECRET, FAKE_URL_SECRET] {
            assert!(!document.contains(secret), "{secret} leaked: {document}");
        }
        let document: serde_json::Value = serde_json::from_str(&document).unwrap();
        let settings = &document["settings"];
        assert_eq!(settings["capabilities"]["auth_tokens_configured"], 1);
        assert_eq!(
            settings["capabilities"]["upstream_credentials_configured"],
            1
        );
        assert_eq!(
            settings["upstreams"][0]["target"],
            "http://***@127.0.0.1:8080/?***"
        );
    }

    #[test]
    fn the_export_carries_status_settings_logging_cache_and_history() {
        let scratch = Scratch::new();
        let state = state_with_secrets(&scratch);
        let record = PressureRecord {
            at_unix_ms: 1_759_795_200_000,
            reading: crate::pressure_history::Reading::Ready {
                non_lender_pct: 3.25,
                profile: Profile::Attended,
                threshold_pct: 25.0,
                baseline_pct: Some(3.25),
                calibrated: true,
                engaged: false,
            },
        };
        state.resources["gpu0"]
            .pressure_history
            .lock()
            .unwrap()
            .push(record.clone());
        std::fs::write(
            scratch.0.join("vacancyd-pressure.json"),
            r#"{"schema": 2, "devices": {}}"#,
        )
        .unwrap();
        state
            .log
            .open_debug_window(std::time::Duration::from_secs(600))
            .unwrap();

        let document = serde_json::to_value(collect(&state)).unwrap();
        assert_eq!(document["schema_version"], SCHEMA_VERSION);
        assert_eq!(document["daemon"]["version"], env!("CARGO_PKG_VERSION"));
        assert_eq!(document["platform"]["os"], std::env::consts::OS);
        assert_eq!(document["status"]["schema_version"], wire::SCHEMA_VERSION);
        assert_eq!(document["logging"]["source"], "debug_window");
        assert_eq!(document["status"]["logging"]["source"], "debug_window");
        assert_eq!(
            document["settings"]["revision"],
            document["saved_settings_revision"]
        );
        assert_eq!(document["pressure_cache"]["state"], "loaded");
        // A file in the previous format is shown migrated, and says so.
        assert_eq!(
            document["pressure_cache"]["content"]["schema"],
            PressureStore::SCHEMA
        );
        assert_eq!(document["pressure_cache"]["content"]["migrated_from"], 2);
        let history = &document["pressure_history"]["gpu0"];
        assert_eq!(history["limit"], 4_320);
        assert_eq!(history["poll_ms"], 5_000);
        assert_eq!(
            history["samples"],
            serde_json::json!([serde_json::to_value(&record).unwrap()])
        );
        assert!(
            history["learned"].is_null(),
            "nothing is learned before the first sample"
        );
        assert!(document["generated_at"].as_str().unwrap().ends_with('Z'));
        assert_eq!(document["reclaims"], serde_json::json!([]));
    }

    #[test]
    fn a_missing_or_damaged_pressure_cache_is_described_not_fatal() {
        let scratch = Scratch::new();
        let state = state_with_secrets(&scratch);
        assert_eq!(pressure_cache(&state).state, "absent");
        std::fs::write(scratch.0.join("vacancyd-pressure.json"), "{not json").unwrap();
        assert_eq!(pressure_cache(&state).state, "invalid");
        std::fs::write(
            scratch.0.join("vacancyd-pressure.json"),
            vec![b' '; crate::plugin::pressure::MAX_STORE_BYTES as usize + 1],
        )
        .unwrap();
        assert_eq!(pressure_cache(&state).state, "too_large");
        // Exactly at the cap is read; whitespace alone is not a store.
        std::fs::write(
            scratch.0.join("vacancyd-pressure.json"),
            vec![b' '; crate::plugin::pressure::MAX_STORE_BYTES as usize],
        )
        .unwrap();
        assert_eq!(pressure_cache(&state).state, "invalid");
    }
}
