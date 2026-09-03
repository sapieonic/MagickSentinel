//! Loading the enrolled device credential and presenting it over mTLS.
//!
//! The credential is two halves that live in different places on purpose:
//!
//! * the **certificate chain**, a file under
//!   `%PROGRAMDATA%\MagickVoice\Sentinel\device\` that the MSI ACLs read-only for
//!   `Users` because it is machine identity; and
//! * the **private key**, which on Windows is not a file at all — it is a
//!   non-exportable CNG key that this process can ask to sign things and cannot read
//!   (`sentinel_service::devicekey`).
//!
//! That split is the reason this module exists rather than the credential being two
//! PEM buffers handed to rustls. rustls loads a private key by parsing DER, and there
//! is no DER to parse: the whole point of the key is that nothing can produce it. So
//! the key is presented as an implementation of [`rustls::sign::SigningKey`] that
//! delegates to `NCryptSignHash`, and both consumers of the credential are wired to
//! accept a signer rather than key bytes.
//!
//! # The two consumers, and why they are wired differently
//!
//! **The ingest socket** (`tungstenite` over rustls) takes a `ClientConfig` directly,
//! so it gets `with_client_cert_resolver` and a resolver that always answers with this
//! device's [`rustls::sign::CertifiedKey`].
//!
//! **The REST client** (`ureq`) does not expose its `ClientConfig`; it builds one from
//! its own `TlsConfig`, which wants a `PrivateKeyDer`. The way through is the one seam
//! rustls leaves: a `CryptoProvider` carries the `KeyProvider` that turns
//! `PrivateKeyDer` into an `Arc<dyn SigningKey>`, and ureq lets a provider be supplied
//! (`unversioned_rustls_crypto_provider`). So the agent hands ureq a provider whose
//! key provider ignores the DER it is given and returns the CNG-backed signer, plus a
//! placeholder `PrivateKeyDer` that is never read. This is a deliberate use of an API
//! marked unversioned, and it is documented as such at the call site; the alternative
//! was implementing ureq's `Transport` trait over a rustls stream by hand, which is
//! considerably more code in the connect path of the process that ships call audio.
//!
//! Both paths sign through the same [`sentinel_service::devicekey::DeviceKey`], so
//! there is one key and one place it can be used from.
//!
//! # Failure is not a downgrade
//!
//! [`load`] returning `None` means the uplink presents no client certificate and the
//! gateway refuses the connection. That is the correct failure and it is preserved
//! exactly as the previous `device_certificate()` stub described it: capture still
//! spools locally and uploads once a certificate exists. There is no path here that
//! connects without a certificate, and no configuration that turns mTLS off.

use rustls::client::ResolvesClientCert;
use rustls::crypto::{CryptoProvider, KeyProvider};
use rustls::pki_types::{CertificateDer, PrivateKeyDer, SubjectPublicKeyInfoDer};
use rustls::sign::{CertifiedKey, Signer, SigningKey};
use rustls::{SignatureAlgorithm, SignatureScheme};
use sentinel_service::devicekey::{self, spki_p256, DeviceKey, KeyKind};
use std::path::Path;
use std::sync::Arc;

/// The loaded credential.
#[derive(Clone)]
pub struct DeviceCredential {
    certified_key: Arc<CertifiedKey>,
    signing_key: Arc<dyn SigningKey>,
    /// The device id from the enrollment record, for telemetry and the heartbeat.
    pub device_id: String,
    /// RFC3339 expiry, as the gateway reported it at enrollment.
    pub not_after: String,
    /// Which key implementation is behind the signer. Reported rather than assumed:
    /// [`KeyKind::Software`] does **not** meet spec 7.2's non-exportable requirement.
    pub key_kind: KeyKind,
}

impl std::fmt::Debug for DeviceCredential {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        // No key, no certificate bytes: this ends up in log lines.
        f.debug_struct("DeviceCredential")
            .field("device_id", &self.device_id)
            .field("not_after", &self.not_after)
            .field("key_kind", &self.key_kind.as_str())
            .finish()
    }
}

