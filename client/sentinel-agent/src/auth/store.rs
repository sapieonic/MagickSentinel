//! Refresh-token storage (spec 7.3 step 7).
//!
//! Windows Credential Manager, with the blob additionally wrapped by DPAPI at **user**
//! scope. Two layers because they defend different things: Credential Manager keeps
//! the secret out of any file the agent writes and ties it to the user profile, and
//! the DPAPI wrap means a credential blob copied out of the vault by an administrator
//! is useless on another machine or under another account.
//!
//! **User scope, not machine scope** — the opposite of the spool key. This is per-user
//! data on a desktop that runs two or three shifts; machine scope would let the
//! evening shift's agent process decrypt the morning shift's refresh token, and a
//! refresh token is a sign-in.

use std::sync::Mutex;

#[derive(Debug, thiserror::Error)]
pub enum StoreError {
    #[error("credential store error: {0}")]
    Platform(String),
    #[error("the stored credential could not be unwrapped; it will be discarded")]
    Corrupt,
}

/// Where the refresh token lives.
pub trait TokenStore: Send + Sync {
    fn save_refresh_token(&self, token: &str) -> Result<(), StoreError>;
    fn load_refresh_token(&self) -> Result<Option<String>, StoreError>;
    /// Remove the credential. Called on sign-out — after the spool has flushed.
    fn clear(&self) -> Result<(), StoreError>;
}

/// Credential Manager target name. Per-user by construction: Credential Manager
/// vaults are per-profile, so the same name in two profiles is two credentials.
pub const CREDENTIAL_TARGET: &str = "MagickVoice/Sentinel/refresh_token";

/// In-process store for tests and for the `--headless` development mode.
///
/// Deliberately not a file: a development build that persisted a real refresh token
/// in plaintext would eventually be run against a real tenant.
#[derive(Debug, Default)]
pub struct MemoryTokenStore {
    inner: Mutex<Option<String>>,
}

impl MemoryTokenStore {
    pub fn new() -> Self {
        Self::default()
    }
}

impl TokenStore for MemoryTokenStore {
    fn save_refresh_token(&self, token: &str) -> Result<(), StoreError> {
        *self.inner.lock().unwrap() = Some(token.to_string());
        Ok(())
    }

    fn load_refresh_token(&self) -> Result<Option<String>, StoreError> {
        Ok(self.inner.lock().unwrap().clone())
    }

    fn clear(&self) -> Result<(), StoreError> {
        *self.inner.lock().unwrap() = None;
        Ok(())
    }
}

#[cfg(windows)]
pub use win::CredentialManagerStore;

#[cfg(windows)]
mod win {
    use super::*;
    use windows::core::PWSTR;
    use windows::Win32::Foundation::{HLOCAL, LocalFree};
    use windows::Win32::Security::Credentials::{
        CredDeleteW, CredFree, CredReadW, CredWriteW, CREDENTIALW, CRED_PERSIST_LOCAL_MACHINE,
        CRED_TYPE_GENERIC,
    };
    use windows::Win32::Security::Cryptography::{
        CryptProtectData, CryptUnprotectData, CRYPTPROTECT_UI_FORBIDDEN, CRYPT_INTEGER_BLOB,
    };

    /// The real store: DPAPI-wrapped, held in Credential Manager.
    pub struct CredentialManagerStore {
        target: Vec<u16>,
    }

    impl CredentialManagerStore {
        pub fn new() -> Self {
            CredentialManagerStore { target: wide(CREDENTIAL_TARGET) }
        }
    }

    impl Default for CredentialManagerStore {
        fn default() -> Self {
            Self::new()
        }
    }

    fn wide(s: &str) -> Vec<u16> {
        s.encode_utf16().chain(std::iter::once(0)).collect()
    }

    /// Owns a buffer DPAPI allocated with `LocalAlloc`.
    struct DpapiBlob(CRYPT_INTEGER_BLOB);

    impl DpapiBlob {
        fn as_slice(&self) -> &[u8] {
            if self.0.pbData.is_null() {
                return &[];
            }
            unsafe { std::slice::from_raw_parts(self.0.pbData, self.0.cbData as usize) }
        }
    }

    impl Drop for DpapiBlob {
        fn drop(&mut self) {
            if !self.0.pbData.is_null() {
                // Zero before freeing: this buffer held a refresh token, and freed
                // heap is readable by anything that later allocates it.
                unsafe {
                    std::ptr::write_bytes(self.0.pbData, 0, self.0.cbData as usize);
                    let _ = LocalFree(HLOCAL(self.0.pbData as *mut _));
                }
            }
        }
    }

    fn protect(plain: &[u8]) -> Result<DpapiBlob, StoreError> {
        let input = CRYPT_INTEGER_BLOB {
            cbData: plain.len() as u32,
            pbData: plain.as_ptr() as *mut u8,
        };
        let mut out = CRYPT_INTEGER_BLOB::default();
        unsafe {
            // No CRYPTPROTECT_LOCAL_MACHINE flag: its absence is what selects **user**
            // scope. Adding it would let every account on a shared desktop decrypt
            // every other account's refresh token.
            //
            // CRYPTPROTECT_UI_FORBIDDEN because the agent may be running before the
            // shell is up; a DPAPI prompt on a session-0-adjacent desktop would hang
            // with no visible window.
            CryptProtectData(
                &input,
                None,
                None,
                None,
                None,
                CRYPTPROTECT_UI_FORBIDDEN,
                &mut out,
            )
            .map_err(|e| StoreError::Platform(e.message()))?;
        }
        Ok(DpapiBlob(out))
    }

