//! The device key in CNG: a persisted machine key with export switched off.
//!
//! This is the implementation spec 7.2 describes and the one `enroll.go`'s comment
//! promises the server side. The private key is created inside the Microsoft Software
//! Key Storage Provider, its export policy is set to zero before it is finalized, and
//! the only operations this module performs on it afterwards are "give me the public
//! key" and "sign this hash". No code path returns key material, because there is no
//! NCrypt call here that could.
//!
//! `windows-rs` 0.58 — the version this workspace pins — exports everything needed:
//! `NCryptOpenStorageProvider`, `NCryptCreatePersistedKey`, `NCryptSetProperty`,
//! `NCryptFinalizeKey`, `NCryptOpenKey`, `NCryptExportKey`, `NCryptSignHash`,
//! `NCryptFreeObject`, and the `MS_KEY_STORAGE_PROVIDER`,
//! `BCRYPT_ECDSA_P256_ALGORITHM`, `NCRYPT_EXPORT_POLICY_PROPERTY`,
//! `NCRYPT_MACHINE_KEY_FLAG`, `NCRYPT_SILENT_FLAG` and `BCRYPT_ECCPUBLIC_BLOB`
//! constants, all under the `Win32_Security_Cryptography` feature the manifest already
//! enables. The one value it does not export is `NCRYPT_SECURITY_DESCR_FLAG`, which is
//! declared below.
//!
//! **Nothing in this file is exercised by any test.** There is no Windows runner and no
//! hardware-in-the-loop test in this repository, so the cross-compile check is the only
//! thing standing between it and rot. Treat it as unverified rather than as working,
//! exactly as `README.md` says of the rest of the `windows/` code.
//!
//! # Two principals, one key
//!
//! The key has to be a **machine** key (`NCRYPT_MACHINE_KEY_FLAG`) and its ACL has to
//! grant the interactive user read-and-use. This is the same constraint that makes the
//! spool key DPAPI machine scope: `SentinelService.exe` runs as LocalSystem and creates
//! and renews the credential, while `SentinelAgent.exe` runs as the signed-in user and
//! is the process that actually presents the certificate on the ingest socket — so it
//! is the process that has to produce TLS handshake signatures. A key readable only by
//! its creator would leave the agent unable to connect, which would present as an mTLS
//! failure against a certificate that looks perfectly valid.
//!
//! A machine key created without an explicit security descriptor is accessible to
//! SYSTEM and Administrators only, so [`SECURITY_DESCR_SDDL`] is applied at creation.
//! It grants `BUILTIN\Users` read — enough to open the key and sign with it, and not
//! enough to delete it or change its policy — and full control to SYSTEM and
//! Administrators. Note what that means and what it does not: any user on the machine
//! can make the device's signature, which is the same trust level the certificate file
//! itself carries under the `device\` ACL. What they cannot do is take the key with
//! them, which is the property being bought.

use super::{sig_der_from_fixed, DeviceKey, DeviceKeyError, KeyKind, Result};
use sha2::{Digest, Sha256};
use windows::core::{HSTRING, PCWSTR};
use windows::Win32::Foundation::NTE_BAD_KEYSET;
use windows::Win32::Security::Cryptography::{
    NCryptCreatePersistedKey, NCryptExportKey, NCryptFinalizeKey, NCryptFreeObject, NCryptOpenKey,
    NCryptOpenStorageProvider, NCryptSetProperty, NCryptSignHash, BCRYPT_ECCKEY_BLOB,
    BCRYPT_ECCPUBLIC_BLOB, BCRYPT_ECDSA_P256_ALGORITHM, BCRYPT_ECDSA_PUBLIC_P256_MAGIC,
    CERT_KEY_SPEC, MS_KEY_STORAGE_PROVIDER, NCRYPT_EXPORT_POLICY_PROPERTY, NCRYPT_FLAGS,
    NCRYPT_HANDLE, NCRYPT_KEY_HANDLE, NCRYPT_MACHINE_KEY_FLAG, NCRYPT_PROV_HANDLE,
    NCRYPT_SECURITY_DESCR_PROPERTY, NCRYPT_SILENT_FLAG,
};

/// `NCRYPT_SECURITY_DESCR_FLAG`, which `windows-rs` 0.58 does not export.
///
/// Value 0x4, from `ncrypt.h`. Required on the `NCryptSetProperty` call that installs
/// the key's security descriptor: without it the provider treats the blob as an
/// ordinary property value and the ACL is not applied, silently.
const NCRYPT_SECURITY_DESCR_FLAG: NCRYPT_FLAGS = NCRYPT_FLAGS(0x0000_0004);

/// `DACL_SECURITY_INFORMATION`, the security information class the descriptor sets.
const DACL_SECURITY_INFORMATION: u32 = 0x0000_0004;