#[derive(Debug, thiserror::Error)]
pub enum CredentialError {
    #[error("this machine is not enrolled: no identity record in {0}")]
    NotEnrolled(String),
    #[error("the device certificate could not be read: {0}")]
    Io(#[from] std::io::Error),
    #[error("the device certificate is not valid PEM")]
    BadCertificate,
    #[error(transparent)]
    Key(#[from] devicekey::DeviceKeyError),
}

/// Load the credential this machine enrolled with.
///
/// Reads the identity record first: it is written last by enrollment, so its presence
/// is what "enrolled" means, and a certificate file with no record beside it is
/// treated as debris from an interrupted enrollment rather than as an identity.
pub fn load(dir: &Path) -> Result<DeviceCredential, CredentialError> {
    let Some(identity) = sentinel_service::device::load_identity(dir)? else {
        return Err(CredentialError::NotEnrolled(dir.display().to_string()));
    };

    let chain = read_chain(&dir.join(&identity.cert_path), &dir.join(&identity.ca_chain_path))?;
    if chain.is_empty() {
        return Err(CredentialError::BadCertificate);
    }

    // `open_existing`, never `open_or_create`: the agent presents machine identity, it
    // does not mint it. Under the installer's ACL it could not write `device\` anyway,
    // but keeping the two entry points distinct means that stays true even where the
    // ACL does not apply.
    let key: Arc<dyn DeviceKey> = Arc::from(devicekey::open_existing(dir)?);
    let key_kind = key.kind();
    let signing_key: Arc<dyn SigningKey> = Arc::new(DeviceSigningKey::new(key)?);
    let certified_key = Arc::new(CertifiedKey::new(chain, signing_key.clone()));

    Ok(DeviceCredential {
        certified_key,
        signing_key,
        device_id: identity.device_id,
        not_after: identity.not_after,
        key_kind,
    })
}

/// The leaf followed by whatever chain was issued with it.
///
/// The chain file is optional in practice — a gateway whose CA is a public
/// intermediate needs it, one whose CA is directly trusted does not — so a missing or
/// empty chain is not an error. A missing *leaf* is.
fn read_chain(
    cert_path: &Path,
    chain_path: &Path,
) -> Result<Vec<CertificateDer<'static>>, CredentialError> {
    let mut pem = std::fs::read(cert_path)?;
    if let Ok(mut chain) = std::fs::read(chain_path) {
        pem.push(b'\n');
        pem.append(&mut chain);
    }
    let certs: Vec<CertificateDer<'static>> = rustls_pemfile::certs(&mut pem.as_slice())
        .collect::<Result<_, _>>()
        .map_err(|_| CredentialError::BadCertificate)?;
    Ok(certs)
}

impl DeviceCredential {
    /// The rustls credential, for the ingest socket.
    pub fn certified_key(&self) -> Arc<CertifiedKey> {
        self.certified_key.clone()
    }

    /// A client-certificate resolver that always answers with this device.
    pub fn resolver(&self) -> Arc<dyn ResolvesClientCert> {
        single_identity_resolver(self.certified_key.clone())
    }

    /// The certificate chain, in the shape ureq's `ClientCert` wants.
    pub fn ureq_client_cert(&self) -> ureq::tls::ClientCert {
        let certs: Vec<ureq::tls::Certificate<'static>> = self
            .certified_key
            .cert
            .iter()
            .map(|c| ureq::tls::Certificate::from_der(c.as_ref()).to_owned())
            .collect();
        // The placeholder. See the module header: ureq's TLS config demands a
        // `PrivateKey` value, our `KeyProvider` ignores it, and the real key cannot be
        // materialised as DER because it lives in CNG. The bytes are a marker rather
        // than anything key-shaped so that a future change which *did* try to parse
        // them fails immediately and loudly instead of half-working.
        let placeholder = ureq::tls::PrivateKey::from_pem(PLACEHOLDER_KEY_PEM.as_bytes())
            .expect("the placeholder key PEM is a compile-time constant")
            .to_owned();
        ureq::tls::ClientCert::new_with_certs(&certs, placeholder)
    }

    /// A rustls `CryptoProvider` whose key provider hands back this device's signer.
    ///
    /// Leaks one allocation per process. `CryptoProvider::key_provider` is a
    /// `&'static dyn KeyProvider`, and the credential is loaded once at start-up and
    /// lives for the life of the process, so a leak here is a one-off constant rather
    /// than growth. The alternative — a global `OnceLock` — is the same lifetime with
    /// more machinery around it.
    pub fn ureq_crypto_provider(&self) -> Arc<CryptoProvider> {
        let base = default_crypto_provider();
        let key_provider: &'static dyn KeyProvider =
            Box::leak(Box::new(DeviceKeyProvider(self.signing_key.clone())));
        Arc::new(CryptoProvider { key_provider, ..base })
    }

