//! One agent per interactive session (spec 6.1).
//!
//! Two agents in one session would open two capture streams on the same endpoint,
//! open two ingest sockets for one device — which the gateway answers with `4429` —
//! and race each other on one spool database. A named mutex is the cheap, correct
//! guard.
//!
//! **`Local\`, not `Global\`.** The `Local\` namespace is per-session, so the console
//! session and an RDP session each get their own agent, which is exactly right on a
//! floor that uses fast user switching between shifts. A `Global\` mutex would let
//! the first session to start block every other session's agent, and the second shift
//! would silently not be recorded.

/// Mutex name, in the session-local namespace.
pub const MUTEX_NAME: &str = r"Local\MagickVoiceSentinelAgent";

/// Held for the process's lifetime. Dropping it releases the session.
pub struct InstanceGuard {
    #[cfg(windows)]
    handle: windows::Win32::Foundation::HANDLE,
}

#[derive(Debug, thiserror::Error)]
pub enum InstanceError {
    #[error("another Sentinel agent is already running in this session")]
    AlreadyRunning,
    #[error("could not create the single-instance mutex: {0}")]
    Platform(String),
}

#[cfg(windows)]
pub fn acquire() -> Result<InstanceGuard, InstanceError> {
    use windows::core::PCWSTR;
    use windows::Win32::Foundation::{GetLastError, ERROR_ALREADY_EXISTS};
    use windows::Win32::System::Threading::CreateMutexW;

    let name: Vec<u16> = MUTEX_NAME.encode_utf16().chain(std::iter::once(0)).collect();
    unsafe {
        let handle = CreateMutexW(None, true, PCWSTR(name.as_ptr()))
            .map_err(|e| InstanceError::Platform(e.message()))?;
        // CreateMutexW *succeeds* and returns a valid handle when the mutex already
        // exists — the only signal is ERROR_ALREADY_EXISTS from GetLastError, which
        // must be read before anything else can overwrite it. Checking only the
        // handle would let every duplicate instance through.
        if GetLastError() == ERROR_ALREADY_EXISTS {
            let _ = windows::Win32::Foundation::CloseHandle(handle);
            return Err(InstanceError::AlreadyRunning);
        }
        Ok(InstanceGuard { handle })
    }
}

/// Off Windows there is no session namespace and no second agent to collide with.
#[cfg(not(windows))]
pub fn acquire() -> Result<InstanceGuard, InstanceError> {
    Ok(InstanceGuard {})
}

#[cfg(windows)]
impl Drop for InstanceGuard {
    fn drop(&mut self) {
        unsafe {
            let _ = windows::Win32::System::Threading::ReleaseMutex(self.handle);
            let _ = windows::Win32::Foundation::CloseHandle(self.handle);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_mutex_is_session_scoped_not_global() {
        // A Global\ mutex would let the console session's agent block every other
        // session's, and the second shift would silently not be recorded.
        assert!(MUTEX_NAME.starts_with(r"Local\"));
        assert!(!MUTEX_NAME.starts_with(r"Global\"));
    }
}