/// The key's DACL, in SDDL.
///
/// * `(A;;GA;;;SY)` — full control to `NT AUTHORITY\SYSTEM`: the service creates,
///   renews and, on decommission, deletes the key.
/// * `(A;;GA;;;BA)` — full control to `BUILTIN\Administrators`, for support.
/// * `(A;;GR;;;BU)` — `GENERIC_READ` to `BUILTIN\Users`. Read on a CNG key is what
///   permits opening it and signing with it; it does not permit changing the export
///   policy or deleting the key.
///
/// `BU` and not `AU`, for the same reason [`crate::ipc::PIPE_SDDL`] uses it: `BUILTIN\Users`
/// excludes machine accounts and service logons that have no business holding this
/// machine's identity. There is deliberately no `WD` (Everyone) ACE.
///
/// `D:P` rather than a bare `D:`: `P` sets SE_DACL_PROTECTED, which stops the
/// provider's container ACL being inherited on top of this one. Without it a
/// permissive default on the key container would widen what is written here, and the
/// widening would not be visible in this string.
pub const SECURITY_DESCR_SDDL: &str = "D:P(A;;GA;;;SY)(A;;GA;;;BA)(A;;GR;;;BU)";

/// Flags every call in this module carries.
///
/// `NCRYPT_SILENT_FLAG` because there is no UI to show: the service has no desktop and
/// the agent must not be able to raise a modal over a live call. A provider that wants
/// to prompt must fail instead, and it will fail with an error we can log.
fn base_flags() -> NCRYPT_FLAGS {
    NCRYPT_MACHINE_KEY_FLAG | NCRYPT_SILENT_FLAG
}

/// An owned `NCRYPT_PROV_HANDLE` that frees itself.
struct Provider(NCRYPT_PROV_HANDLE);

impl Drop for Provider {
    fn drop(&mut self) {
        // A leaked provider handle keeps a KSP session open for the life of the
        // process. Nothing catastrophic, but the service is long-lived and renews on
        // a schedule, so leaking once a year is still leaking.
        unsafe {
            let _ = NCryptFreeObject(NCRYPT_HANDLE(self.0 .0));
        }
    }
}

/// The device key, held open for the life of the process.
///
/// Held open rather than reopened per signature: rustls asks for a signature inside
/// the TLS handshake, and reopening a KSP key there would put a registry and file
/// system round trip on the connect path of every reconnect on a floor that reconnects
/// whenever its internet hiccups.
pub struct CngDeviceKey {
    key: NCRYPT_KEY_HANDLE,
    /// Kept alive because the key handle is only valid while its provider is.
    _provider: Provider,
}

// The handles are process-wide and the NCrypt API is documented as thread-safe for
// concurrent operations on one key handle, which is what makes it safe to hand this to
// rustls behind an `Arc`. The raw handles are not `Send`/`Sync` in the bindings because
// they are plain integers, hence the manual assertions.
unsafe impl Send for CngDeviceKey {}
unsafe impl Sync for CngDeviceKey {}

impl Drop for CngDeviceKey {
    fn drop(&mut self) {
        unsafe {
            let _ = NCryptFreeObject(NCRYPT_HANDLE(self.key.0));
        }
    }
}

impl CngDeviceKey {
    /// Open the persisted key, or create it if this machine has none.
    ///
    /// Only the service should reach the creating branch: creating a machine key needs
    /// privileges the interactive user does not have, and the agent calls
    /// [`CngDeviceKey::open`] instead.
    pub fn open_or_create(name: &str) -> Result<Self> {
        match Self::open(name) {
            Ok(k) => Ok(k),
            Err(DeviceKeyError::NotFound) => Self::create(name),
            Err(e) => Err(e),
        }
    }

    /// Open the persisted key, failing with [`DeviceKeyError::NotFound`] if absent.
    pub fn open(name: &str) -> Result<Self> {
        let provider = open_provider()?;
        let name = HSTRING::from(name);
        let mut key = NCRYPT_KEY_HANDLE::default();
        let rc = unsafe {
            NCryptOpenKey(
                provider.0,
                &mut key,
                PCWSTR(name.as_ptr()),
                CERT_KEY_SPEC(0),
                base_flags(),
            )
        };
        match rc {
            Ok(()) => Ok(CngDeviceKey { key, _provider: provider }),
            Err(e) if e.code() == NTE_BAD_KEYSET => Err(DeviceKeyError::NotFound),
            Err(e) => Err(DeviceKeyError::Platform(format!("NCryptOpenKey: {e}"))),
        }
    }

