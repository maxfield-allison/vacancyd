//! The daemon's log filter, and the temporary debug window an administrator
//! can open without restarting.
//!
//! What is written is decided in this order:
//!
//! 1. An open debug window logs vacancyd's own events at `debug` until it
//!    closes on its own, is ended early, or the daemon restarts.
//! 2. `RUST_LOG`, when it is set and parses, replaces `[log] level`.
//! 3. `[log] level` from the configuration, `info` by default.
//!
//! The filter sits behind a reload handle, so a window takes effect at once
//! on the console logger and on the Windows service log alike. A window is
//! held in memory only: after a restart the configured level applies again.
//! It closes at its end on the monotonic clock or the wall clock, whichever
//! comes first, so a machine that sleeps through the end does not log debug
//! for longer than asked.

use std::sync::{Arc, Mutex, MutexGuard, OnceLock};
use std::time::{Duration, SystemTime};

use tokio::time::Instant;
use tracing::level_filters::LevelFilter;
use tracing_subscriber::layer::{Layered, SubscriberExt};
use tracing_subscriber::util::SubscriberInitExt;
use tracing_subscriber::{reload, EnvFilter, Layer, Registry};
use vacancyd::status::Logging;

use crate::config::LogLevel;

/// How long a debug window stays open when the request names no duration.
pub const DEBUG_WINDOW_DEFAULT: Duration = Duration::from_secs(30 * 60);
/// The shortest window a request may ask for.
pub const DEBUG_WINDOW_MIN: Duration = Duration::from_secs(60);
/// The longest window a request may ask for. Debug output is roughly one
/// line per resource per sample, which the bounded Windows service log
/// holds for a few hours; a longer window would rotate away its own start.
pub const DEBUG_WINDOW_MAX: Duration = Duration::from_secs(4 * 60 * 60);

/// The Windows service log's whole budget: the live file and its archives
/// (see `service_log`, whose tests check this value).
pub const SERVICE_LOG_TOTAL_BYTES: u64 = 4 * 2 * 1024 * 1024;
/// A debug window may fill at most this much of the service log with
/// per-sample decision records, so it cannot rotate away the lines that led
/// up to it.
const DEBUG_LOG_BUDGET_BYTES: u64 = SERVICE_LOG_TOTAL_BYTES / 2;
/// One decision record in the log. A measured line is about 270 bytes; the
/// margin covers long resource ids and vacancyd's other debug lines.
const DECISION_RECORD_BYTES: u64 = 384;
/// How often a waiting supervisor rechecks the wall clock, which can pass a
/// window's end while the machine sleeps.
const WALL_CLOCK_CHECK: Duration = Duration::from_secs(60);

/// The target prefix of every event vacancyd itself emits.
const CRATE_TARGET: &str = "vacancyd";
/// Libraries stay here unless `RUST_LOG` says otherwise, so `debug` shows
/// vacancyd's reasoning rather than HTTP and runtime internals.
const LIBRARY_LEVEL: &str = "info";

type FilterHandle = reload::Handle<EnvFilter, Registry>;

/// A duration outside [`DEBUG_WINDOW_MIN`]..=[`DEBUG_WINDOW_MAX`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct InvalidDuration;

/// The longest debug window whose decision records fit the log budget: one
/// record per tracked GPU per pressure sample, every `poll_ms`. At least
/// [`DEBUG_WINDOW_MIN`], never more than [`DEBUG_WINDOW_MAX`].
pub fn debug_window_limit(poll_ms: u64, tracked_gpus: usize) -> Duration {
    if tracked_gpus == 0 {
        return DEBUG_WINDOW_MAX;
    }
    let records_per_minute =
        (tracked_gpus as u64).saturating_mul(60_000u64.div_ceil(poll_ms.max(1_000)));
    let minutes = DEBUG_LOG_BUDGET_BYTES / DECISION_RECORD_BYTES.saturating_mul(records_per_minute);
    let ceiling = DEBUG_WINDOW_MAX.as_secs() / 60;
    Duration::from_secs(minutes.clamp(1, ceiling) * 60)
}