    /// Does the key behind this credential meet spec 7.2?
    pub fn meets_non_exportable_requirement(&self) -> bool {
        self.key_kind.meets_non_exportable_requirement()
    }
}

/// The `ring` provider, which is what `ureq`'s `rustls` feature already links.
///
/// Named explicitly rather than taken from the process default: `CryptoProvider::get_default`
/// returns `None` unless something installed one, and picking whichever of `ring` and
/// `aws-lc-rs` happens to be in the dependency graph is how a build silently changes
/// its FIPS posture.
fn default_crypto_provider() -> CryptoProvider {
    rustls::crypto::ring::default_provider()
}

/// A PEM key that is never parsed. See [`DeviceCredential::ureq_client_cert`].
///
/// Not a real key: a real one here would be a private key sitting in the binary, which
/// is exactly the thing this whole module exists to avoid, and a reader finding it
/// would reasonably assume it was in use.
const PLACEHOLDER_KEY_PEM: &str = concat!(
    "-----BEGIN PRIVATE KEY-----\n",
    // "SENTINEL PLACEHOLDER - NOT A KEY" in base64, padded to a legal PEM body.
    "U0VOVElORUwgUExBQ0VIT0xERVIgLSBOT1QgQSBLRVk=\n",
    "-----END PRIVATE KEY-----\n"
);

/// Wrap a credential as a rustls client-certificate resolver.
///
/// One definition, used by both the REST client and the ingest socket, so there is a
/// single answer to "what does this machine present".
pub fn single_identity_resolver(key: Arc<CertifiedKey>) -> Arc<dyn ResolvesClientCert> {
    Arc::new(AlwaysThisDevice(key))
}

#[derive(Debug)]
struct AlwaysThisDevice(Arc<CertifiedKey>);

impl ResolvesClientCert for AlwaysThisDevice {
    fn resolve(
        &self,
        _root_hint_subjects: &[&[u8]],
        _sigschemes: &[SignatureScheme],
    ) -> Option<Arc<CertifiedKey>> {
        // The hints are ignored on purpose. This machine has exactly one identity, and
        // a device that declined to present it because the server's CA hint did not
        // match would produce a `4403` that looks like a revocation. Presenting it and
        // letting the gateway decide gives an error that says what actually happened.
        Some(self.0.clone())
    }

    fn has_certs(&self) -> bool {
        true
    }
}

/// The `KeyProvider` seam that lets ureq use a key it cannot see.
#[derive(Debug)]
struct DeviceKeyProvider(Arc<dyn SigningKey>);

impl KeyProvider for DeviceKeyProvider {
    fn load_private_key(
        &self,
        _key_der: PrivateKeyDer<'static>,
    ) -> Result<Arc<dyn SigningKey>, rustls::Error> {
        // The argument is ignored, and that is safe because this provider is
        // constructed per credential and installed on an agent that presents exactly
        // one client certificate. It is not a general-purpose provider and must not be
        // set as a process default.
        Ok(self.0.clone())
    }
}

/// `rustls::sign::SigningKey` over a [`DeviceKey`].
struct DeviceSigningKey {
    key: Arc<dyn DeviceKey>,
    /// Cached so `public_key` can hand out a borrow, which lets rustls check that the
    /// certificate and the key actually match at load time rather than at handshake
    /// time on a customer's floor.
    spki: Vec<u8>,
}

impl std::fmt::Debug for DeviceSigningKey {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("DeviceSigningKey")
            .field("kind", &self.key.kind().as_str())
            .finish()
    }
}

impl DeviceSigningKey {
    fn new(key: Arc<dyn DeviceKey>) -> Result<Self, devicekey::DeviceKeyError> {
        let spki = spki_p256(&key.public_point()?);
        Ok(DeviceSigningKey { key, spki })
    }
}

impl SigningKey for DeviceSigningKey {
    fn choose_scheme(&self, offered: &[SignatureScheme]) -> Option<Box<dyn Signer>> {
        // One scheme, because the key is one curve. Offering nothing else is not a
        // limitation to work around: a P-256 key cannot produce a P-384 signature, and
        // a gateway that will not accept ECDSA-P256-SHA256 cannot accept this device.
        offered
            .contains(&SignatureScheme::ECDSA_NISTP256_SHA256)
            .then(|| Box::new(DeviceSigner(self.key.clone())) as Box<dyn Signer>)
    }

    fn public_key(&self) -> Option<SubjectPublicKeyInfoDer<'_>> {
        Some(SubjectPublicKeyInfoDer::from(self.spki.as_slice()))
    }

    fn algorithm(&self) -> SignatureAlgorithm {
        SignatureAlgorithm::ECDSA
    }
}

struct DeviceSigner(Arc<dyn DeviceKey>);

impl std::fmt::Debug for DeviceSigner {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("DeviceSigner")
    }
}

