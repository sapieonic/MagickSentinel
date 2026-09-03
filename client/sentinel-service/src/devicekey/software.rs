//! A P-256 key in a PEM file. **Development and CI only.**
//!
//! This exists so the whole enrollment path — key generation, CSR construction, the
//! `POST /v1/devices/enroll` exchange, certificate persistence, and the mTLS handshake
//! the uplink makes with the result — runs in CI on Linux and on a developer's machine
//! against a local gateway. Without it none of that could be tested at all, and the
//! only way to find out whether the CSR was well formed would be to run an MSI on a
//! Windows box.
//!
//! **It does not meet spec 7.2.** The requirement is that the private key never leaves
//! the endpoint because it is generated in CNG and marked non-exportable. This key is
//! a file. Anyone who can read the file has the device's identity, and on Windows that
//! would be everyone the `device\` ACL grants read to — which is `BUILTIN\Users`,
//! i.e. every agent on the floor. That is not a smaller version of the CNG guarantee;
//! it is the absence of it.
//!
//! Three things keep it out of a shipped build, and they are deliberately redundant
//! because any single one of them can be defeated by a plausible-looking change:
//!
//! 1. **The release gate.** [`SoftwareDeviceKey::permitted`] is false in a build
//!    without `debug_assertions` unless the `dev-software-device-key` feature was
//!    typed. `open_or_create` and `open` both return
//!    [`DeviceKeyError::SoftwareKeyNotPermitted`] when it is false, so a release
//!    binary cannot construct one even if the file is sitting there.
//! 2. **Selection order.** [`super::open_or_create`] reaches CNG first on Windows and
//!    never falls through to this on a CNG failure.
//! 3. **Loud surfacing.** Construction logs at `error`, the kind travels with the
//!    credential as [`super::KeyKind::Software`], and the agent turns that into a
//!    heartbeat-visible client event so a fleet view shows which desktops are running
//!    on a key that does not meet the requirement.
//!
//! The module is compiled unconditionally rather than behind the feature, which is a
//! deliberate trade: `cargo test` — the command the repository documents and CI runs
//! — is a debug build with default features, and a feature-gated implementation would
//! simply not be covered by it. Compiling always and gating at construction means the
//! CSR builder, the PKCS#8 round trip and the signature format are all tested on every
//! run, while a release build still cannot use the thing.

use super::{DeviceKey, DeviceKeyError, KeyKind, Result};
use p256::ecdsa::{signature::Signer, Signature, SigningKey};
use p256::pkcs8::{DecodePrivateKey, EncodePrivateKey};
use std::path::{Path, PathBuf};

/// File the key is stored in, inside the device credential directory.
///
/// Named `.dev.` in the middle so that a file listing of a machine that should be
/// running on CNG makes the problem obvious to whoever is looking at it.
pub const KEY_FILE: &str = "device-key.dev.pem";

pub struct SoftwareDeviceKey {
    key: SigningKey,
    path: PathBuf,
}

impl SoftwareDeviceKey {
    /// Whether this build may use a software device key at all.
    ///
    /// `debug_assertions` covers `cargo test`, `cargo run` and a developer's build.
    /// The feature covers the rare case of wanting the software path in an optimised
    /// build — a load test against a staging gateway, say — and has to be typed.
    pub const fn permitted() -> bool {
        cfg!(debug_assertions) || cfg!(feature = "dev-software-device-key")
    }

    fn deny_if_not_permitted() -> Result<()> {
        if Self::permitted() {
            Ok(())
        } else {
            Err(DeviceKeyError::SoftwareKeyNotPermitted)
        }
    }

    /// Shout about it. Called on every construction, not once per process: a log line
    /// that appears only at startup is a log line nobody sees in the window they are
    /// actually looking at.
    fn warn_loudly(path: &Path) {
        tracing::error!(
            target: "sentinel.telemetry",
            event = "device_key.software_in_use",
            key_kind = "software",
            meets_non_exportable_requirement = false,
            // The path, not the key. There is no code path in this crate that logs
            // key material, and there must not be one.
            path_exists = path.exists(),
            "USING A SOFTWARE DEVICE KEY: the private key is a file on disk and does \
             NOT meet the non-exportable-key requirement (spec 7.2). This is a \
             development build only."
        );
    }

