//! The SQLCipher key for the spool: generated at enrollment, wrapped with DPAPI at
//! **machine** scope (spec 6.5).
//!
//! # Why machine scope, and why that is not a detail
//!
//! `docs/architecture.md` spells this out: the client is two processes running as two
//! different principals. `SentinelService.exe` is LocalSystem and generates and renews
//! this key; `SentinelAgent.exe` is the interactive user and is the process that
//! actually opens the spool database, because WASAPI is audio-session scoped and
//! capture cannot run in session 0. A user-scoped `CryptProtectData` blob can only be
//! unwrapped by the user who created it, so a user-scoped wrap would leave whichever
//! of the two did not create it unable to open the file — and across a shift change it
//! would leave *every* subsequent agent unable to open a spool the first agent wrapped.
//!
//! This is the opposite of the refresh token, which is correctly user scope: a refresh
//! token belongs to one signed-in agent and the next shift must not inherit it. The two
//! secrets live in different stores for that reason — the token in Credential Manager
//! at user scope (`sentinel_agent::auth::store`), the spool key in a machine-scope
//! DPAPI blob here.
//!
//! # What is stored where
//!
//! The wrapped blob is a file in the device credential directory, next to the
//! certificate, which the MSI ACLs read-only for `Users`: the service writes it, the
//! agent reads it. Machine-scope DPAPI is not a secret from a local administrator —
//! anyone who can run code as SYSTEM on the machine can unwrap it — and it is not
//! meant to be. What it defends is the case the requirement is actually about: a disk
//! or a whole machine leaving the building. The spool database on that disk is
//! SQLCipher-encrypted and its key cannot be unwrapped anywhere but on that machine's
//! Windows installation.
//!
//! # No fallback
//!
//! There is deliberately no default, no constant and no "unconfigured" string in this
//! module. [`resolve`] returns a `Result`, and a release build has exactly two
//! outcomes: a real unwrapped key, or an error the caller must surface. The previous
//! shape — `env::var(..).unwrap_or_else(|_| "unconfigured".into())` — was worse than
//! having no encryption, because it produced a spool that *looked* encrypted while
//! every machine on the floor used the same key.

use std::path::{Path, PathBuf};

/// File holding the wrapped key, inside the device credential directory.
pub const KEY_FILE: &str = "spool.key";

/// Bytes of key material. 32 bytes is what SQLCipher's KDF wants as input entropy;
/// the hex form below is what `PRAGMA key` receives.
const KEY_BYTES: usize = 32;

/// Description string on the DPAPI blob.
///
/// Visible to anyone inspecting the blob, so it names the product and nothing else — no
/// tenant, no device id, no user.
#[cfg(windows)]
const BLOB_DESCRIPTION: &str = "MagickVoice Sentinel spool key";

#[derive(Debug, thiserror::Error)]
pub enum SpoolKeyError {
    #[error("no wrapped spool key exists yet; the device has not been enrolled")]
    NotFound,
    #[error("spool key file error: {0}")]
    Io(#[from] std::io::Error),
    #[error("the wrapped spool key could not be unwrapped: {0}")]
    Unwrap(String),
    #[error("the spool key could not be wrapped: {0}")]
    Wrap(String),
    #[error("the unwrapped spool key is not {KEY_BYTES} bytes")]
    Malformed,
    #[error(
        "DPAPI is not available on this platform, and an unwrapped spool key is not \
         permitted in this build: call audio would sit on disk under a key stored in \
         the clear next to it. Build with --features dev-plaintext-spool-key if that \
         is genuinely what you intend."
    )]
    PlaintextNotPermitted,
}

pub type Result<T> = std::result::Result<T, SpoolKeyError>;

/// Wraps and unwraps the spool key. A trait so the enrollment path is testable off
/// Windows without either mocking the Win32 API or shipping a plaintext fallback into
/// the code that runs on a desktop.
pub trait KeyWrapper: Send + Sync {
    /// Wrap `plaintext` so that only this machine can unwrap it.
    fn wrap(&self, plaintext: &[u8]) -> Result<Vec<u8>>;
    fn unwrap_blob(&self, blob: &[u8]) -> Result<Vec<u8>>;
    /// Whether this wrapper actually binds the key to the machine. Reported in
    /// telemetry so a fleet view can show which desktops do not.
    fn binds_to_machine(&self) -> bool;
}

