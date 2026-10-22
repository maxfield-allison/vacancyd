//! Public wire contracts shared by the daemon and local clients.
pub mod local_admin;
pub mod settings;
pub mod status;
pub mod status_client;
#[cfg(windows)]
pub mod windows_pipe;
