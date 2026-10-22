//! Native Windows Service Control Manager host.
//!
//! The public daemon logic remains in [`crate::run_until`]. This module is a
//! deliberately thin adapter: translate SCM lifecycle controls into the same
//! shutdown future used by an interactive process, and report truthful service
//! state back to Windows.

use std::path::PathBuf;
use std::sync::{Arc, Mutex, OnceLock};
use std::time::Duration;

use anyhow::{anyhow, Context, Result};
use tokio::sync::oneshot;
use windows_service::define_windows_service;
use windows_service::service::{
    ServiceControl, ServiceControlAccept, ServiceExitCode, ServiceState, ServiceStatus, ServiceType,
};
use windows_service::service_control_handler::{self, ServiceControlHandlerResult};
use windows_service::service_dispatcher;

const SERVICE_NAME: &str = "vacancyd";
const START_WAIT: Duration = Duration::from_secs(15);
/// Covers the borrowed-request drain (at most 10 s) plus RUNTIME_SHUTDOWN.
const STOP_WAIT: Duration = Duration::from_secs(25);
/// After the daemon returns, how long to wait for blocking probe work before
/// reporting Stopped. Every probe subprocess has a deadline of at most 5 s,
/// and the in-process GPU counter sample returns within 8 s or is
/// abandoned, so this lets any sample in progress end.
const RUNTIME_SHUTDOWN: Duration = Duration::from_secs(9);

static CONFIG_PATH: OnceLock<PathBuf> = OnceLock::new();

define_windows_service!(ffi_service_main, service_main);

/// Enter the SCM dispatcher. This blocks until the service stops.
pub fn dispatch(config_path: PathBuf) -> Result<()> {
    CONFIG_PATH
        .set(config_path)
        .map_err(|_| anyhow!("Windows service dispatcher was initialized twice"))?;
    service_dispatcher::start(SERVICE_NAME, ffi_service_main)
        .context("starting Windows service dispatcher")
}

fn service_main(_arguments: Vec<std::ffi::OsString>) {
    if let Err(error) = run_service() {
        tracing::error!(error = %error, "Windows service stopped with an error");
    }
}

fn run_service() -> Result<()> {
    let config_path = CONFIG_PATH
        .get()
        .cloned()
        .ok_or_else(|| anyhow!("Windows service config path is unavailable"))?;
    let (shutdown_tx, shutdown_rx) = oneshot::channel();
    let shutdown_tx = Arc::new(Mutex::new(Some(shutdown_tx)));
    let handler_tx = shutdown_tx.clone();
    let event_handler = move |control| handle_control(control, &handler_tx);
    let status_handle = service_control_handler::register(SERVICE_NAME, event_handler)
        .context("registering Windows service control handler")?;

    status_handle
        .set_service_status(service_status(
            ServiceState::StartPending,
            ServiceControlAccept::empty(),
            ServiceExitCode::NO_ERROR,
            START_WAIT,
        ))
        .context("reporting Windows service start-pending state")?;

    let runtime = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
        .context("building Windows service async runtime")?;
    let running_handle = status_handle;
    let stopping_handle = status_handle;
    let result = runtime.block_on(crate::run_until(
        config_path,
        async move {
            let _ = shutdown_rx.await;
            if let Err(error) = stopping_handle.set_service_status(service_status(
                ServiceState::StopPending,
                ServiceControlAccept::empty(),
                ServiceExitCode::NO_ERROR,
                STOP_WAIT,
            )) {
                tracing::error!(error = %error, "could not report service stop-pending state");
            }
        },
        move || {
            running_handle
                .set_service_status(service_status(
                    ServiceState::Running,
                    ServiceControlAccept::STOP | ServiceControlAccept::PRESHUTDOWN,
                    ServiceExitCode::NO_ERROR,
                    Duration::ZERO,
                ))
                .context("reporting Windows service running state")
        },
    ));

    // Report Stopped only once the runtime's tasks and blocking probes have
    // ended, so SCM never sees a stopped service whose process still runs.
    runtime.shutdown_timeout(RUNTIME_SHUTDOWN);

    let exit_code = if result.is_ok() {
        ServiceExitCode::NO_ERROR
    } else {
        ServiceExitCode::ServiceSpecific(1)
    };
    status_handle
        .set_service_status(service_status(
            ServiceState::Stopped,
            ServiceControlAccept::empty(),
            exit_code,
            Duration::ZERO,
        ))
        .context("reporting Windows service stopped state")?;
    result
}

fn handle_control(
    control: ServiceControl,
    shutdown_tx: &Mutex<Option<oneshot::Sender<()>>>,
) -> ServiceControlHandlerResult {
    match control {
        ServiceControl::Interrogate => ServiceControlHandlerResult::NoError,
        ServiceControl::Stop | ServiceControl::Preshutdown | ServiceControl::Shutdown => {
            if let Ok(mut sender) = shutdown_tx.lock() {
                if let Some(sender) = sender.take() {
                    let _ = sender.send(());
                }
            }
            ServiceControlHandlerResult::NoError
        }
        _ => ServiceControlHandlerResult::NotImplemented,
    }
}

fn service_status(
    current_state: ServiceState,
    controls_accepted: ServiceControlAccept,
    exit_code: ServiceExitCode,
    wait_hint: Duration,
) -> ServiceStatus {
    ServiceStatus {
        service_type: ServiceType::OWN_PROCESS,
        current_state,
        controls_accepted,
        exit_code,
        checkpoint: 0,
        wait_hint,
        process_id: None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn stop_controls_signal_shutdown_once_and_remain_idempotent() {
        let (sender, receiver) = oneshot::channel();
        let sender = Mutex::new(Some(sender));

        assert!(matches!(
            handle_control(ServiceControl::Stop, &sender),
            ServiceControlHandlerResult::NoError
        ));
        receiver.await.expect("stop should signal the daemon");
        assert!(matches!(
            handle_control(ServiceControl::Preshutdown, &sender),
            ServiceControlHandlerResult::NoError
        ));
    }

    #[test]
    fn interrogation_is_accepted_and_unimplemented_controls_are_rejected() {
        let (sender, _receiver) = oneshot::channel();
        let sender = Mutex::new(Some(sender));

        assert!(matches!(
            handle_control(ServiceControl::Interrogate, &sender),
            ServiceControlHandlerResult::NoError
        ));
        assert!(matches!(
            handle_control(ServiceControl::Pause, &sender),
            ServiceControlHandlerResult::NotImplemented
        ));
    }
}
