//! Adapter implementations — one module per inference server dialect.
//!
//! Mirrors the layout dnsweaver uses for `providers/`: the trait and
//! registry live in [`crate::plugin::adapter`], each implementation lives
//! here in its own module, and nothing else in the daemon knows their names.

pub mod chatterbox;
pub mod comfyui;
pub mod ollama;
pub mod openai;

#[cfg(test)]
mod unload_tests;
