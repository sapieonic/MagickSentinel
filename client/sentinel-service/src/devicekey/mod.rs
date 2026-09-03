//! The device's private key: the machine half of the two identities capture requires
//! (spec 7.2).
//!
//! The security property this module exists to hold, stated the way `enroll.go` and
//! `contracts/openapi.yaml` both state it:
//!
//! > The private key never leaves the endpoint. It is generated in CNG, marked
//! > non-exportable, and only the CSR crosses the wire.
//!
//! That is why the trait below is shaped the way it is. [`DeviceKey`] can produce a
//! public key and it can produce a signature; there is deliberately **no method that
//! returns private key material**. A CNG key cannot implement such a method, so any
//! caller that needed one would silently force the software implementation, and the
//! property would be lost without anything failing. The absence is the enforcement.
//!
//! # Two implementations, and they are not equivalent
//!
//! * [`cng::CngDeviceKey`] (Windows) — a persisted machine key in the Microsoft
//!   Software Key Storage Provider with its export policy set to zero. Signing is
//!   `NCryptSignHash`; the key bytes are never in this process's address space and
//!   never on the filesystem. **This meets the requirement.**
//!
//! * [`software::SoftwareDeviceKey`] — a P-256 key in a PEM file next to the
//!   certificate. It exists so the enrollment exchange, the CSR, the mTLS handshake
//!   and the uplink can be exercised in CI on Linux, where there is no CNG.
//!   **This does not meet the requirement**: the key is exportable by definition,
//!   because it is a file. Every use of it logs at `error` level and raises a
//!   heartbeat-visible client event, and [`software::SoftwareDeviceKey::permitted`]
//!   refuses to construct one in a release build unless the
//!   `dev-software-device-key` feature was typed deliberately.
//!
//! The distinction is not papered over anywhere: [`KeyKind`] travels with the loaded
//! credential, the enrollment record on disk names which kind signed the CSR, and the
//! agent surfaces it.
//!
//! # A note on the TODO this replaced
//!
//! `sentinel-agent/src/main.rs` used to carry a TODO saying that `windows-rs` 0.58
//! exposes no NCrypt surface for generating a P-256 key non-exportably and building a
//! CSR against it. That was checked and is not true: the pinned 0.58 exports
//! `NCryptOpenStorageProvider`, `NCryptCreatePersistedKey`, `NCryptSetProperty`,
//! `NCryptFinalizeKey`, `NCryptExportKey`, `NCryptSignHash` and the
//! `NCRYPT_EXPORT_POLICY_PROPERTY` / `MS_KEY_STORAGE_PROVIDER` /
//! `BCRYPT_ECDSA_P256_ALGORITHM` constants, all under the
//! `Win32_Security_Cryptography` feature the crate already enables. No direct
//! `ncrypt.dll` binding and no version bump were needed. The CSR is built by
//! [`crate::csr`] over [`der`](crate::der), because the only ASN.1 this client emits
//! is one PKCS#10 request.

pub mod software;

#[cfg(windows)]
pub mod cng;

use crate::der;

/// Name the device key is persisted under in the KSP.
///
/// Stable across renewals on purpose: renewal re-certifies the *same* key rather than
/// minting a new one, so a certificate that fails to arrive does not leave the machine
/// with an orphaned key and no way back to the old one.
pub const KEY_NAME: &str = "MagickVoice.Sentinel.Device";

#[derive(Debug, thiserror::Error)]
pub enum DeviceKeyError {
    #[error("the platform key store is unavailable: {0}")]
    Platform(String),
    #[error("no device key exists yet")]
    NotFound,
    #[error(
        "a software device key is not permitted in this build: the private key would be \
         a file on disk, which does not meet the non-exportable-key requirement (spec \
         7.2). Build with --features dev-software-device-key if that is genuinely what \
         you intend."
    )]
    SoftwareKeyNotPermitted,
    #[error("device key file error: {0}")]
    Io(#[from] std::io::Error),
    #[error("the stored device key is not a usable P-256 key: {0}")]
    Malformed(String),
    #[error("signing failed: {0}")]
    Sign(String),
}

pub type Result<T> = std::result::Result<T, DeviceKeyError>;