/// A window that opened, and how long it was given.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Opened {
    pub logging: Logging,
    pub granted: Duration,
    /// The request asked for more than the log budget allows.
    pub limited: bool,
}

/// How a `RUST_LOG` filter treats vacancyd's own events.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct EnvironmentReach {
    /// The level its most specific directive covering the `vacancyd`
    /// target gives that target, `OFF` when none does.
    crate_level: LevelFilter,
    /// A directive names a narrower vacancyd target (`vacancyd::pressure`),
    /// a span or a field. A `vacancyd=debug` directive cannot override it.
    narrower: bool,
}

/// Read what `directives` (already accepted by `EnvFilter`) do to
/// vacancyd's events. A directive's target applies to every target it is a
/// prefix of, and the longest applicable target wins.
fn environment_reach(directives: &str) -> EnvironmentReach {
    let mut best: Option<(usize, LevelFilter)> = None;
    let mut narrower = false;
    for directive in directives
        .split(',')
        .map(str::trim)
        .filter(|d| !d.is_empty())
    {
        let (spec, level) = match directive.rsplit_once('=') {
            Some((spec, level)) if !level.contains(['}', ']']) => (spec, level.parse().ok()),
            _ => (directive, None),
        };
        if spec.contains('[') {
            narrower = true;
            continue;
        }
        let (target, level) = match level {
            Some(level) => (spec, level),
            None if spec == directive => match directive.parse::<LevelFilter>() {
                Ok(level) => ("", level),
                // A bare target enables everything under it.
                Err(_) => (directive, LevelFilter::TRACE),
            },
            None => continue,
        };
        if target
            .strip_prefix(CRATE_TARGET)
            .is_some_and(|rest| rest.starts_with("::"))
        {
            narrower = true;
        } else if CRATE_TARGET.starts_with(target)
            && best.is_none_or(|(length, _)| target.len() >= length)
        {
            best = Some((target.len(), level));
        }
    }
    EnvironmentReach {
        crate_level: best.map_or(LevelFilter::OFF, |(_, level)| level),
        narrower,
    }
}

pub struct LogControl {
    /// `None` when no subscriber was installed, as in tests: the level is
    /// still tracked and reported, there is just nothing to reload.
    handle: Option<FilterHandle>,
    /// The `RUST_LOG` directives, when they are in force.
    environment: Option<String>,
    inner: Mutex<Inner>,
    /// Wakes [`LogControl::supervise`] when a window opens or closes.
    changed: tokio::sync::Notify,
}

struct Inner {
    configured: LogLevel,
    window: Option<DebugWindow>,
    /// The longest window the log budget allows; see [`debug_window_limit`].
    window_limit: Duration,
}

#[derive(Debug, Clone, Copy)]
struct DebugWindow {
    closes_at: Instant,
    /// The same moment on the wall clock. The window also closes when this
    /// passes, which the monotonic clock may not see across a sleep.
    closes_at_wall: SystemTime,
}

static INSTALLED: OnceLock<Arc<LogControl>> = OnceLock::new();

/// Install the process logger writing to standard error, for commands
/// whose standard output is their result.
pub fn install_stderr() {
    use std::io::IsTerminal;
    install(
        tracing_subscriber::fmt::layer()
            .with_ansi(std::io::stderr().is_terminal())
            .with_writer(std::io::stderr),
    );
}

/// Install the process logger writing to standard output, the destination
/// foreground daemon runs and systemd journal capture have always used.
pub fn install_stdout() {
    use std::io::IsTerminal;
    install(
        tracing_subscriber::fmt::layer()
            // Colour only for a person at a terminal: under systemd the
            // output goes to the journal, where escape codes show up as raw
            // bytes.
            .with_ansi(std::io::stdout().is_terminal()),
    );
}

