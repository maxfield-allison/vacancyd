//! Probe implementations — one module per resource kind.
//!
//! Mirrors dnsweaver's `sources/` layout: the trait and registry live in
//! [`crate::plugin::probe`], each implementation lives here.

pub mod gpu_id;
pub mod gpu_procs;
pub mod listeners;
pub mod nvidia;
pub mod system;
