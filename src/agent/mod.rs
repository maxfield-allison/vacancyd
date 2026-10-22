//! The user-session agent, and the channel it reports over.
//!
//! # Why this exists
//!
//! A Windows service runs in session 0 and cannot see the interactive
//! desktop. Session *state* (active / locked / logged-out) it can read
//! through the Terminal Services API, and that carries the owner check on its
//! own — but two finer signals are out of reach:
//!
//! - **Idle time.** `WTSINFOEX.LastInputTime` would have supplied it without
//!   an agent, but it is documented as unreliable and measures `0` in
//!   practice. `GetLastInputInfo` works and reports only for the *calling*
//!   session, which a service can never be.
//! - **Foreground application**, which lets lending continue while the owner
//!   writes email but stop the moment they launch a game.
//!
//! The daemon and per-session reporter share a package. `vacancyd-session`
//! is the windowless reporter; `vacancyd agent` uses the same client code
//! for foreground diagnostics. Neither reporter makes admission decisions.
//!
//! # Trust
//!
//! The agent **reports facts and never decides**. Policy stays in the
//! service. Three rules keep that honest:
//!
//! 1. **A missing agent must never increase lending.** No report means
//!    `idle_seconds: None`, and the service behaves exactly as it does with
//!    no agent installed — conservatively.
//! 2. **Stale data is not data.** Reports carry sample time and sequence,
//!    expire, and are invalidated as soon as their connection ends.
//! 3. **The pipe peer establishes the session.** The ACL admits interactive
//!    users, while the service asks Windows which session opened each exact
//!    pipe instance. A process cannot claim a different user's session.
//!
//! The remaining trust boundary is deliberate: another process inside the
//! same Windows logon session can still impersonate that session's reporter.
//! This channel trusts the logon session, not the installed executable.
//!
//! # Platform support
//!
//! The channel is a Windows named pipe today, because session 0 is a Windows
//! problem. On other platforms the modules compile to nothing: the service
//! runs without an agent channel and reports `idle_seconds: None`, which is
//! the same safe default as a Windows install with no agent registered.
//! A Linux agent would report the same [`AgentReport`] over a unix socket —
//! the report type is deliberately platform-neutral so that lands without
//! touching the service.

#[cfg(windows)]
pub mod admin_server;
#[cfg(windows)]
pub mod client;
#[cfg(windows)]
pub mod server;
#[cfg(any(windows, target_os = "linux", test))]
pub mod supervise;

mod report;
pub use report::*;

#[cfg(windows)]
mod session_lock;
