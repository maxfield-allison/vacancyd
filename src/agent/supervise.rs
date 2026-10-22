//! Keep a local channel (the agent pipe, the admin pipe or the admin
//! socket) running after a failure.
//!
//! Without the agent channel, idle lending stops for every session, which is
//! safe but lasts until someone restarts the service; without an admin
//! channel, only reading and pausing remain for local users. A channel can
//! fail to start for reasons that pass: another process briefly holding the
//! pipe name, the instance limit during a burst of reconnects, a runtime
//! directory not yet created, resource exhaustion. Restarting with a bounded
//! backoff recovers from all of them without spinning.

use std::future::Future;
use std::time::Duration;

use tokio::time::Instant;
use tracing::{debug, warn};

/// First delay after a failure; doubles on each consecutive failure.
pub const RESTART_MIN: Duration = Duration::from_secs(1);
/// Longest delay between restarts.
pub const RESTART_MAX: Duration = Duration::from_secs(60);
/// A run lasting this long was healthy, so the next failure starts over at
/// [`RESTART_MIN`].
pub const HEALTHY_RUN: Duration = Duration::from_secs(60);

/// Marks a channel as accepting connections for as long as it lives, so a
/// run that ends by any path, including `?`, clears the mark.
#[cfg_attr(not(any(windows, target_os = "linux")), allow(dead_code))]
pub struct Listening<'a>(&'a std::sync::atomic::AtomicBool);

#[cfg_attr(not(any(windows, target_os = "linux")), allow(dead_code))]
impl<'a> Listening<'a> {
    pub fn mark(flag: &'a std::sync::atomic::AtomicBool) -> Self {
        flag.store(true, std::sync::atomic::Ordering::Relaxed);
        Self(flag)
    }
}

impl Drop for Listening<'_> {
    fn drop(&mut self) {
        self.0.store(false, std::sync::atomic::Ordering::Relaxed);
    }
}

/// Run `channel` forever, restarting it with exponential backoff whenever it
/// returns. `name` identifies it in the log. Cancel the returned future to
/// stop.
///
/// A failure is logged as a warning when it first appears; the same failure
/// repeated on every retry (a socket directory the service cannot create, for
/// example) is logged at debug level until it changes or a run succeeds, so a
/// lasting condition does not fill the log once a minute.
pub async fn supervise<F, Fut>(name: &'static str, mut channel: F)
where
    F: FnMut() -> Fut,
    Fut: Future<Output = anyhow::Result<()>>,
{
    let mut delay = RESTART_MIN;
    let mut last_failure: Option<String> = None;
    loop {
        let started = Instant::now();
        let outcome = channel().await;
        if started.elapsed() >= HEALTHY_RUN {
            delay = RESTART_MIN;
            last_failure = None;
        }
        let retry_in_ms = delay.as_millis() as u64;
        match outcome {
            Ok(()) => {
                last_failure = None;
                warn!(
                    channel = name,
                    retry_in_ms, "local channel ended; restarting"
                )
            }
            Err(error) => {
                let failure = format!("{error:#}");
                if last_failure.as_deref() == Some(failure.as_str()) {
                    debug!(channel = name, error = %failure, retry_in_ms, "local channel still failing; retrying")
                } else {
                    warn!(channel = name, error = %failure, retry_in_ms, "local channel stopped; restarting");
                    last_failure = Some(failure);
                }
            }
        }
        tokio::time::sleep(delay).await;
        delay = (delay * 2).min(RESTART_MAX);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::{Arc, Mutex};

    fn gaps(starts: &[Instant]) -> Vec<u64> {
        starts
            .windows(2)
            .map(|pair| (pair[1] - pair[0]).as_secs())
            .collect()
    }

    #[tokio::test(start_paused = true)]
    async fn a_failing_channel_is_restarted_with_bounded_backoff() {
        let starts = Arc::new(Mutex::new(Vec::new()));
        let recorded = starts.clone();
        let supervisor = tokio::spawn(supervise("test channel", move || {
            recorded.lock().unwrap().push(Instant::now());
            async { Err(anyhow::anyhow!("could not create pipe instance")) }
        }));
        tokio::time::sleep(Duration::from_secs(300)).await;
        supervisor.abort();
        let starts = starts.lock().unwrap();
        assert_eq!(gaps(&starts)[..9], [1, 2, 4, 8, 16, 32, 60, 60, 60]);
    }

    #[tokio::test(start_paused = true)]
    async fn a_healthy_run_resets_the_backoff() {
        let starts = Arc::new(Mutex::new(Vec::new()));
        let recorded = starts.clone();
        let supervisor = tokio::spawn(supervise("test channel", move || {
            let mut starts = recorded.lock().unwrap();
            starts.push(Instant::now());
            // Fail fast three times, then run healthily once, then fail fast.
            let healthy = starts.len() == 4;
            async move {
                if healthy {
                    tokio::time::sleep(HEALTHY_RUN).await;
                }
                Err(anyhow::anyhow!("pipe server stopped"))
            }
        }));
        tokio::time::sleep(Duration::from_secs(120)).await;
        supervisor.abort();
        let starts = starts.lock().unwrap();
        // 1, 2, 4 between the fast failures; the fourth run lasts 60 s and is
        // followed by the minimum delay again.
        assert_eq!(gaps(&starts)[..4], [1, 2, 4, 61]);
    }

    #[test]
    fn a_lasting_failure_warns_once_and_a_new_one_warns_again() {
        // The current-thread runtime keeps the supervisor on this thread,
        // which is the one `test_log` captures (see its module notes).
        let log = crate::test_log::capture_at(tracing::level_filters::LevelFilter::DEBUG);
        {
            let runtime = tokio::runtime::Builder::new_current_thread()
                .enable_time()
                .start_paused(true)
                .build()
                .unwrap();
            runtime.block_on(async {
                let runs = Arc::new(Mutex::new(0u32));
                let counted = runs.clone();
                let supervisor = tokio::spawn(supervise("test channel", move || {
                    let mut runs = counted.lock().unwrap();
                    *runs += 1;
                    let failure = if *runs <= 4 {
                        "directory is read-only"
                    } else {
                        "address in use"
                    };
                    async move { Err(anyhow::anyhow!(failure)) }
                }));
                tokio::time::sleep(Duration::from_secs(200)).await;
                supervisor.abort();
            });
        }
        let text = log.contents();
        let warnings: Vec<&str> = text
            .lines()
            .filter(|line| line.contains(" WARN "))
            .collect();
        assert_eq!(warnings.len(), 2, "{text}");
        assert!(warnings[0].contains("directory is read-only"), "{text}");
        assert!(warnings[1].contains("address in use"), "{text}");
        assert!(text.contains("local channel still failing"), "{text}");
    }
}