/// Install the process logger writing to the bounded Windows service log.
#[cfg(windows)]
pub fn install_service_log(log: crate::service_log::ServiceLog) {
    install(
        tracing_subscriber::fmt::layer()
            .with_ansi(false)
            .with_writer(std::sync::Mutex::new(log)),
    );
}

fn install<L>(output: L)
where
    L: Layer<Layered<reload::Layer<EnvFilter, Registry>, Registry>> + Send + Sync + 'static,
{
    let (environment, rejected) = match environment_directives() {
        Ok(environment) => (environment, None),
        Err(error) => (None, Some(error)),
    };
    let control = LogControl::detached_with(environment);
    let (filter, handle) = reload::Layer::new(EnvFilter::new(control.current_directives()));
    tracing_subscriber::registry()
        .with(filter)
        .with(output)
        .init();
    let control = Arc::new(LogControl {
        handle: Some(handle),
        ..control
    });
    if let Some(error) = rejected {
        tracing::warn!(
            error,
            "RUST_LOG is not a valid filter and is ignored; [log] level applies"
        );
    }
    // Only `main` installs, once per process.
    let _ = INSTALLED.set(control);
}

/// The installed control, or a detached one when nothing was installed (in
/// tests, and for commands that never start the daemon).
pub fn installed() -> Arc<LogControl> {
    INSTALLED
        .get()
        .cloned()
        .unwrap_or_else(|| Arc::new(LogControl::detached()))
}

/// `RUST_LOG`, if it is set to a filter that parses. An empty value counts
/// as unset.
fn environment_directives() -> Result<Option<String>, String> {
    let Ok(value) = std::env::var(EnvFilter::DEFAULT_ENV) else {
        return Ok(None);
    };
    if value.trim().is_empty() {
        return Ok(None);
    }
    EnvFilter::try_new(&value)
        .map(|_| Some(value.trim().trim_end_matches(',').to_owned()))
        .map_err(|error| error.to_string())
}

/// The filter for a configured level. Levels finer than `info` apply to
/// vacancyd only.
fn level_directives(level: LogLevel) -> String {
    match level {
        LogLevel::Error | LogLevel::Warn | LogLevel::Info => level.as_str().to_owned(),
        LogLevel::Debug | LogLevel::Trace => {
            format!("{LIBRARY_LEVEL},{CRATE_TARGET}={}", level.as_str())
        }
    }
}

impl LogControl {
    pub fn detached() -> Self {
        Self::detached_with(None)
    }

    fn detached_with(environment: Option<String>) -> Self {
        Self {
            handle: None,
            environment,
            inner: Mutex::new(Inner {
                configured: LogLevel::default(),
                window: None,
                window_limit: DEBUG_WINDOW_MAX,
            }),
            changed: tokio::sync::Notify::new(),
        }
    }

