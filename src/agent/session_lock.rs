//! Session-local singleton shared by both agent entry points.
use anyhow::{Context, Result};
use windows_sys::Win32::{
    Foundation::{CloseHandle, GetLastError, ERROR_ALREADY_EXISTS, HANDLE},
    System::Threading::CreateMutexW,
};

pub struct SessionLock(HANDLE);

impl SessionLock {
    pub fn acquire() -> Result<Option<Self>> {
        Self::named("Local\\vacancyd-session-agent-v1")
    }

    fn named(name: &str) -> Result<Option<Self>> {
        let name: Vec<u16> = name.encode_utf16().chain(Some(0)).collect();
        // SAFETY: valid terminated name. No thread ownership is requested:
        // handle lifetime, not mutex ownership, is our singleton lease.
        let handle = unsafe { CreateMutexW(std::ptr::null(), 0, name.as_ptr()) };
        if handle.is_null() {
            return Err(std::io::Error::last_os_error()).context("creating session agent lock");
        }
        let lock = Self(handle);
        // SAFETY: queried immediately after CreateMutexW as required.
        if unsafe { GetLastError() } == ERROR_ALREADY_EXISTS {
            return Ok(None); // closes only this additional handle
        }
        Ok(Some(lock))
    }
}

impl Drop for SessionLock {
    fn drop(&mut self) {
        // SAFETY: uniquely owned handle; Windows releases it on process death too.
        unsafe {
            CloseHandle(self.0);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn duplicate_is_refused_until_the_last_handle_closes() {
        let name = format!("Local\\vacancyd-lock-test-{}", std::process::id());
        let first = SessionLock::named(&name).unwrap().unwrap();
        assert!(SessionLock::named(&name).unwrap().is_none());
        drop(first);
        assert!(SessionLock::named(&name).unwrap().is_some());
    }
}