/// The wrapper for this build: DPAPI at machine scope on Windows, and off Windows the
/// gated development wrapper.
pub fn wrapper() -> Box<dyn KeyWrapper> {
    #[cfg(windows)]
    {
        Box::new(windows_dpapi::DpapiMachineScope)
    }
    #[cfg(not(windows))]
    {
        Box::new(dev::PlaintextWrapper)
    }
}

/// Generate a key, wrap it, and write the blob. Overwrites any existing blob.
///
/// **Called at enrollment and nowhere else.** Re-wrapping a *new* key on an existing
/// machine makes the spool that is already on disk unreadable — which for a compliance
/// product means silently discarding call audio that was captured and not yet
/// acknowledged. If a key ever has to be rotated, the spool has to be drained first,
/// and that is a deliberate operation rather than a side effect of a start-up path.
pub fn generate_and_store(dir: &Path, wrapper: &dyn KeyWrapper) -> Result<String> {
    let mut material = [0u8; KEY_BYTES];
    fill_random(&mut material);
    let blob = wrapper.wrap(&material)?;

    std::fs::create_dir_all(dir)?;
    let path = dir.join(KEY_FILE);
    let tmp = dir.join(format!("{KEY_FILE}.tmp"));
    std::fs::write(&tmp, &blob)?;
    std::fs::rename(&tmp, &path)?;

    tracing::info!(
        target: "sentinel.telemetry",
        event = "spool_key.generated",
        binds_to_machine = wrapper.binds_to_machine(),
        blob_bytes = blob.len(),
        "wrote a wrapped spool key"
    );
    Ok(to_hex(&material))
}

/// Read and unwrap the key, as the hex string `PRAGMA key` takes.
///
/// Hex rather than the raw bytes: `PRAGMA key = '<text>'` treats its argument as a
/// passphrase and runs it through PBKDF2, and a passphrase has to survive being a
/// string. Passing 32 arbitrary bytes would mean worrying about embedded quotes and
/// NULs for no gain, since the entropy is in the material either way.
pub fn resolve(dir: &Path, wrapper: &dyn KeyWrapper) -> Result<String> {
    let path = dir.join(KEY_FILE);
    let blob = match std::fs::read(&path) {
        Ok(b) => b,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Err(SpoolKeyError::NotFound),
        Err(e) => return Err(SpoolKeyError::Io(e)),
    };
    let material = wrapper.unwrap_blob(&blob)?;
    if material.len() != KEY_BYTES {
        return Err(SpoolKeyError::Malformed);
    }
    Ok(to_hex(&material))
}

/// Where the wrapped key lives, for diagnostics that must not log the key itself.
pub fn key_path(dir: &Path) -> PathBuf {
    dir.join(KEY_FILE)
}

fn to_hex(bytes: &[u8]) -> String {
    crate::update::hex_lower(bytes)
}

fn fill_random(out: &mut [u8]) {
    // The OS CSPRNG. `getrandom` on Linux, `BCryptGenRandom` on Windows — both via
    // `rand`'s `OsRng`, which is the same source the PKCE verifier uses.
    use rand::RngCore;
    rand::rngs::OsRng.fill_bytes(out);
}

#[cfg(windows)]
mod windows_dpapi {
    use super::{KeyWrapper, Result, SpoolKeyError, BLOB_DESCRIPTION};
    use windows::core::HSTRING;
    use windows::Win32::Foundation::{HLOCAL, LocalFree};
    use windows::Win32::Security::Cryptography::{
        CryptProtectData, CryptUnprotectData, CRYPTPROTECT_LOCAL_MACHINE, CRYPT_INTEGER_BLOB,
    };

    /// `CryptProtectData` / `CryptUnprotectData` with `CRYPTPROTECT_LOCAL_MACHINE`.
    pub struct DpapiMachineScope;

    /// An output blob from DPAPI, freed with `LocalFree`.
    struct OwnedBlob(CRYPT_INTEGER_BLOB);

    impl OwnedBlob {
        fn to_vec(&self) -> Vec<u8> {
            if self.0.pbData.is_null() || self.0.cbData == 0 {
                return Vec::new();
            }
            unsafe { std::slice::from_raw_parts(self.0.pbData, self.0.cbData as usize) }.to_vec()
        }
    }

    impl Drop for OwnedBlob {
        fn drop(&mut self) {
            if !self.0.pbData.is_null() {
                // DPAPI allocates with LocalAlloc; the caller must LocalFree. For the
                // unwrap direction this buffer holds the key in the clear, so it is
                // also zeroed first — a freed heap block is not a scrubbed one, and
                // this process later writes a minidump on crash.
                unsafe {
                    std::ptr::write_bytes(self.0.pbData, 0, self.0.cbData as usize);
                    let _ = LocalFree(Some(HLOCAL(self.0.pbData as *mut _)));
                }
            }
        }
    }