    fn lock(&self) -> MutexGuard<'_, Inner> {
        self.inner
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
    }

    /// Cap later debug windows at `limit` (see [`debug_window_limit`]).
    pub fn limit_debug_window(&self, limit: Duration) {
        self.lock().window_limit = limit.clamp(DEBUG_WINDOW_MIN, DEBUG_WINDOW_MAX);
    }

    /// Apply the running configuration's `[log] level`. `RUST_LOG` and an
    /// open debug window still take precedence over it.
    pub fn configure(&self, level: LogLevel) {
        let mut inner = self.lock();
        inner.configured = level;
        self.apply(&inner);
        let view = self.view(&inner, Instant::now(), SystemTime::now());
        drop(inner);
        tracing::info!(
            level = view.level,
            source = view.source,
            configured = level.as_str(),
            "log level"
        );
    }

    /// Open a debug window for `duration`, or move the end of the open one.
    /// A duration within bounds but beyond the log budget is shortened to
    /// fit, and [`Opened::limited`] says so.
    pub fn open_debug_window(&self, duration: Duration) -> Result<Opened, InvalidDuration> {
        self.open_debug_window_at(duration, Instant::now(), SystemTime::now())
    }

    fn open_debug_window_at(
        &self,
        duration: Duration,
        now: Instant,
        wall: SystemTime,
    ) -> Result<Opened, InvalidDuration> {
        if !(DEBUG_WINDOW_MIN..=DEBUG_WINDOW_MAX).contains(&duration) {
            return Err(InvalidDuration);
        }
        let mut inner = self.lock();
        let granted = duration.min(inner.window_limit);
        inner.window = Some(DebugWindow {
            closes_at: now + granted,
            closes_at_wall: wall + granted,
        });
        self.apply(&inner);
        let view = self.view(&inner, now, wall);
        drop(inner);
        self.changed.notify_one();
        let limited = granted < duration;
        tracing::info!(
            minutes = granted.as_secs() / 60,
            requested_minutes = duration.as_secs() / 60,
            limited,
            until = view.debug_until.as_deref().unwrap_or_default(),
            "debug logging window opened"
        );
        Ok(Opened {
            logging: view,
            granted,
            limited,
        })
    }

    /// End the open debug window early. Ending when none is open changes
    /// nothing and is not an error.
    pub fn close_debug_window(&self) -> Logging {
        let mut inner = self.lock();
        let was_open = inner.window.take().is_some();
        self.apply(&inner);
        let view = self.view(&inner, Instant::now(), SystemTime::now());
        drop(inner);
        if was_open {
            self.changed.notify_one();
            tracing::info!(level = view.level, "debug logging window ended early");
        }
        view
    }

    /// Close the window if it is due at `now` on either clock. Returns
    /// whether it closed.
    fn expire(&self, now: Instant, wall: SystemTime) -> bool {
        let mut inner = self.lock();
        if inner.window.is_none_or(|window| !window.is_due(now, wall)) {
            return false;
        }
        inner.window = None;
        self.apply(&inner);
        let view = self.view(&inner, now, wall);
        drop(inner);
        tracing::info!(level = view.level, "debug logging window closed");
        true
    }

    /// What is being logged now. A window that is due closes here even if
    /// [`Self::supervise`] has not run yet, so a report never shows a
    /// window that has already ended.
    pub fn status(&self) -> Logging {
        self.status_at(Instant::now(), SystemTime::now())
    }

    fn status_at(&self, now: Instant, wall: SystemTime) -> Logging {
        self.expire(now, wall);
        self.view(&self.lock(), now, wall)
    }

    /// Close each debug window when it is due. Runs for the daemon's
    /// lifetime; a window that would outlive the process simply ends with it.
    pub async fn supervise(self: Arc<Self>) {
        loop {
            // `notify_one` leaves a permit when nobody waits, so a change
            // made between reading the deadline and waiting is not lost.
            let deadline = self.lock().window.map(|window| window.closes_at);
            match deadline {
                Some(closes_at) => {
                    let wake = closes_at.min(Instant::now() + WALL_CLOCK_CHECK);
                    tokio::select! {
                        () = tokio::time::sleep_until(wake) => {
                            self.expire(Instant::now(), SystemTime::now());
                        }
                        () = self.changed.notified() => {}
                    }
                }
                None => self.changed.notified().await,
            }
        }
    }

    fn current_directives(&self) -> String {
        self.directives(&self.lock())
    }

    fn directives(&self, inner: &Inner) -> String {
        let base = match &self.environment {
            Some(environment) => environment.clone(),
            None => level_directives(inner.configured),
        };
        if inner.window.is_none() || !self.window_raises(inner) {
            return base;
        }
        // A later directive for the same target replaces an earlier one.
        format!("{base},{CRATE_TARGET}=debug")
    }

    /// Whether a window's `vacancyd=debug` would log more than the base
    /// filter does. It is never added where it would lower verbosity.
    fn window_raises(&self, inner: &Inner) -> bool {
        match &self.environment {
            Some(environment) => environment_reach(environment).crate_level < LevelFilter::DEBUG,
            None => inner.configured < LogLevel::Debug,
        }
    }

    fn apply(&self, inner: &Inner) {
        let Some(handle) = &self.handle else {
            return;
        };
        let directives = self.directives(inner);
        let result = EnvFilter::try_new(&directives)
            .map_err(|error| error.to_string())
            .and_then(|filter| handle.reload(filter).map_err(|error| error.to_string()));
        if let Err(error) = result {
            tracing::warn!(directives, error, "could not change the log filter");
        }
    }

    fn view(&self, inner: &Inner, now: Instant, wall: SystemTime) -> Logging {
        // With RUST_LOG in force, the window decides vacancyd's level only
        // when it raised it and no narrower RUST_LOG directive overrides it.
        let (level, source) = match (inner.window.is_some(), &self.environment) {
            (false, None) => (inner.configured.as_str(), "config"),
            (false, Some(_)) => ("custom", "environment"),
            (true, None) if inner.configured == LogLevel::Trace => ("trace", "debug_window"),
            (true, None) => ("debug", "debug_window"),
            (true, Some(environment))
                if self.window_raises(inner) && !environment_reach(environment).narrower =>
            {
                ("debug", "debug_window")
            }
            (true, Some(_)) => ("custom", "environment"),
        };
        Logging {
            level: level.into(),
            source: source.into(),
            configured_level: inner.configured.as_str().into(),
            environment_override: self.environment.is_some(),
            debug_until: inner
                .window
                .map(|window| rfc3339_utc(window.closes_at_wall)),
            debug_remaining_s: inner.window.map(|window| {
                let remaining = window.remaining(now, wall);
                remaining.as_millis().div_ceil(1000) as u64
            }),
        }
    }
}