    /// Create the key, mark it non-exportable, ACL it, and finalize it.
    ///
    /// The order is not interchangeable. `NCRYPT_EXPORT_POLICY_PROPERTY` and the
    /// security descriptor must both be set **before** `NCryptFinalizeKey`: after
    /// finalization the export policy cannot be tightened, only read, so a key
    /// finalized first is a key that stays exportable for its whole life. That is the
    /// single most important sequencing constraint in this file.
    fn create(name: &str) -> Result<Self> {
        let provider = open_provider()?;
        let name = HSTRING::from(name);
        let mut key = NCRYPT_KEY_HANDLE::default();
        unsafe {
            NCryptCreatePersistedKey(
                provider.0,
                &mut key,
                BCRYPT_ECDSA_P256_ALGORITHM,
                PCWSTR(name.as_ptr()),
                // AT_KEYEXCHANGE / AT_SIGNATURE are CryptoAPI legacy key specs and
                // must be zero for a CNG key.
                CERT_KEY_SPEC(0),
                base_flags(),
            )
            .map_err(|e| DeviceKeyError::Platform(format!("NCryptCreatePersistedKey: {e}")))?;
        }
        let key = CngDeviceKey { key, _provider: provider };

        // Export policy zero: not NCRYPT_ALLOW_EXPORT_FLAG, not
        // NCRYPT_ALLOW_PLAINTEXT_EXPORT_FLAG, nothing. Zero is what makes the private
        // key unable to leave the provider, which is the whole claim.
        let policy = 0u32;
        unsafe {
            NCryptSetProperty(
                NCRYPT_HANDLE(key.key.0),
                NCRYPT_EXPORT_POLICY_PROPERTY,
                &policy.to_le_bytes(),
                NCRYPT_SILENT_FLAG,
            )
            .map_err(|e| {
                DeviceKeyError::Platform(format!("NCryptSetProperty(export policy): {e}"))
            })?;
        }

        key.apply_security_descriptor()?;

        unsafe {
            NCryptFinalizeKey(NCRYPT_HANDLE(key.key.0), NCRYPT_SILENT_FLAG)
                .map_err(|e| DeviceKeyError::Platform(format!("NCryptFinalizeKey: {e}")))?;
        }

        tracing::info!(
            target: "sentinel.telemetry",
            event = "device_key.created",
            key_kind = "cng",
            meets_non_exportable_requirement = true,
            "created a non-exportable P-256 device key in the platform key store"
        );
        Ok(key)
    }

    /// Install [`SECURITY_DESCR_SDDL`] on the key so the agent can sign with it.
    fn apply_security_descriptor(&self) -> Result<()> {
        use windows::Win32::Security::Authorization::{
            ConvertStringSecurityDescriptorToSecurityDescriptorW, SDDL_REVISION_1,
        };
        use windows::Win32::Security::PSECURITY_DESCRIPTOR;
        use windows::Win32::System::Memory::LocalFree;

        let sddl = HSTRING::from(SECURITY_DESCR_SDDL);
        let mut sd = PSECURITY_DESCRIPTOR::default();
        let mut len: u32 = 0;
        unsafe {
            ConvertStringSecurityDescriptorToSecurityDescriptorW(
                PCWSTR(sddl.as_ptr()),
                SDDL_REVISION_1,
                &mut sd,
                Some(&mut len),
            )
            .map_err(|e| DeviceKeyError::Platform(format!("SDDL conversion: {e}")))?;
        }
        // The descriptor is a self-relative blob of `len` bytes; NCrypt copies it, so
        // it only has to outlive the call.
        let blob = unsafe { std::slice::from_raw_parts(sd.0 as *const u8, len as usize) };
        let result = unsafe {
            NCryptSetProperty(
                NCRYPT_HANDLE(self.key.0),
                NCRYPT_SECURITY_DESCR_PROPERTY,
                blob,
                NCRYPT_SECURITY_DESCR_FLAG
                    | NCRYPT_FLAGS(DACL_SECURITY_INFORMATION)
                    | NCRYPT_SILENT_FLAG,
            )
        };
        unsafe {
            // `LocalFree` and not `drop`: the descriptor was allocated by advapi32.
            let _ = LocalFree(Some(windows::Win32::Foundation::HLOCAL(sd.0)));
        }
        result.map_err(|e| {
            DeviceKeyError::Platform(format!("NCryptSetProperty(security descriptor): {e}"))
        })?;
        Ok(())
    }

    /// Delete the persisted key.
    ///
    /// Not called by anything today, and deliberately kept next to creation so the
    /// decommission path has an obvious home. Note that deleting the key strands every
    /// certificate issued against it: the machine can no longer prove it is itself, and
    /// re-enrollment needs a fresh single-use token from the portal.
    pub fn delete(self) -> Result<()> {
        use windows::Win32::Security::Cryptography::NCryptDeleteKey;
        let handle = self.key;
        // Deleting frees the handle, so forget our own Drop rather than double-freeing.
        std::mem::forget(self);
        unsafe {
            NCryptDeleteKey(handle, 0)
                .map_err(|e| DeviceKeyError::Platform(format!("NCryptDeleteKey: {e}")))
        }
    }
}