    fn in_blob(data: &[u8]) -> CRYPT_INTEGER_BLOB {
        CRYPT_INTEGER_BLOB {
            cbData: data.len() as u32,
            // DPAPI does not write through this pointer; the cast away from const is
            // the shape of the C API, not a mutation.
            pbData: data.as_ptr() as *mut u8,
        }
    }

    impl KeyWrapper for DpapiMachineScope {
        fn wrap(&self, plaintext: &[u8]) -> Result<Vec<u8>> {
            let description = HSTRING::from(BLOB_DESCRIPTION);
            let input = in_blob(plaintext);
            let mut out = OwnedBlob(CRYPT_INTEGER_BLOB::default());
            unsafe {
                CryptProtectData(
                    &input,
                    &description,
                    // No optional entropy. A second secret to protect this one only
                    // moves the problem, and it would have to live in the same
                    // directory as the blob.
                    None,
                    None,
                    None,
                    // MACHINE scope. See this module's header: the service wraps and
                    // the agent unwraps, and they are different principals.
                    CRYPTPROTECT_LOCAL_MACHINE,
                    &mut out.0,
                )
                .map_err(|e| SpoolKeyError::Wrap(e.to_string()))?;
            }
            Ok(out.to_vec())
        }

        fn unwrap_blob(&self, blob: &[u8]) -> Result<Vec<u8>> {
            let input = in_blob(blob);
            let mut out = OwnedBlob(CRYPT_INTEGER_BLOB::default());
            unsafe {
                // No flag on the unwrap: the scope is recorded in the blob, so a
                // machine-scope blob unwraps for any principal on the machine and a
                // user-scope one does not. Passing the flag here would be harmless
                // and misleading.
                CryptUnprotectData(&input, None, None, None, None, 0, &mut out.0)
                    .map_err(|e| SpoolKeyError::Unwrap(e.to_string()))?;
            }
            Ok(out.to_vec())
        }

        fn binds_to_machine(&self) -> bool {
            true
        }
    }
}

#[cfg(not(windows))]
mod dev {
    use super::{KeyWrapper, Result, SpoolKeyError};

    /// Off-Windows stand-in for DPAPI. **Development and CI only.**
    ///
    /// It does not wrap anything: the "blob" is the key material with a header. That is
    /// the honest shape, because there is no machine-bound key store to use here and
    /// pretending otherwise — obfuscating the bytes, XORing them with something derived
    /// from the hostname — would produce something that looks like protection in a
    /// code review and is not.
    ///
    /// Gated the same way the software device key is: permitted in a debug build,
    /// otherwise requiring the `dev-plaintext-spool-key` feature to be typed
    /// deliberately. The shipping client is Windows-only, so this path is not reachable
    /// in a release artefact at all; the gate is there for the case where someone
    /// builds a release binary on Linux to benchmark something.
    pub struct PlaintextWrapper;

    /// A header so the file cannot be mistaken for a real DPAPI blob, and so a machine
    /// that somehow received one from elsewhere fails loudly.
    const MAGIC: &[u8] = b"SENTINEL-DEV-UNWRAPPED-SPOOL-KEY\0";

    const fn permitted() -> bool {
        cfg!(debug_assertions) || cfg!(feature = "dev-plaintext-spool-key")
    }

    fn deny_if_not_permitted() -> Result<()> {
        if permitted() {
            Ok(())
        } else {
            Err(SpoolKeyError::PlaintextNotPermitted)
        }
    }

    impl KeyWrapper for PlaintextWrapper {
        fn wrap(&self, plaintext: &[u8]) -> Result<Vec<u8>> {
            deny_if_not_permitted()?;
            tracing::error!(
                target: "sentinel.telemetry",
                event = "spool_key.unwrapped_storage",
                binds_to_machine = false,
                "STORING THE SPOOL KEY UNWRAPPED: there is no DPAPI on this platform, \
                 so the key sits next to the database it encrypts. Development only."
            );
            let mut out = Vec::with_capacity(MAGIC.len() + plaintext.len());
            out.extend_from_slice(MAGIC);
            out.extend_from_slice(plaintext);
            Ok(out)
        }