impl DebugWindow {
    fn is_due(&self, now: Instant, wall: SystemTime) -> bool {
        self.closes_at <= now || self.closes_at_wall <= wall
    }

    /// Time left by whichever clock ends the window first.
    fn remaining(&self, now: Instant, wall: SystemTime) -> Duration {
        let wall_left = self
            .closes_at_wall
            .duration_since(wall)
            .unwrap_or(Duration::ZERO);
        self.closes_at.saturating_duration_since(now).min(wall_left)
    }
}

/// `time` as an RFC 3339 UTC timestamp with whole seconds, such as
/// `2026-10-07T04:30:00Z`. A time before 1970 reads as the epoch.
pub fn rfc3339_utc(time: SystemTime) -> String {
    let seconds = time
        .duration_since(SystemTime::UNIX_EPOCH)
        .map_or(0, |since| since.as_secs());
    let (days, of_day) = (seconds / 86_400, seconds % 86_400);
    // Civil date from days since 1970-01-01, after Howard Hinnant's
    // `civil_from_days`, with eras of 400 years.
    let shifted = days + 719_468;
    let era = shifted / 146_097;
    let day_of_era = shifted % 146_097;
    let year_of_era =
        (day_of_era - day_of_era / 1460 + day_of_era / 36_524 - day_of_era / 146_096) / 365;
    let day_of_year = day_of_era - (365 * year_of_era + year_of_era / 4 - year_of_era / 100);
    let month_index = (5 * day_of_year + 2) / 153;
    let day = day_of_year - (153 * month_index + 2) / 5 + 1;
    let month = if month_index < 10 {
        month_index + 3
    } else {
        month_index - 9
    };
    let year = year_of_era + era * 400 + u64::from(month <= 2);
    format!(
        "{year:04}-{month:02}-{day:02}T{:02}:{:02}:{:02}Z",
        of_day / 3600,
        of_day % 3600 / 60,
        of_day % 60
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    fn control(environment: Option<&str>, configured: LogLevel) -> LogControl {
        let control = LogControl::detached_with(environment.map(str::to_owned));
        control.lock().configured = configured;
        control
    }

    #[test]
    fn configured_levels_finer_than_info_apply_to_vacancyd_only() {
        for (level, expected) in [
            (LogLevel::Error, "error"),
            (LogLevel::Warn, "warn"),
            (LogLevel::Info, "info"),
            (LogLevel::Debug, "info,vacancyd=debug"),
            (LogLevel::Trace, "info,vacancyd=trace"),
        ] {
            let control = control(None, level);
            assert_eq!(control.current_directives(), expected);
            let view = control.status();
            assert_eq!(view.level, level.as_str());
            assert_eq!(view.source, "config");
            assert!(!view.environment_override);
            assert_eq!(view.debug_until, None);
        }
    }

    #[test]
    fn rust_log_replaces_the_configured_level() {
        let control = control(Some("warn,vacancyd::proxy=trace"), LogLevel::Debug);
        assert_eq!(control.current_directives(), "warn,vacancyd::proxy=trace");
        let view = control.status();
        assert_eq!(view.level, "custom");
        assert_eq!(view.source, "environment");
        assert_eq!(view.configured_level, "debug");
        assert!(view.environment_override);
    }

    #[test]
    fn a_debug_window_adds_vacancyd_debug_only_where_it_raises_verbosity() {
        for (environment, configured, expected, level, source) in [
            (
                None,
                LogLevel::Info,
                "info,vacancyd=debug",
                "debug",
                "debug_window",
            ),
            (
                None,
                LogLevel::Warn,
                "warn,vacancyd=debug",
                "debug",
                "debug_window",
            ),
            // Already at or finer than the window asks for.
            (
                None,
                LogLevel::Debug,
                "info,vacancyd=debug",
                "debug",
                "debug_window",
            ),
            (
                None,
                LogLevel::Trace,
                "info,vacancyd=trace",
                "trace",
                "debug_window",
            ),
            (
                Some("error"),
                LogLevel::Info,
                "error,vacancyd=debug",
                "debug",
                "debug_window",
            ),
            // RUST_LOG already logs vacancyd more finely: never lowered, and
            // the window does not claim to decide.
            (
                Some("vacancyd=trace"),
                LogLevel::Info,
                "vacancyd=trace",
                "custom",
                "environment",
            ),
            (
                Some("info,vacancy=trace"),
                LogLevel::Info,
                "info,vacancy=trace",
                "custom",
                "environment",
            ),
            // A narrower directive keeps its own level whatever the window
            // adds, so the window cannot claim debug for all of vacancyd.
            (
                Some("info,vacancyd::pressure=warn"),
                LogLevel::Info,
                "info,vacancyd::pressure=warn,vacancyd=debug",
                "custom",
                "environment",
            ),
        ] {
            let control = control(environment, configured);
            let opened = control.open_debug_window(DEBUG_WINDOW_DEFAULT).unwrap();
            assert_eq!(control.current_directives(), expected, "{environment:?}");
            assert!(EnvFilter::try_new(expected).is_ok(), "{expected}");
            assert_eq!(opened.logging.level, level, "{environment:?}");
            assert_eq!(opened.logging.source, source, "{environment:?}");
            assert_eq!(opened.logging.debug_remaining_s, Some(30 * 60));
            assert!(opened.logging.debug_until.is_some());
            assert!(!opened.limited);

            let closed = control.close_debug_window();
            assert_eq!(closed.debug_until, None);
            assert_eq!(closed.debug_remaining_s, None);
            assert_ne!(closed.source, "debug_window");
            assert_eq!(
                control.current_directives(),
                environment.map_or_else(|| level_directives(configured), str::to_owned)
            );
        }
    }

    #[test]
    fn rust_log_reach_follows_the_longest_applicable_target() {
        let reach = |directives| environment_reach(directives);
        for (directives, crate_level, narrower) in [
            ("info", LevelFilter::INFO, false),
            ("vacancyd", LevelFilter::TRACE, false),
            ("trace,vacancyd=warn", LevelFilter::WARN, false),
            ("vacancyd=warn,vac=trace", LevelFilter::WARN, false),
            ("hyper=trace", LevelFilter::OFF, false),
            ("vacancyd_session=trace", LevelFilter::OFF, false),
            ("info,vacancyd::pressure=warn", LevelFilter::INFO, true),
            ("info,[request]=trace", LevelFilter::INFO, true),
            (
                "info,vacancyd[pressure{resource=gpu0}]=trace",
                LevelFilter::INFO,
                true,
            ),
        ] {
            assert!(EnvFilter::try_new(directives).is_ok(), "{directives}");
            assert_eq!(
                reach(directives),
                EnvironmentReach {
                    crate_level,
                    narrower
                },
                "{directives}"
            );
        }
    }

    #[test]
    fn a_window_must_be_between_one_minute_and_four_hours() {
        let control = control(None, LogLevel::Info);
        for refused in [
            Duration::ZERO,
            Duration::from_secs(59),
            DEBUG_WINDOW_MAX + Duration::from_secs(1),
        ] {
            assert_eq!(control.open_debug_window(refused), Err(InvalidDuration));
        }
        assert!(control.lock().window.is_none());
        for accepted in [DEBUG_WINDOW_MIN, DEBUG_WINDOW_MAX] {
            assert!(control.open_debug_window(accepted).is_ok());
        }
    }

    #[test]
    fn the_log_budget_limits_long_windows_on_busy_machines() {
        // One GPU at the default cadence fits the full four hours.
        assert_eq!(debug_window_limit(5_000, 1), DEBUG_WINDOW_MAX);
        assert_eq!(debug_window_limit(5_000, 0), DEBUG_WINDOW_MAX);
        // Four GPUs sampled every second: 240 records a minute.
        let busy = debug_window_limit(1_000, 4);
        assert_eq!(busy, Duration::from_secs(45 * 60));
        assert!(DECISION_RECORD_BYTES * 240 * (busy.as_secs() / 60) <= SERVICE_LOG_TOTAL_BYTES / 2);
        assert_eq!(debug_window_limit(1_000, 10_000), DEBUG_WINDOW_MIN);

        let control = control(None, LogLevel::Info);
        control.limit_debug_window(busy);
        let opened = control.open_debug_window(DEBUG_WINDOW_MAX).unwrap();
        assert!(opened.limited);
        assert_eq!(opened.granted, busy);
        assert_eq!(opened.logging.debug_remaining_s, Some(45 * 60));
        let within = control.open_debug_window(DEBUG_WINDOW_DEFAULT).unwrap();
        assert!(!within.limited);
        assert_eq!(within.granted, DEBUG_WINDOW_DEFAULT);
    }

    #[test]
    fn reopening_moves_the_end_of_the_window() {
        let control = control(None, LogLevel::Info);
        let now = Instant::now();
        let wall = SystemTime::UNIX_EPOCH + Duration::from_secs(1_759_795_200);
        control
            .open_debug_window_at(Duration::from_secs(4 * 60 * 60), now, wall)
            .unwrap();
        let opened = control
            .open_debug_window_at(Duration::from_secs(10 * 60), now, wall)
            .unwrap();
        assert_eq!(
            opened.logging.debug_until.as_deref(),
            Some("2025-10-07T00:10:00Z")
        );
        assert_eq!(opened.logging.debug_remaining_s, Some(600));
    }

    #[test]
    fn the_wall_clock_ends_a_window_the_monotonic_clock_slept_through() {
        let control = control(None, LogLevel::Info);
        let now = Instant::now();
        let wall = SystemTime::UNIX_EPOCH + Duration::from_secs(1_759_795_200);
        control
            .open_debug_window_at(DEBUG_WINDOW_DEFAULT, now, wall)
            .unwrap();
        // Ten minutes of sleep: the monotonic clock moved one second.
        let view = control.status_at(
            now + Duration::from_secs(1),
            wall + Duration::from_secs(10 * 60),
        );
        assert_eq!(view.debug_remaining_s, Some(20 * 60));
        let view = control.status_at(now + Duration::from_secs(2), wall + DEBUG_WINDOW_DEFAULT);
        assert_eq!(view.source, "config");
        assert_eq!(view.debug_until, None);
        assert_eq!(view.debug_remaining_s, None);
        assert!(control.lock().window.is_none());
    }

    #[tokio::test(start_paused = true)]
    async fn a_window_closes_on_its_own_when_it_is_due() {
        let control = Arc::new(control(None, LogLevel::Info));
        let supervisor = tokio::spawn(control.clone().supervise());
        control.open_debug_window(DEBUG_WINDOW_DEFAULT).unwrap();

        tokio::time::sleep(Duration::from_secs(29 * 60)).await;
        assert!(control.lock().window.is_some());
        assert_eq!(control.status().debug_remaining_s, Some(60));

        tokio::time::sleep(Duration::from_secs(2 * 60)).await;
        // Closed by the supervisor, not by a status read.
        assert!(control.lock().window.is_none());
        assert_eq!(control.current_directives(), "info");
        supervisor.abort();
    }

    #[tokio::test(start_paused = true)]
    async fn the_supervisor_follows_a_window_that_was_shortened() {
        let control = Arc::new(control(None, LogLevel::Info));
        let supervisor = tokio::spawn(control.clone().supervise());
        control
            .open_debug_window(Duration::from_secs(10 * 60))
            .unwrap();
        // Let the supervisor start waiting on the ten-minute deadline.
        tokio::time::sleep(Duration::from_secs(5)).await;
        control.open_debug_window(DEBUG_WINDOW_MIN).unwrap();

        // No status read anywhere below: only the supervisor can close it.
        tokio::time::sleep(Duration::from_secs(55)).await;
        assert!(control.lock().window.is_some(), "closed early");
        tokio::time::sleep(Duration::from_secs(6)).await;
        assert!(
            control.lock().window.is_none(),
            "still open after the shortened end"
        );
        assert_eq!(control.current_directives(), "info");
        supervisor.abort();
    }

    #[tokio::test(start_paused = true)]
    async fn a_status_read_never_reports_a_window_that_is_due() {
        // No supervisor runs here.
        let control = control(None, LogLevel::Warn);
        control.open_debug_window(DEBUG_WINDOW_MIN).unwrap();
        tokio::time::advance(DEBUG_WINDOW_MIN).await;
        let view = control.status();
        assert_eq!(view.source, "config");
        assert_eq!(view.level, "warn");
        assert_eq!(view.debug_until, None);
    }

    #[test]
    fn the_installed_filter_is_reloaded_in_place() {
        let (_layer, handle) = reload::Layer::<EnvFilter, Registry>::new(EnvFilter::new("info"));
        let control = LogControl {
            handle: Some(handle.clone()),
            ..control(None, LogLevel::Info)
        };
        let current = || handle.with_current(|filter| filter.to_string()).unwrap();
        control.configure(LogLevel::Warn);
        assert_eq!(current(), "warn");
        control.open_debug_window(DEBUG_WINDOW_DEFAULT).unwrap();
        // The filter lists its most specific directive first.
        assert_eq!(current(), "vacancyd=debug,warn");
        control.close_debug_window();
        assert_eq!(current(), "warn");
    }

    #[test]
    fn timestamps_are_rfc3339_utc() {
        for (seconds, expected) in [
            (0, "1970-01-01T00:00:00Z"),
            (951_782_400, "2000-02-29T00:00:00Z"),
            (1_759_795_200, "2025-10-07T00:00:00Z"),
            (1_791_337_530, "2026-10-07T01:45:30Z"),
            (4_107_542_399, "2100-02-28T23:59:59Z"),
        ] {
            assert_eq!(
                rfc3339_utc(SystemTime::UNIX_EPOCH + Duration::from_secs(seconds)),
                expected
            );
        }
    }
}
