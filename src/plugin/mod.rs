//! Plugin seams.
//!
//! vacancyd has exactly three places where someone will want to add support
//! for hardware, a server, or an operating system that we do not have. Each
//! is a trait plus a registry keyed by a `kind` string that appears verbatim
//! in the config file, so adding an implementation is: write the type,
//! register it, document the kind. Nothing else in the daemon changes.
//!
//! | Seam | Trait | Answers | Config key |
//! | ---- | ----- | ------- | ---------- |
//! | [`probe`] | [`Probe`](probe::Probe) | "how much of this resource is free?" | `[[resource]] kind` |
//! | [`adapter`] | [`Adapter`](adapter::Adapter) | "is this request inference, and which model?" | `[[upstream]] kind` |
//! | [`presence`] | [`PresenceSource`](presence::PresenceSource) | "is the owner using the machine?" | automatic per-OS |
//!
//! The registries are deliberately plain: a `&'static str` to a constructor.
//! There is no dynamic loading and no plugin ABI — implementations are
//! compiled in. That keeps a single self-contained binary, which is the
//! whole deployment story on both Windows and Linux.

pub mod adapter;
pub mod presence;
pub mod pressure;
pub mod probe;
