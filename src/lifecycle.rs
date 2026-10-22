//! Upstream observations are evidence with an expiry, never inferred from a
//! successful TCP connection or free memory alone.
use crate::state::{AppState, ObservationQuality};
use serde::{Deserialize, Serialize};
use std::sync::Arc;
use std::time::{Duration, Instant};

pub const FRESH_FOR: Duration = Duration::from_secs(10);

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct Models {
    pub models: Vec<vacancyd::status::Model>,
    pub busy: bool,
}

pub async fn run(state: Arc<AppState>) {
    state.admission.lock().unwrap().sync_active_metrics(&state);
    tokio::join!(model_loop(state.clone()), listener_loop(state));
}

async fn model_loop(state: Arc<AppState>) {
    futures_util::future::join_all(state.config.upstreams.iter().map(|upstream| {
        let state = state.clone();
        async move {
            let runtime = &state.upstreams[&upstream.id];
            loop {
                if let Some(probe) = runtime
                    .adapter
                    .observe_models(&runtime.observer_http, upstream.target.as_str())
                {
                    let started = Instant::now();
                    let result = tokio::time::timeout(Duration::from_secs(4), probe).await;
                    let mut observation = runtime.models.write().unwrap();
                    match result {
                        Ok(Ok(models)) => observation.record_success_at(models, started),
                        _ => observation.record_failure("model observation unavailable"),
                    }
                }
                sample_footprints(&state, &upstream.id);
                tokio::time::sleep(Duration::from_secs(2)).await;
            }
        }
    }))
    .await;
}

/// Each upstream's own target listener. A target that is not on this machine
/// is reported as not observable rather than matched to whatever local socket
/// shares its port.
async fn listener_loop(state: Arc<AppState>) {
    let targets = state
        .config
        .upstreams
        .iter()
        .map(|upstream| upstream.target.as_str().to_owned())
        .collect::<Vec<_>>();
    loop {
        let probe_targets = targets.clone();
        let results = tokio::task::spawn_blocking(move || {
            crate::probes::listeners::target_listener_states(&probe_targets)
        })
        .await
        .expect("listener probe worker panicked");
        let sampled_at = Instant::now();
        for (upstream, result) in state.config.upstreams.iter().zip(results) {
            let mut observation = state.upstreams[&upstream.id].listener.write().unwrap();
            match result {
                Ok(listener) => observation.record_success_at(listener, sampled_at),
                Err(error) => observation.record_failure(error),
            }
        }
        tokio::time::sleep(Duration::from_secs(5)).await;
    }
}

pub fn sample_footprints(state: &AppState, upstream: &str) {
    // Copy the observation and its time out before taking the ledger lock;
    // admission takes the ledger first, then the observation.
    let (observation, sampled_at) = {
        let models = state.upstreams[upstream].models.read().unwrap();
        (models.view(FRESH_FOR), models.valid_since())
    };
    if observation.quality != ObservationQuality::Ready {
        return;
    }
    if let (Some(models), Some(sampled_at)) = (observation.value, sampled_at) {
        let mut ledger = state.admission.lock().unwrap();
        ledger.observe_models(state, upstream, &models.models);
        ledger.learn_contexts(upstream, &models.models, sampled_at);
    }
}
