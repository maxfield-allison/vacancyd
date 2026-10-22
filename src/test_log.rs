//! Log capture for tests, through one process-wide subscriber.
//!
//! The obvious harness, a thread-scoped subscriber from
//! `tracing::subscriber::set_default`, is unreliable in a parallel test
//! binary. tracing caches each callsite's interest globally,
//! computed when the callsite is first reached. While a single scoped
//! subscriber is the only one registered, tracing-core computes that interest
//! from the default subscriber *of the thread that reaches the callsite
//! first*. If another test thread, which has no subscriber, reaches a shared
//! `warn!` first, the callsite is cached as "never" and the capturing test's
//! own event is discarded before its subscriber is consulted. Re-deriving the
//! cache after installing the subscriber does not help, because the other
//! thread can register the callsite afterwards. On Windows a refused loopback
//! connect takes about two seconds, which held that window open long enough
//! for the native CI job to lose the event every time.
//!
//! A global subscriber is every thread's default, so the interest a callsite
//! caches is the same whichever thread registers it. Isolation between tests
//! moves from the dispatcher to the writer: [`capture`] points a thread-local
//! sink at a buffer, a per-event (never cached) filter formats only events on
//! a thread that has one, and events on every other thread are dropped.

use std::cell::RefCell;
use std::io::Write;
use std::sync::{Arc, Mutex, Once};

use tracing::level_filters::LevelFilter;
use tracing_subscriber::layer::SubscriberExt;
use tracing_subscriber::Layer;

/// The capturing thread's buffer and the most verbose level it records.
type Sink = (Arc<Mutex<Vec<u8>>>, LevelFilter);

thread_local! {
    static SINK: RefCell<Option<Sink>> = const { RefCell::new(None) };
}

static INSTALL: Once = Once::new();

/// Log lines written on the calling thread while this guard is alive.
pub struct Capture {
    buffer: Arc<Mutex<Vec<u8>>>,
}

impl Capture {
    pub fn contents(&self) -> String {
        String::from_utf8_lossy(&self.buffer.lock().unwrap()).into_owned()
    }
}

impl Drop for Capture {
    fn drop(&mut self) {
        SINK.with(|sink| sink.borrow_mut().take());
    }
}

/// Capture INFO and above logged on this thread until the guard drops.
///
/// Only the calling thread is captured. Code under test that logs from a
/// spawned thread or a multi-threaded runtime's worker is not, so use a
/// current-thread runtime (the `#[tokio::test]` default).
pub fn capture() -> Capture {
    capture_at(LevelFilter::INFO)
}

/// [`capture`], recording events down to `level`.
pub fn capture_at(level: LevelFilter) -> Capture {
    INSTALL.call_once(|| {
        let layer = tracing_subscriber::fmt::layer()
            .with_ansi(false)
            .with_writer(|| ThreadSink)
            // Dynamic, so the answer is taken per event on the emitting
            // thread. A plain `filter_fn` is cached per callsite like any
            // interest, which would reintroduce the first-thread-wins bug.
            .with_filter(
                tracing_subscriber::filter::dynamic_filter_fn(|metadata, _| {
                    SINK.with(|sink| {
                        sink.borrow()
                            .as_ref()
                            .is_some_and(|(_, level)| metadata.level() <= level)
                    })
                })
                .with_max_level_hint(LevelFilter::TRACE),
            );
        tracing::subscriber::set_global_default(tracing_subscriber::registry().with(layer))
            .expect("no other global tracing subscriber is installed in the test binary");
    });
    let buffer = Arc::new(Mutex::new(Vec::new()));
    SINK.with(|sink| {
        let previous = sink.borrow_mut().replace((buffer.clone(), level));
        assert!(
            previous.is_none(),
            "log capture is already active on this thread"
        );
    });
    Capture { buffer }
}

struct ThreadSink;

impl Write for ThreadSink {
    fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
        SINK.with(|sink| {
            if let Some((buffer, _)) = sink.borrow().as_ref() {
                buffer.lock().unwrap().extend_from_slice(bytes);
            }
        });
        Ok(bytes.len())
    }

    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn captures_this_thread_only() {
        let log = capture();
        tracing::warn!("captured-on-this-thread");
        std::thread::spawn(|| tracing::warn!("logged-on-another-thread"))
            .join()
            .unwrap();
        let text = log.contents();
        assert!(text.contains("captured-on-this-thread"), "{text}");
        assert!(!text.contains("logged-on-another-thread"), "{text}");
    }

    #[test]
    fn a_callsite_first_reached_on_another_thread_is_still_captured() {
        // The failing sequence: another thread, with no capture of its
        // own, is the first to reach a callsite while this thread is
        // capturing. The event this thread then logs through the same
        // callsite must still arrive.
        fn shared_callsite(marker: &str) {
            tracing::warn!(marker, "shared-callsite");
        }
        let log = capture();
        std::thread::spawn(|| shared_callsite("other-thread"))
            .join()
            .unwrap();
        shared_callsite("this-thread");
        let text = log.contents();
        assert!(text.contains("this-thread"), "{text}");
        assert!(!text.contains("other-thread"), "{text}");
    }

    #[test]
    fn the_capture_level_is_per_thread() {
        let log = capture();
        tracing::debug!("debug-not-requested");
        tracing::info!("info-requested");
        drop(log);
        let verbose = capture_at(LevelFilter::DEBUG);
        tracing::debug!("debug-requested");
        tracing::trace!("trace-not-requested");
        let text = verbose.contents();
        assert!(text.contains("debug-requested"), "{text}");
        assert!(!text.contains("trace-not-requested"), "{text}");
        assert!(!text.contains("debug-not-requested"), "{text}");
    }

    #[test]
    fn nothing_is_captured_after_the_guard_drops() {
        let log = capture();
        let buffer = log.buffer.clone();
        drop(log);
        tracing::warn!("after-the-guard");
        assert!(buffer.lock().unwrap().is_empty());
        // A new capture on the same thread starts empty.
        let again = capture();
        tracing::info!("second-capture");
        assert!(again.contents().contains("second-capture"));
    }
}