/// Which implementation is behind a loaded key. Travels with the credential so the
/// difference between "non-exportable in CNG" and "a file" is never inferred.
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum KeyKind {
    /// Persisted, non-exportable, in a CNG key storage provider.
    Cng,
    /// A PEM file. Development and CI only.
    Software,
}

impl KeyKind {
    /// Does this key satisfy spec 7.2's non-exportable requirement?
    pub fn meets_non_exportable_requirement(self) -> bool {
        matches!(self, KeyKind::Cng)
    }

    pub fn as_str(self) -> &'static str {
        match self {
            KeyKind::Cng => "cng",
            KeyKind::Software => "software",
        }
    }
}

/// The device's signing key.
///
/// `Send + Sync` because rustls holds it inside an `Arc<dyn SigningKey>` for the
/// lifetime of the client config, and both the REST agent and the ingest socket sign
/// through the same instance.
pub trait DeviceKey: Send + Sync {
    /// The public key as an uncompressed SEC1 point: `0x04 || X || Y`, 65 bytes.
    ///
    /// Uncompressed rather than compressed because that is the form both CNG's
    /// `BCRYPT_ECCKEY_BLOB` and RFC 5480's `subjectPublicKey` use, so no point
    /// decompression is needed anywhere in this crate.
    fn public_point(&self) -> Result<[u8; 65]>;

    /// ECDSA over SHA-256 of `message`, as a DER `ECDSA-Sig-Value`.
    ///
    /// The message is hashed here rather than by the caller so that the CNG
    /// implementation — which signs a hash, not a message — and the software one,
    /// which signs a message, present the same interface to rustls, whose
    /// `Signer::sign` is handed the message.
    fn sign(&self, message: &[u8]) -> Result<Vec<u8>>;

    fn kind(&self) -> KeyKind;
}

/// `SubjectPublicKeyInfo` for a P-256 public key (RFC 5480 section 2).
///
/// ```text
/// SubjectPublicKeyInfo ::= SEQUENCE {
///   algorithm  SEQUENCE { OID id-ecPublicKey, OID prime256v1 },
///   subjectPublicKey BIT STRING  -- 0x04 || X || Y
/// }
/// ```
///
/// The named-curve OID goes in the parameters rather than explicit curve parameters:
/// Go's `x509.ParseCertificateRequest` only understands named curves, and
/// `enroll.go` refuses anything whose `PublicKeyAlgorithm` is not ECDSA.
pub fn spki_p256(point: &[u8; 65]) -> Vec<u8> {
    const ID_EC_PUBLIC_KEY: [u32; 6] = [1, 2, 840, 10045, 2, 1];
    const PRIME256V1: [u32; 7] = [1, 2, 840, 10045, 3, 1, 7];
    der::sequence(&[
        der::sequence(&[der::oid(&ID_EC_PUBLIC_KEY), der::oid(&PRIME256V1)]),
        der::bit_string(point),
    ])
}

/// DER `ECDSA-Sig-Value ::= SEQUENCE { r INTEGER, s INTEGER }` from the fixed-width
/// `r || s` form CNG returns.
///
/// CNG hands back 64 bytes: a 32-byte big-endian `r` followed by a 32-byte `s`, both
/// zero-padded. X.509 and TLS want them as DER INTEGERs, which are signed — see
/// [`der::unsigned_integer`] for why the padding matters.
pub fn sig_der_from_fixed(raw: &[u8]) -> Result<Vec<u8>> {
    if raw.len() != 64 {
        return Err(DeviceKeyError::Sign(format!(
            "expected a 64-byte P-256 signature, got {} bytes",
            raw.len()
        )));
    }
    Ok(der::sequence(&[
        der::unsigned_integer(&raw[..32]),
        der::unsigned_integer(&raw[32..]),
    ]))
}

