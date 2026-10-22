use super::{comfyui::ComfyUi, ollama::Ollama};
use crate::plugin::adapter::Adapter;
use axum::{
    routing::{get, post},
    Json, Router,
};
use http::StatusCode;
use serde_json::{json, Value};
use std::sync::{Arc, Mutex};

struct Server {
    base: String,
    requests: Arc<Mutex<Vec<Value>>>,
    task: tokio::task::JoinHandle<()>,
}

impl Drop for Server {
    fn drop(&mut self) {
        self.task.abort();
    }
}

async fn server(
    list_status: StatusCode,
    list: Value,
    unload_status: StatusCode,
    reply: Value,
) -> Server {
    let requests = Arc::new(Mutex::new(Vec::new()));
    let recorded = requests.clone();
    let app = Router::new()
        .route(
            "/api/ps",
            get(move || async move { (list_status, Json(list)) }),
        )
        .route(
            "/api/generate",
            post(move |Json(body): Json<Value>| async move {
                recorded.lock().unwrap().push(body);
                (unload_status, Json(reply))
            }),
        )
        .route("/free", post(move || async move { unload_status }));
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let base = format!("http://{}", listener.local_addr().unwrap());
    let task = tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
    Server {
        base,
        requests,
        task,
    }
}

fn client() -> reqwest::Client {
    reqwest::Client::builder()
        .redirect(reqwest::redirect::Policy::none())
        .build()
        .unwrap()
}

fn residents() -> Value {
    json!({"models": [{"name": "test:latest"}]})
}

fn acknowledgement() -> Value {
    json!({"model": "test:latest", "done": true, "done_reason": "unload"})
}

#[tokio::test]
async fn ollama_unload_rejects_failed_or_ambiguous_inventory() {
    for (status, list) in [
        (StatusCode::SERVICE_UNAVAILABLE, residents()),
        (StatusCode::TEMPORARY_REDIRECT, residents()),
        (StatusCode::OK, json!({"error": "unavailable"})),
        (StatusCode::OK, json!({"models": [{}]})),
        (StatusCode::OK, json!({"models": [{"name": ""}]})),
        (
            StatusCode::OK,
            json!({"models": [{"name": "test:latest"}, {}]}),
        ),
        (
            StatusCode::OK,
            json!({"models": [], "padding": "x".repeat(1024 * 1024)}),
        ),
    ] {
        let s = server(status, list, StatusCode::OK, acknowledgement()).await;
        assert!(
            Ollama::default()
                .unload_all(&client(), &s.base)
                .await
                .is_err(),
            "inventory {status} must fail"
        );
        assert!(s.requests.lock().unwrap().is_empty());
    }
}

#[tokio::test]
async fn ollama_unload_requires_a_successful_terminal_acknowledgement() {
    for (status, reply) in [
        (StatusCode::INTERNAL_SERVER_ERROR, acknowledgement()),
        (StatusCode::UNAUTHORIZED, acknowledgement()),
        (StatusCode::TEMPORARY_REDIRECT, acknowledgement()),
        (StatusCode::OK, json!({"error": "still busy"})),
        (
            StatusCode::OK,
            json!({"done": false, "done_reason": "unload"}),
        ),
        (StatusCode::OK, json!({"done": true, "done_reason": "load"})),
        (
            StatusCode::OK,
            json!({"done": true, "done_reason": "unload", "model": "other:latest"}),
        ),
    ] {
        let s = server(StatusCode::OK, residents(), status, reply).await;
        assert!(
            Ollama::default()
                .unload_all(&client(), &s.base)
                .await
                .is_err(),
            "unload {status} must fail"
        );
    }
}

#[tokio::test]
async fn ollama_unload_accepts_empty_inventory_and_acknowledged_unload() {
    let empty = server(
        StatusCode::OK,
        json!({"models": []}),
        StatusCode::OK,
        acknowledgement(),
    )
    .await;
    assert_eq!(
        Ollama::default()
            .unload_all(&client(), &empty.base)
            .await
            .unwrap(),
        0
    );
    let s = server(
        StatusCode::OK,
        residents(),
        StatusCode::OK,
        acknowledgement(),
    )
    .await;
    assert_eq!(
        Ollama::default()
            .unload_all(&client(), &s.base)
            .await
            .unwrap(),
        1
    );
    assert_eq!(
        s.requests.lock().unwrap()[0],
        json!({"model": "test:latest", "keep_alive": 0, "stream": false})
    );
}

#[tokio::test]
async fn comfyui_unload_reports_http_failure_and_accepts_empty_success() {
    for status in [
        StatusCode::UNAUTHORIZED,
        StatusCode::INTERNAL_SERVER_ERROR,
        StatusCode::TEMPORARY_REDIRECT,
    ] {
        let s = server(StatusCode::OK, residents(), status, json!({})).await;
        assert!(
            ComfyUi.unload_all(&client(), &s.base).await.is_err(),
            "unload {status} must fail"
        );
    }
    let s = server(StatusCode::OK, residents(), StatusCode::OK, json!({})).await;
    assert_eq!(ComfyUi.unload_all(&client(), &s.base).await.unwrap(), 1);
}

#[tokio::test]
async fn failed_unload_does_not_increment_the_yield_counter() {
    let s = server(
        StatusCode::OK,
        residents(),
        StatusCode::INTERNAL_SERVER_ERROR,
        acknowledgement(),
    )
    .await;
    let config = toml::from_str(&format!(
        r#"
        [server]
        listen = "127.0.0.1:0"
        [[resource]]
        id = "mem"
        kind = "system-memory"
        [[upstream]]
        id = "ollama"
        listen = "127.0.0.1:0"
        target = "{}"
        kind = "ollama"
        resource = "mem"
        yield_on_veto = true
        on_owner_return = "cancel"
    "#,
        s.base
    ))
    .unwrap();
    let state = Arc::new(crate::state::AppState::new(config).unwrap());
    crate::yield_control::on_veto_engaged(state.clone()).await;
    assert_eq!(
        state
            .metrics
            .yields_total
            .with_label_values(&["ollama"])
            .get(),
        0
    );
}