    /// Load the key, generating and persisting one if the file is absent.
    pub fn open_or_create(dir: &Path) -> Result<Self> {
        Self::deny_if_not_permitted()?;
        let path = dir.join(KEY_FILE);
        match Self::read(&path) {
            Ok(k) => Ok(k),
            Err(DeviceKeyError::NotFound) => Self::generate(dir),
            Err(e) => Err(e),
        }
    }

    /// Load the key, failing if there is none.
    pub fn open(dir: &Path) -> Result<Self> {
        Self::deny_if_not_permitted()?;
        Self::read(&dir.join(KEY_FILE))
    }

    /// Generate a fresh P-256 key and persist it as PKCS#8 PEM.
    pub fn generate(dir: &Path) -> Result<Self> {
        Self::deny_if_not_permitted()?;
        let path = dir.join(KEY_FILE);
        Self::warn_loudly(&path);

        let key = SigningKey::random(&mut p256::elliptic_curve::rand_core::OsRng);
        let pem = key
            .to_pkcs8_pem(p256::pkcs8::LineEnding::LF)
            .map_err(|e| DeviceKeyError::Malformed(e.to_string()))?;
        std::fs::create_dir_all(dir)?;
        // Write-then-rename, for the same reason `device::save_identity` does it: a
        // torn key file would make the machine look unenrolled and mint a second
        // device row for it on the next start.
        let tmp = dir.join(format!("{KEY_FILE}.tmp"));
        std::fs::write(&tmp, pem.as_bytes())?;
        restrict_permissions(&tmp)?;
        std::fs::rename(&tmp, &path)?;
        Ok(SoftwareDeviceKey { key, path })
    }

    fn read(path: &Path) -> Result<Self> {
        let pem = match std::fs::read_to_string(path) {
            Ok(s) => s,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                return Err(DeviceKeyError::NotFound)
            }
            Err(e) => return Err(DeviceKeyError::Io(e)),
        };
        Self::warn_loudly(path);
        let key = SigningKey::from_pkcs8_pem(&pem)
            .map_err(|e| DeviceKeyError::Malformed(e.to_string()))?;
        Ok(SoftwareDeviceKey { key, path: path.to_path_buf() })
    }

    /// The file the key lives in, for diagnostics.
    pub fn path(&self) -> &Path {
        &self.path
    }
}

/// Owner-only permissions on the key file where the platform has them.
///
/// Cosmetic relative to the real problem — the key is still a file — but a
/// world-readable private key in a developer's home directory is how a development key
/// ends up in a bug report attachment.
#[cfg(unix)]
fn restrict_permissions(path: &Path) -> Result<()> {
    use std::os::unix::fs::PermissionsExt;
    std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600))?;
    Ok(())
}

#[cfg(not(unix))]
fn restrict_permissions(path: &Path) -> Result<()> {
    // On Windows the directory ACL the MSI applies is what governs, and this path is
    // only reachable in a development build in the first place.
    let _ = path;
    Ok(())
}

impl DeviceKey for SoftwareDeviceKey {
    fn public_point(&self) -> Result<[u8; 65]> {
        let encoded = self.key.verifying_key().to_encoded_point(false);
        let bytes = encoded.as_bytes();
        let mut out = [0u8; 65];
        if bytes.len() != 65 {
            return Err(DeviceKeyError::Malformed(format!(
                "expected a 65-byte uncompressed point, got {}",
                bytes.len()
            )));
        }
        out.copy_from_slice(bytes);
        Ok(out)
    }

    fn sign(&self, message: &[u8]) -> Result<Vec<u8>> {
        // `Signer<Signature>` for `SigningKey` is ECDSA over SHA-256, which is the
        // algorithm the CSR's signatureAlgorithm names and the only one the gateway
        // accepts. `to_der` gives the SEQUENCE { r, s } form; the padding rules are
        // the crate's problem here rather than ours.
        let sig: Signature = self.key.sign(message);
        Ok(sig.to_der().as_bytes().to_vec())
    }

