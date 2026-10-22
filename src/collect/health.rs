//! Diagnostic target reachability, independent of admission and presence.
use std::{sync::Arc, time::Duration};

use futures_util::future::join_all;

use crate::state::AppState;

pub const FRESH_FOR: Duration = Duration::from_secs(30);
const POLL_INTERVAL: Duration = Duration::from_secs(10);
const TIMEOUT: Duration = Duration::from_secs(2);

fn client() -> reqwest::Client {
    reqwest::Client::builder()
        .no_proxy()
        .redirect(reqwest::redirect::Policy::none())
        .timeout(TIMEOUT)
        .build()
        .expect("fixed diagnostic HTTP client")
}

async fn check(http: &reqwest::Client, target: &str, path: &str) -> Result<u16, &'static str> {
    // Headers alone establish HTTP reachability. Do not buffer an untrusted
    // response body, follow a redirect, forward credentials, or run inference.
    let response = http
        .get(format!("{}{path}", target.trim_end_matches('/')))
        .send()
        .await;
    response
        .map(|r| r.status().as_u16())
        .map_err(|_| "Cannot reach the upstream target (two-second limit).")
}

pub async fn run(state: Arc<AppState>) {
    let http = client();
    loop {
        join_all(state.config.upstreams.iter().map(|upstream| async {
            let runtime = &state.upstreams[&upstream.id];
            let Some(path) = runtime.adapter.health_path() else {
                return;
            };
            let result = check(&http, upstream.target.as_str(), path).await;
            let mut health = runtime.health.write().unwrap();
            match result {
                Ok(status) => health.record_success(status),
                Err(error) => health.record_failure(error),
            }
        }))
        .await;
        tokio::time::sleep(POLL_INTERVAL).await;
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::{
        http::{HeaderMap, StatusCode},
        routing::get,
        Router,
    };

    #[tokio::test]
    async fn a_target_that_never_sends_headers_is_bounded() {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let target = format!("http://{}", listener.local_addr().unwrap());
        let task = tokio::spawn(async move {
            let (_socket, _) = listener.accept().await.unwrap();
            std::future::pending::<()>().await;
        });
        let started = std::time::Instant::now();
        assert!(
            tokio::time::timeout(Duration::from_secs(4), check(&client(), &target, "/"))
                .await
                .unwrap()
                .is_err()
        );
        assert!(started.elapsed() >= TIMEOUT);
        task.abort();
    }

    #[tokio::test]
    async fn diagnostics_distinguish_status_and_do_not_follow_redirects_or_send_auth() {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let target = format!("http://{}", listener.local_addr().unwrap());
        let task = tokio::spawn(async move {
            let app = Router::new()
                .route(
                    "/ok",
                    get(|headers: HeaderMap| async move {
                        assert!(!headers.contains_key("authorization"));
                        StatusCode::OK
                    }),
                )
                .route("/auth", get(|| async { StatusCode::UNAUTHORIZED }))
                .route(
                    "/redirect",
                    get(|| async { axum::response::Redirect::temporary("/ok") }),
                );
            axum::serve(listener, app).await.unwrap();
        });
        let http = client();
        assert_eq!(check(&http, &target, "/ok").await.unwrap(), 200);
        assert_eq!(check(&http, &target, "/auth").await.unwrap(), 401);
        assert_eq!(check(&http, &target, "/redirect").await.unwrap(), 307);
        task.abort();
        let _ = task.await;
        assert!(check(&http, &target, "/ok").await.is_err());
    }
}