/// Open the device key, creating it if this machine has none.
///
/// Order is not a preference, it is a requirement: CNG first on Windows, and the
/// software key only where CNG cannot exist or where a developer asked for it. A
/// Windows machine whose KSP call fails does **not** fall through to a software key —
/// it returns the platform error, because falling through would turn a broken TPM or a
/// policy-restricted KSP into a silent downgrade of the product's headline security
/// claim. That is precisely the paper-over this module refuses.
pub fn open_or_create(dir: &std::path::Path) -> Result<Box<dyn DeviceKey>> {
    #[cfg(windows)]
    {
        let _ = dir;
        let key = cng::CngDeviceKey::open_or_create(KEY_NAME)?;
        return Ok(Box::new(key));
    }
    #[cfg(not(windows))]
    {
        // No CNG off Windows, so there is nothing to fall back *from*. The gate in
        // `SoftwareDeviceKey` still applies: a release build refuses.
        let key = software::SoftwareDeviceKey::open_or_create(dir)?;
        Ok(Box::new(key))
    }
}

/// Open an existing device key without creating one.
///
/// The agent uses this: it presents the certificate the service enrolled and must not
/// be able to mint machine identity of its own. Under the installer's ACL it could not
/// write `device\` anyway, but the two call sites are kept distinct so that stays true
/// by construction rather than by filesystem permission.
pub fn open_existing(dir: &std::path::Path) -> Result<Box<dyn DeviceKey>> {
    #[cfg(windows)]
    {
        let _ = dir;
        Ok(Box::new(cng::CngDeviceKey::open(KEY_NAME)?))
    }
    #[cfg(not(windows))]
    {
        Ok(Box::new(software::SoftwareDeviceKey::open(dir)?))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_spki_is_the_rfc_5480_named_curve_form() {
        let point = [0x04u8; 65];
        let spki = spki_p256(&point);
        // SEQUENCE { SEQUENCE { OID, OID }, BIT STRING }
        assert_eq!(spki[0], der::tag::SEQUENCE);
        // The two OIDs appear verbatim: id-ecPublicKey then prime256v1.
        let id_ec = [0x06u8, 0x07, 0x2A, 0x86, 0x48, 0xCE, 0x3D, 0x02, 0x01];
        let p256 = [0x06u8, 0x08, 0x2A, 0x86, 0x48, 0xCE, 0x3D, 0x03, 0x01, 0x07];
        let at = spki.windows(id_ec.len()).position(|w| w == id_ec).expect("id-ecPublicKey");
        assert_eq!(&spki[at + id_ec.len()..at + id_ec.len() + p256.len()], &p256);
        // The point is carried whole, with a zero unused-bit count in front.
        let bit_string_at = spki.len() - 67;
        assert_eq!(spki[bit_string_at - 2], der::tag::BIT_STRING);
        assert_eq!(spki[bit_string_at], 0x00);
        assert_eq!(&spki[bit_string_at + 1..], &point[..]);
    }

    #[test]
    fn a_fixed_width_signature_becomes_two_der_integers() {
        let mut raw = [0u8; 64];
        raw[31] = 0x2A; // r = 42
        raw[32] = 0xFF; // s has its top bit set, so it needs the pad
        let der_sig = sig_der_from_fixed(&raw).unwrap();
        assert_eq!(der_sig[0], der::tag::SEQUENCE);
        // r: INTEGER 0x2A
        assert_eq!(&der_sig[2..5], &[0x02, 0x01, 0x2A]);
        // s: INTEGER 00 FF 00 .. (padded, leading zero for the sign bit)
        assert_eq!(&der_sig[5..8], &[0x02, 0x20, 0x00]);
    }

    #[test]
    fn a_signature_of_the_wrong_width_is_an_error_not_a_truncation() {
        // A KSP that answers with a different size is a bug we must not paper over by
        // slicing: a truncated signature fails verification server-side with a
        // message about the certificate, not about us.
        assert!(sig_der_from_fixed(&[0u8; 63]).is_err());
        assert!(sig_der_from_fixed(&[0u8; 65]).is_err());
        assert!(sig_der_from_fixed(&[]).is_err());
    }

    #[test]
    fn only_the_cng_key_claims_to_meet_the_requirement() {
        assert!(KeyKind::Cng.meets_non_exportable_requirement());
        assert!(
            !KeyKind::Software.meets_non_exportable_requirement(),
            "a key in a file is exportable by definition; nothing may report otherwise"
        );
        assert_eq!(KeyKind::Cng.as_str(), "cng");
        assert_eq!(KeyKind::Software.as_str(), "software");
    }
}