impl Signer for DeviceSigner {
    fn sign(&self, message: &[u8]) -> Result<Vec<u8>, rustls::Error> {
        // rustls hands over the unhashed handshake transcript; `DeviceKey::sign` is
        // defined to hash with SHA-256, which is what this scheme requires, and to
        // return the DER signature form TLS 1.3 wants.
        self.0.sign(message).map_err(|e| {
            // The error text is ours, not the peer's, and carries no key material.
            rustls::Error::General(format!("device key signing failed: {e}"))
        })
    }

    fn scheme(&self) -> SignatureScheme {
        SignatureScheme::ECDSA_NISTP256_SHA256
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use sentinel_service::device::{save_identity, DeviceIdentity};
    use sentinel_service::devicekey::software::SoftwareDeviceKey;
    use sentinel_service::enroll::{CERT_FILE, CHAIN_FILE};

    /// A leaf certificate issued by a throwaway CA against a *different* key, used
    /// only where the test needs some parseable PEM.
    const SOME_CERT_PEM: &str = "-----BEGIN CERTIFICATE-----\n\
        MIIBIjANBgkqhkiG9w0BAQEFAAOCAQ8AMIIBCgKCAQEAtest\n\
        -----END CERTIFICATE-----\n";

    fn enrolled_dir() -> tempfile::TempDir {
        let dir = tempfile::tempdir().unwrap();
        SoftwareDeviceKey::generate(dir.path()).unwrap();
        std::fs::write(dir.path().join(CERT_FILE), SOME_CERT_PEM).unwrap();
        std::fs::write(dir.path().join(CHAIN_FILE), SOME_CERT_PEM).unwrap();
        save_identity(
            dir.path(),
            &DeviceIdentity {
                device_id: "1b4e28ba-2fa1-11d2-883f-0016d3cca427".into(),
                machine_guid: "{guid}".into(),
                hw_fingerprint: "f".repeat(64),
                not_after: "2027-09-03T00:00:00Z".into(),
                cert_path: CERT_FILE.into(),
                ca_chain_path: CHAIN_FILE.into(),
            },
        )
        .unwrap();
        dir
    }

    #[test]
    fn an_unenrolled_machine_has_no_credential() {
        // The correct failure: no certificate means the gateway refuses the
        // connection and capture spools locally, rather than a silent downgrade to an
        // unauthenticated upload.
        let dir = tempfile::tempdir().unwrap();
        assert!(matches!(load(dir.path()), Err(CredentialError::NotEnrolled(_))));
    }

    #[test]
    fn a_certificate_with_no_identity_record_is_debris_not_an_identity() {
        // Enrollment writes the record last, so this is what a machine looks like
        // after an interrupted enrollment. Treating it as enrolled would present a
        // certificate with no device id behind it.
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join(CERT_FILE), SOME_CERT_PEM).unwrap();
        assert!(matches!(load(dir.path()), Err(CredentialError::NotEnrolled(_))));
    }

    #[test]
    fn an_enrolled_machine_loads_a_credential_that_knows_what_kind_of_key_it_has() {
        let dir = enrolled_dir();
        let cred = load(dir.path()).unwrap();
        assert_eq!(cred.device_id, "1b4e28ba-2fa1-11d2-883f-0016d3cca427");
        assert_eq!(cred.not_after, "2027-09-03T00:00:00Z");
        assert_eq!(cred.key_kind, KeyKind::Software);
        assert!(
            !cred.meets_non_exportable_requirement(),
            "a software key must never report that it meets spec 7.2"
        );
        // Leaf plus chain.
        assert_eq!(cred.certified_key().cert.len(), 2);
    }

    #[test]
    fn a_missing_certificate_file_is_an_error_rather_than_an_empty_chain() {
        let dir = enrolled_dir();
        std::fs::remove_file(dir.path().join(CERT_FILE)).unwrap();
        assert!(matches!(load(dir.path()), Err(CredentialError::Io(_))));
    }

    #[test]
    fn a_certificate_that_is_not_pem_is_refused() {
        let dir = enrolled_dir();
        std::fs::write(dir.path().join(CERT_FILE), b"not a certificate").unwrap();
        std::fs::write(dir.path().join(CHAIN_FILE), b"").unwrap();
        assert!(matches!(load(dir.path()), Err(CredentialError::BadCertificate)));
    }

    #[test]
    fn the_chain_file_is_optional() {
        // A gateway whose CA is directly trusted issues no intermediate.
        let dir = enrolled_dir();
        std::fs::remove_file(dir.path().join(CHAIN_FILE)).unwrap();
        let cred = load(dir.path()).unwrap();
        assert_eq!(cred.certified_key().cert.len(), 1);
    }