        fn unwrap_blob(&self, blob: &[u8]) -> Result<Vec<u8>> {
            deny_if_not_permitted()?;
            let body = blob
                .strip_prefix(MAGIC)
                .ok_or_else(|| SpoolKeyError::Unwrap("not a development key blob".into()))?;
            Ok(body.to_vec())
        }

        fn binds_to_machine(&self) -> bool {
            false
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A wrapper that binds nothing, for the platform-neutral tests below. Separate
    /// from the development wrapper so these tests exercise `generate_and_store` and
    /// `resolve` rather than the gate.
    struct Reversing;

    impl KeyWrapper for Reversing {
        fn wrap(&self, plaintext: &[u8]) -> Result<Vec<u8>> {
            let mut v = plaintext.to_vec();
            v.reverse();
            Ok(v)
        }
        fn unwrap_blob(&self, blob: &[u8]) -> Result<Vec<u8>> {
            let mut v = blob.to_vec();
            v.reverse();
            Ok(v)
        }
        fn binds_to_machine(&self) -> bool {
            false
        }
    }

    struct Refusing;

    impl KeyWrapper for Refusing {
        fn wrap(&self, _p: &[u8]) -> Result<Vec<u8>> {
            Err(SpoolKeyError::Wrap("no key store".into()))
        }
        fn unwrap_blob(&self, _b: &[u8]) -> Result<Vec<u8>> {
            Err(SpoolKeyError::Unwrap("wrong machine".into()))
        }
        fn binds_to_machine(&self) -> bool {
            true
        }
    }

    #[test]
    fn a_generated_key_round_trips_through_the_wrapped_blob() {
        let dir = tempfile::tempdir().unwrap();
        let w = Reversing;
        let key = generate_and_store(dir.path(), &w).unwrap();
        assert_eq!(key.len(), 64, "32 bytes as lowercase hex");
        assert!(key.chars().all(|c| c.is_ascii_hexdigit()));
        assert_eq!(resolve(dir.path(), &w).unwrap(), key);
        assert!(!dir.path().join(format!("{KEY_FILE}.tmp")).exists());
    }

    #[test]
    fn the_key_material_is_not_stored_in_the_clear_by_the_wrapper() {
        // A trivial check that `generate_and_store` actually hands the material to the
        // wrapper rather than writing it and calling the wrapper afterwards.
        let dir = tempfile::tempdir().unwrap();
        let key = generate_and_store(dir.path(), &Reversing).unwrap();
        let blob = std::fs::read(key_path(dir.path())).unwrap();
        assert_eq!(blob.len(), 32);
        assert_ne!(to_hex(&blob), key, "the blob is not the key");
    }

    #[test]
    fn two_generations_produce_different_keys() {
        let a = tempfile::tempdir().unwrap();
        let b = tempfile::tempdir().unwrap();
        assert_ne!(
            generate_and_store(a.path(), &Reversing).unwrap(),
            generate_and_store(b.path(), &Reversing).unwrap(),
            "every machine gets its own key"
        );
    }

    #[test]
    fn a_missing_blob_is_not_found_rather_than_an_empty_key() {
        // This is the case the old `unwrap_or_else(|_| "unconfigured")` swallowed. It
        // must be distinguishable, because "not enrolled yet" and "enrolled but the
        // key will not unwrap" call for different operator actions.
        let dir = tempfile::tempdir().unwrap();
        assert!(matches!(resolve(dir.path(), &Reversing), Err(SpoolKeyError::NotFound)));
    }

    #[test]
    fn an_unwrappable_blob_is_an_error_and_never_a_default_key() {
        let dir = tempfile::tempdir().unwrap();
        generate_and_store(dir.path(), &Reversing).unwrap();
        // Same blob, a wrapper that refuses: this is what a disk moved to another
        // machine looks like.
        assert!(matches!(resolve(dir.path(), &Refusing), Err(SpoolKeyError::Unwrap(_))));
    }

    #[test]
    fn a_short_or_long_unwrapped_key_is_rejected() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(key_path(dir.path()), b"too short").unwrap();
        assert!(matches!(resolve(dir.path(), &Reversing), Err(SpoolKeyError::Malformed)));
    }

    #[test]
    fn a_failure_to_wrap_leaves_no_blob_behind() {
        // A blob written before the wrap succeeded would be a file that looks like a
        // key and unwraps to nothing.
        let dir = tempfile::tempdir().unwrap();
        assert!(generate_and_store(dir.path(), &Refusing).is_err());
        assert!(!key_path(dir.path()).exists());
    }
}