    fn unprotect(wrapped: &[u8]) -> Result<DpapiBlob, StoreError> {
        let input = CRYPT_INTEGER_BLOB {
            cbData: wrapped.len() as u32,
            pbData: wrapped.as_ptr() as *mut u8,
        };
        let mut out = CRYPT_INTEGER_BLOB::default();
        unsafe {
            CryptUnprotectData(
                &input,
                None,
                None,
                None,
                None,
                CRYPTPROTECT_UI_FORBIDDEN,
                &mut out,
            )
            // A blob written under a different profile, or on a machine that has since
            // been re-imaged, cannot be unwrapped. That is a discard-and-sign-in-again
            // case, not an error to retry.
            .map_err(|_| StoreError::Corrupt)?;
        }
        Ok(DpapiBlob(out))
    }

    impl TokenStore for CredentialManagerStore {
        fn save_refresh_token(&self, token: &str) -> Result<(), StoreError> {
            let wrapped = protect(token.as_bytes())?;
            let bytes = wrapped.as_slice();
            let mut target = self.target.clone();
            let mut comment = wide("MagickVoice Sentinel sign-in");

            let cred = CREDENTIALW {
                Flags: Default::default(),
                Type: CRED_TYPE_GENERIC,
                TargetName: PWSTR(target.as_mut_ptr()),
                Comment: PWSTR(comment.as_mut_ptr()),
                CredentialBlobSize: bytes.len() as u32,
                CredentialBlob: bytes.as_ptr() as *mut u8,
                // LOCAL_MACHINE persistence means "survives logoff on this machine",
                // not "readable by other users" — the vault is still per-profile, and
                // the DPAPI wrap is still user-scoped. ENTERPRISE would try to roam
                // the credential to a domain profile, taking a machine-bound DPAPI
                // blob with it, where it would fail to unwrap on the next desktop.
                Persist: CRED_PERSIST_LOCAL_MACHINE,
                ..Default::default()
            };
            unsafe { CredWriteW(&cred, 0) }.map_err(|e| StoreError::Platform(e.message()))?;
            Ok(())
        }

        fn load_refresh_token(&self) -> Result<Option<String>, StoreError> {
            let mut ptr: *mut CREDENTIALW = std::ptr::null_mut();
            let read = unsafe {
                CredReadW(
                    windows::core::PCWSTR(self.target.as_ptr()),
                    CRED_TYPE_GENERIC,
                    0,
                    &mut ptr,
                )
            };
            if read.is_err() || ptr.is_null() {
                // Not found is the ordinary signed-out state, not a failure.
                return Ok(None);
            }
            let result = (|| {
                let cred = unsafe { &*ptr };
                let wrapped = unsafe {
                    std::slice::from_raw_parts(
                        cred.CredentialBlob,
                        cred.CredentialBlobSize as usize,
                    )
                };
                let plain = unprotect(wrapped)?;
                String::from_utf8(plain.as_slice().to_vec()).map_err(|_| StoreError::Corrupt)
            })();
            unsafe { CredFree(ptr as *const std::ffi::c_void) };

            match result {
                Ok(t) => Ok(Some(t)),
                // A credential we cannot unwrap will never become readable; leaving it
                // behind means every start pays for the same failure.
                Err(StoreError::Corrupt) => {
                    let _ = self.clear();
                    Ok(None)
                }
                Err(e) => Err(e),
            }
        }

        fn clear(&self) -> Result<(), StoreError> {
            let r = unsafe {
                CredDeleteW(
                    windows::core::PCWSTR(self.target.as_ptr()),
                    CRED_TYPE_GENERIC,
                    0,
                )
            };
            // Deleting a credential that is not there is the desired end state.
            match r {
                Ok(()) => Ok(()),
                Err(_) => Ok(()),
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_memory_store_round_trips_and_clears() {
        let s = MemoryTokenStore::new();
        assert_eq!(s.load_refresh_token().unwrap(), None);
        s.save_refresh_token("rt-1").unwrap();
        assert_eq!(s.load_refresh_token().unwrap().as_deref(), Some("rt-1"));
        s.save_refresh_token("rt-2").unwrap();
        assert_eq!(s.load_refresh_token().unwrap().as_deref(), Some("rt-2"));
        s.clear().unwrap();
        assert_eq!(s.load_refresh_token().unwrap(), None);
        s.clear().unwrap();
    }

    #[test]
    fn the_credential_target_is_namespaced_to_this_product() {
        // Credential Manager is a shared vault; a generic target name would collide
        // with, or be readable as, some other application's credential.
        assert!(CREDENTIAL_TARGET.starts_with("MagickVoice/Sentinel/"));
    }
}