    #[test]
    fn the_resolver_always_presents_this_device() {
        // Ignoring the CA hint is deliberate: declining to present the certificate
        // would produce a 4403 that reads as a revocation.
        let dir = enrolled_dir();
        let cred = load(dir.path()).unwrap();
        let resolver = cred.resolver();
        assert!(resolver.has_certs());
        assert!(resolver.resolve(&[], &[]).is_some(), "even with no hints and no schemes");
        assert!(resolver
            .resolve(&[b"some-unrelated-ca"], &[SignatureScheme::RSA_PSS_SHA256])
            .is_some());
    }

    #[test]
    fn the_signer_offers_exactly_p256_and_signs_through_the_device_key() {
        use p256::ecdsa::signature::Verifier;
        let dir = enrolled_dir();
        let cred = load(dir.path()).unwrap();
        let certified = cred.certified_key();
        let signing_key = &certified.key;

        assert!(signing_key.choose_scheme(&[SignatureScheme::RSA_PSS_SHA256]).is_none());
        assert!(signing_key.choose_scheme(&[SignatureScheme::ECDSA_NISTP384_SHA384]).is_none());
        let signer = signing_key
            .choose_scheme(&[
                SignatureScheme::ECDSA_NISTP384_SHA384,
                SignatureScheme::ECDSA_NISTP256_SHA256,
            ])
            .expect("P-256 is offered");
        assert_eq!(signer.scheme(), SignatureScheme::ECDSA_NISTP256_SHA256);

        // The signature is the real thing, over the unhashed message, verifiable
        // against the key the device enrolled with.
        let message = b"a TLS 1.3 CertificateVerify transcript would go here";
        let sig = signer.sign(message).unwrap();
        let key = SoftwareDeviceKey::open(dir.path()).unwrap();
        let vk = p256::ecdsa::VerifyingKey::from_sec1_bytes(&key.public_point().unwrap()).unwrap();
        let parsed = p256::ecdsa::DerSignature::try_from(sig.as_slice()).unwrap();
        vk.verify(message, &parsed).expect("rustls' signature verifies");
    }

    #[test]
    fn the_signing_key_publishes_an_spki_so_rustls_can_check_it_against_the_certificate() {
        let dir = enrolled_dir();
        let cred = load(dir.path()).unwrap();
        let certified = cred.certified_key();
        let spki = certified.key.public_key().expect("an SPKI is published");
        let key = SoftwareDeviceKey::open(dir.path()).unwrap();
        assert_eq!(spki.as_ref(), spki_p256(&key.public_point().unwrap()).as_slice());
        assert_eq!(certified.key.algorithm(), SignatureAlgorithm::ECDSA);
    }

    #[test]
    fn the_ureq_key_provider_returns_the_device_signer_whatever_der_it_is_handed() {
        // The seam that lets ureq present a key it cannot see. If this ever stops
        // being true the REST client silently loses its client certificate, and
        // device-scoped routes start answering 403.
        let dir = enrolled_dir();
        let cred = load(dir.path()).unwrap();
        let provider = cred.ureq_crypto_provider();
        let nonsense = PrivateKeyDer::Pkcs8(vec![0u8; 8].into());
        let loaded = provider.key_provider.load_private_key(nonsense).unwrap();
        assert_eq!(loaded.algorithm(), SignatureAlgorithm::ECDSA);
        assert!(loaded.choose_scheme(&[SignatureScheme::ECDSA_NISTP256_SHA256]).is_some());
    }

    #[test]
    fn the_placeholder_key_is_not_a_key() {
        // A real key here would be a private key compiled into the binary, which is
        // the thing this module exists to avoid.
        let der = ureq::tls::PrivateKey::from_pem(PLACEHOLDER_KEY_PEM.as_bytes()).unwrap();
        assert_eq!(
            String::from_utf8_lossy(der.der()),
            "SENTINEL PLACEHOLDER - NOT A KEY",
            "anyone dumping the DER should see what it is"
        );
        assert!(
            rustls::crypto::ring::default_provider()
                .key_provider
                .load_private_key(PrivateKeyDer::Pkcs8(der.der().to_vec().into()))
                .is_err(),
            "the placeholder must not parse as a usable private key"
        );
    }

    #[test]
    fn debug_output_carries_no_key_or_certificate_bytes() {
        let dir = enrolled_dir();
        let cred = load(dir.path()).unwrap();
        let s = format!("{cred:?}");
        assert!(s.contains("1b4e28ba"));
        assert!(!s.contains("BEGIN"), "no PEM in a log line: {s}");
        assert!(!s.contains("PRIVATE"), "{s}");
    }
}
