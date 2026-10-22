//! Presence-source implementations, one per platform.
//!
//! Adding a platform means implementing
//! [`PresenceSource`](crate::plugin::presence::PresenceSource) here and
//! selecting it in [`crate::plugin::presence::for_platform`]. Windows reads
//! WTS sessions plus per-session reporters; Linux reads systemd-logind over
//! the system D-Bus. A richer Linux signal (foreground app, media, precise
//! idle) needs a per-session agent, and macOS wants `NSWorkspace` plus
//! `CGSessionCopyCurrentDictionary`.

#[cfg(any(not(any(windows, target_os = "linux")), test))]
pub mod fallback;
#[cfg(target_os = "linux")]
pub mod linux;
#[cfg(any(target_os = "linux", test))]
pub mod logind_selection;
#[cfg(any(target_os = "linux", test))]
pub mod owner_processes;
#[cfg(any(windows, test))]
pub mod power_requests;
#[cfg(any(windows, test))]
pub mod session_selection;
#[cfg(windows)]
pub mod windows;