fn open_provider() -> Result<Provider> {
    let mut handle = NCRYPT_PROV_HANDLE::default();
    unsafe {
        NCryptOpenStorageProvider(&mut handle, MS_KEY_STORAGE_PROVIDER, 0)
            .map_err(|e| DeviceKeyError::Platform(format!("NCryptOpenStorageProvider: {e}")))?;
    }
    Ok(Provider(handle))
}

impl DeviceKey for CngDeviceKey {
    fn public_point(&self) -> Result<[u8; 65]> {
        // Two-call pattern: ask for the size, then the bytes. Only the *public* blob
        // type is ever requested; `BCRYPT_ECCPRIVATE_BLOB` does not appear in this
        // crate and would fail anyway against an export policy of zero.
        let mut needed: u32 = 0;
        unsafe {
            NCryptExportKey(
                self.key,
                NCRYPT_KEY_HANDLE::default(),
                BCRYPT_ECCPUBLIC_BLOB,
                None,
                None,
                &mut needed,
                NCRYPT_SILENT_FLAG,
            )
            .map_err(|e| DeviceKeyError::Platform(format!("NCryptExportKey(size): {e}")))?;
        }
        let mut blob = vec![0u8; needed as usize];
        let mut written: u32 = 0;
        unsafe {
            NCryptExportKey(
                self.key,
                NCRYPT_KEY_HANDLE::default(),
                BCRYPT_ECCPUBLIC_BLOB,
                None,
                Some(&mut blob),
                &mut written,
                NCRYPT_SILENT_FLAG,
            )
            .map_err(|e| DeviceKeyError::Platform(format!("NCryptExportKey: {e}")))?;
        }
        blob.truncate(written as usize);
        point_from_ecc_blob(&blob)
    }

    fn sign(&self, message: &[u8]) -> Result<Vec<u8>> {
        // NCrypt signs a *hash*, so the SHA-256 happens here. `ppaddinginfo` is None
        // and the flags carry no padding mode: padding is an RSA concept and passing
        // one for an ECDSA key is an invalid-parameter error rather than a no-op.
        let digest = Sha256::digest(message);
        let mut needed: u32 = 0;
        unsafe {
            NCryptSignHash(self.key, None, &digest, None, &mut needed, NCRYPT_SILENT_FLAG)
                .map_err(|e| DeviceKeyError::Sign(format!("NCryptSignHash(size): {e}")))?;
        }
        let mut raw = vec![0u8; needed as usize];
        let mut written: u32 = 0;
        unsafe {
            NCryptSignHash(
                self.key,
                None,
                &digest,
                Some(&mut raw),
                &mut written,
                NCRYPT_SILENT_FLAG,
            )
            .map_err(|e| DeviceKeyError::Sign(format!("NCryptSignHash: {e}")))?;
        }
        raw.truncate(written as usize);
        // CNG returns `r || s`, fixed width. X.509 and TLS want DER.
        sig_der_from_fixed(&raw)
    }

    fn kind(&self) -> KeyKind {
        KeyKind::Cng
    }
}

/// Pull `0x04 || X || Y` out of a `BCRYPT_ECCKEY_BLOB`.
///
/// The blob is a `{ dwMagic, cbKey }` header followed by `cbKey` bytes of X and
/// `cbKey` bytes of Y, with **no** leading `0x04` — that marker is a SEC1 encoding
/// detail CNG does not use. The magic is checked rather than assumed: a key that came
/// back as some other curve would otherwise be silently reassembled into a 65-byte
/// buffer of the wrong thing, and the failure would surface as a certificate the
/// gateway cannot verify.
fn point_from_ecc_blob(blob: &[u8]) -> Result<[u8; 65]> {
    let header = std::mem::size_of::<BCRYPT_ECCKEY_BLOB>();
    if blob.len() < header {
        return Err(DeviceKeyError::Malformed(format!(
            "ECC public blob is {} bytes, shorter than its header",
            blob.len()
        )));
    }
    let magic = u32::from_le_bytes(blob[0..4].try_into().expect("4 bytes"));
    let cb_key = u32::from_le_bytes(blob[4..8].try_into().expect("4 bytes")) as usize;
    if magic != BCRYPT_ECDSA_PUBLIC_P256_MAGIC {
        return Err(DeviceKeyError::Malformed(format!(
            "expected a P-256 ECDSA public blob, got magic {magic:#x}"
        )));
    }
    if cb_key != 32 || blob.len() < header + 64 {
        return Err(DeviceKeyError::Malformed(format!(
            "expected 32-byte coordinates, got cbKey {cb_key} in {} bytes",
            blob.len()
        )));
    }
    let mut point = [0u8; 65];
    point[0] = 0x04;
    point[1..].copy_from_slice(&blob[header..header + 64]);
    Ok(point)
}