    fn kind(&self) -> KeyKind {
        KeyKind::Software
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use p256::ecdsa::signature::Verifier;

    #[test]
    fn a_generated_key_round_trips_through_the_file() {
        let dir = tempfile::tempdir().unwrap();
        let a = SoftwareDeviceKey::generate(dir.path()).unwrap();
        let b = SoftwareDeviceKey::open(dir.path()).unwrap();
        assert_eq!(a.public_point().unwrap(), b.public_point().unwrap());
        assert!(!dir.path().join(format!("{KEY_FILE}.tmp")).exists(), "the temp file is renamed away");
    }

    #[test]
    fn open_or_create_generates_once_and_then_reuses() {
        let dir = tempfile::tempdir().unwrap();
        assert!(matches!(SoftwareDeviceKey::open(dir.path()), Err(DeviceKeyError::NotFound)));
        let first = SoftwareDeviceKey::open_or_create(dir.path()).unwrap().public_point().unwrap();
        let second = SoftwareDeviceKey::open_or_create(dir.path()).unwrap().public_point().unwrap();
        assert_eq!(first, second, "a second call must not silently re-key the device");
    }

    #[test]
    fn the_public_point_is_an_uncompressed_sec1_point() {
        let dir = tempfile::tempdir().unwrap();
        let key = SoftwareDeviceKey::generate(dir.path()).unwrap();
        let point = key.public_point().unwrap();
        assert_eq!(point[0], 0x04, "0x04 is the uncompressed-point marker");
        assert_ne!(&point[1..33], &[0u8; 32], "X is not zero");
        assert_ne!(&point[33..], &[0u8; 32], "Y is not zero");
    }

    #[test]
    fn signatures_verify_against_the_public_point_and_are_der() {
        let dir = tempfile::tempdir().unwrap();
        let key = SoftwareDeviceKey::generate(dir.path()).unwrap();
        let message = b"CertificationRequestInfo would go here";
        let der_sig = key.sign(message).unwrap();

        assert_eq!(der_sig[0], crate::der::tag::SEQUENCE, "DER ECDSA-Sig-Value");
        let parsed = p256::ecdsa::DerSignature::try_from(der_sig.as_slice()).unwrap();
        let point = key.public_point().unwrap();
        let vk = p256::ecdsa::VerifyingKey::from_sec1_bytes(&point).unwrap();
        vk.verify(message, &parsed).expect("the signature verifies against the exported point");
        assert!(vk.verify(b"a different message", &parsed).is_err());
    }

    #[test]
    fn a_corrupt_key_file_is_an_error_rather_than_a_silent_regeneration() {
        // Regenerating would mint a second device identity for a machine that already
        // has a certificate, leaving a stale device row online in the fleet view.
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join(KEY_FILE), b"-----BEGIN PRIVATE KEY-----\nnope\n").unwrap();
        assert!(matches!(
            SoftwareDeviceKey::open_or_create(dir.path()),
            Err(DeviceKeyError::Malformed(_))
        ));
    }

    #[test]
    fn the_kind_never_claims_to_be_non_exportable() {
        let dir = tempfile::tempdir().unwrap();
        let key = SoftwareDeviceKey::generate(dir.path()).unwrap();
        assert_eq!(key.kind(), KeyKind::Software);
        assert!(!key.kind().meets_non_exportable_requirement());
    }

    #[test]
    fn the_release_gate_matches_the_build() {
        // In a test build `debug_assertions` is on, so the software key is permitted
        // and every test above can run. The assertion that matters for shipping is
        // the other direction, and it is structural: `permitted()` is a const fn over
        // `cfg!`, so a release build without the feature cannot reach the file at all.
        assert!(SoftwareDeviceKey::permitted(), "tests are a debug build");
        assert_eq!(
            SoftwareDeviceKey::permitted(),
            cfg!(debug_assertions) || cfg!(feature = "dev-software-device-key")
        );
    }
}
