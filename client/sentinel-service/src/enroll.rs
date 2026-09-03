//! The client half of `POST /v1/devices/enroll` (spec 7.2).
//!
//! One exchange, run once per machine by the SYSTEM service:
//!
//! 1. Generate a P-256 key that cannot leave the machine ([`crate::devicekey`]).
//! 2. Build a PKCS#10 CSR against it ([`crate::csr`]). **Only the CSR crosses the
//!    wire.**
//! 3. `POST` it with the single-use enrollment token the installer was given
//!    (`ENROLLMENTTOKEN`, tenant-scoped, 24 h TTL).
//! 4. Persist the returned certificate and CA chain under the data directory the MSI
//!    ACLs read-only for `Users`, plus the identity record the agent reads.
//! 5. Generate and wrap the spool key ([`crate::spoolkey`]), which is the other secret
//!    that only exists after enrollment.
//!
//! # What the token being single-use forces
//!
//! `enroll.go` consumes the token **atomically before signing**, so a retry with the
//! same token fails even if the response was lost in flight. That is the right server
//! behaviour and it constrains the client: a lost 201 is unrecoverable with that token,
//! so this module must not retry a request it cannot prove failed *before* reaching
//! the handler. [`enroll`] therefore makes exactly one attempt at the HTTP call, and
//! the transport distinguishes "could not connect" — where nothing was consumed and a
//! retry is safe — from "the server answered", where it is not. Getting this wrong
//! burns a deployment wave's token and produces a machine that needs a human.
//!
//! # What is NOT here
//!
//! There is no renewal endpoint in `contracts/openapi.yaml` and this module does not
//! invent one. Renewal is re-enrollment with a fresh token; see [`renewal_decision`].
//!
//! Nothing here talks to the gateway's authenticated routes. Enrollment is the one
//! exchange that legitimately precedes both identities — the machine has no certificate
//! yet, which is the entire point — and the enrollment token is the only credential it
//! carries.

use crate::device::{save_identity, DeviceIdentity};
use crate::devicekey::{DeviceKey, KeyKind};
use crate::spoolkey;
use serde::{Deserialize, Serialize};
use std::path::Path;

/// Certificate and chain filenames inside the credential directory.
///
/// Referenced by name in [`DeviceIdentity`] rather than hard-coded at the read site, so
/// a future format change is one place.
pub const CERT_FILE: &str = "device.crt";
pub const CHAIN_FILE: &str = "ca-chain.crt";

/// The request body, matching `EnrollRequest` in `contracts/openapi.yaml` and
/// `enrollRequest` in `server/gateway/internal/api/enroll.go`.
///
/// Field names are the contract's. `contracts/` is the source of truth: a change starts
/// there and lands in this struct and in the Go one together.
#[derive(Debug, Clone, Serialize)]
pub struct EnrollRequest {
    pub enrollment_token: String,
    pub csr_pem: String,
    pub machine_guid: String,
    pub hw_fingerprint: String,
    pub os_build: String,
    /// `"A"` or `"B"`. The gateway rejects anything else with `unsupported_tier`,
    /// on the grounds that a tier C machine reaching enrollment means the installer's
    /// launch condition was bypassed.
    pub capture_tier: String,
    pub agent_version: String,
}

/// The 201 body: `EnrollResponse`.
#[derive(Debug, Clone, Deserialize)]
pub struct EnrollResponse {
    pub device_id: String,
    pub certificate_pem: String,
    pub ca_chain_pem: String,
    /// RFC3339. Stored verbatim so the renewal decision reads the server's answer
    /// rather than a locally recomputed expiry.
    pub not_after: String,
}

#[derive(Debug, thiserror::Error)]
pub enum EnrollError {
    #[error("no enrollment token was supplied; the installer's ENROLLMENTTOKEN property was empty")]
    NoToken,
    #[error(
        "this machine's capture tier is {0:?}, which cannot be enrolled: only A and B \
         support audio capture, and the installer is supposed to have refused"
    )]
    UnsupportedTier(Option<String>),
    #[error("could not reach the gateway: {0}")]
    Unreachable(String),
    #[error("the enrollment token is invalid, expired, or already used")]
    TokenUnusable,
    #[error("the gateway has no certificate authority configured (503 no_ca)")]
    NoCertificateAuthority,
    #[error("the gateway rejected the enrollment request: {status} {code}")]
    Rejected { status: u16, code: String },
    #[error("the gateway's response could not be decoded: {0}")]
    Decode(String),
    #[error("the issued certificate is not a usable PEM certificate")]
    BadCertificate,
    #[error(transparent)]
    Key(#[from] crate::devicekey::DeviceKeyError),
    #[error(transparent)]
    SpoolKey(#[from] spoolkey::SpoolKeyError),
    #[error("writing the device credential failed: {0}")]
    Io(#[from] std::io::Error),
}

/// The HTTP call, behind a trait.
///
/// A trait for two reasons. The obvious one is testability: the whole exchange, its
/// error mapping and its persistence run against a fake in CI, with no network. The
/// less obvious one is that this is the one request in the client that carries a
/// credential which is destroyed by being used, so the boundary between "the request
/// happened" and "the request did not" has to be explicit and inspectable rather than
/// buried inside a generic HTTP helper.
pub trait EnrollTransport {
    /// Perform the exchange.
    ///
    /// Implementations MUST return [`EnrollError::Unreachable`] only when they are
    /// confident the request never reached the handler, because that is the only error
    /// after which retrying with the same token can succeed.
    fn post(&self, url: &str, body: &EnrollRequest) -> Result<EnrollResponse, EnrollError>;
}

/// What enrollment produced.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Enrolled {
    pub identity: DeviceIdentity,
    /// Which kind of key signed the CSR, so the caller can say so out loud.
    pub key_kind: KeyKind,
    /// The unwrapped SQLCipher key, for the caller that is about to open the spool.
    /// Never logged and never persisted in this form.
    pub spool_key: String,
}

/// Everything about this machine the request carries.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MachineFacts {
    pub machine_guid: String,
    pub hw_fingerprint: String,
    pub os_build: String,
    /// `Some("A")` / `Some("B")`, or `None` on a tier C machine.
    pub capture_tier: Option<String>,
    pub agent_version: String,
}

/// Whether this machine is enrolled, and whether it should re-enroll.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RenewalDecision {
    /// No identity on disk. Enroll if a token is available.
    NotEnrolled,
    /// Enrolled, certificate comfortably valid.
    Current,
    /// Inside the 30-day renewal window (or already expired), and a fresh single-use
    /// token is on disk. Re-enroll.
    RenewNow,
    /// Inside the renewal window with no token. Nothing the client can do; the gateway
    /// already knows every device's `not_after` from `RegisterDevice`, so the alerting
    /// for this lives server-side where it belongs rather than needing a client signal.
    RenewBlockedNoToken,
}

/// File an operator or MDM drops a fresh single-use token into for renewal.
///
/// SYSTEM-writable only, in the same directory as the certificate. There is no client
/// API for minting a token — that is `POST /v1/admin/enrollment-tokens`, which needs
/// the `manage_fleet` capability — so a machine cannot renew itself unattended, by
/// design: a device that could mint its own identity indefinitely is a device a
/// revocation cannot stop.
pub const RENEWAL_TOKEN_FILE: &str = "renewal-token";

/// Decide what to do about the certificate.
///
/// `not_after_ms` and `now_ms` are epoch milliseconds so the caller owns time parsing;
/// [`crate::device::needs_renewal`] treats an unparseable expiry as "renew" rather than
/// as "valid forever", and this inherits that.
pub fn renewal_decision(
    identity: Option<&DeviceIdentity>,
    not_after_ms: Option<i64>,
    now_ms: i64,
    renewal_token_present: bool,
) -> RenewalDecision {
    let Some(_id) = identity else {
        return RenewalDecision::NotEnrolled;
    };
    if !crate::device::needs_renewal(not_after_ms, now_ms) {
        return RenewalDecision::Current;
    }
    if renewal_token_present {
        RenewalDecision::RenewNow
    } else {
        RenewalDecision::RenewBlockedNoToken
    }
}

/// Read the renewal token, if one has been dropped in.
pub fn read_renewal_token(dir: &Path) -> Option<String> {
    let token = std::fs::read_to_string(dir.join(RENEWAL_TOKEN_FILE)).ok()?;
    let token = token.trim().to_string();
    if token.is_empty() {
        None
    } else {
        Some(token)
    }
}

/// Consume the renewal token file.
///
/// Removed *after* a successful enrollment, not before: the token is single-use
/// server-side, so deleting it first would leave a machine whose enrollment failed for
/// a transient reason with no token and no way to retry. Deleting it after means a
/// crash between the 201 and the delete leaves a spent token on disk, which fails
/// loudly on the next attempt with `token_unusable` rather than doing anything unsafe.
pub fn clear_renewal_token(dir: &Path) -> std::io::Result<()> {
    match std::fs::remove_file(dir.join(RENEWAL_TOKEN_FILE)) {
        Ok(()) => Ok(()),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(e) => Err(e),
    }
}

/// Run the exchange and persist everything it produced.
///
/// `dir` is the credential directory — `%PROGRAMDATA%\MagickVoice\Sentinel\device` in
/// production, which `client/installer/README.md` documents as read-only for `Users`
/// precisely because it holds machine identity.
pub fn enroll(
    api_base: &str,
    token: &str,
    facts: &MachineFacts,
    key: &dyn DeviceKey,
    transport: &dyn EnrollTransport,
    key_wrapper: &dyn spoolkey::KeyWrapper,
    dir: &Path,
) -> Result<Enrolled, EnrollError> {
    if token.trim().is_empty() {
        return Err(EnrollError::NoToken);
    }
    let Some(tier) = facts.capture_tier.clone().filter(|t| t == "A" || t == "B") else {
        // Refused here rather than sending it: the gateway would answer
        // `unsupported_tier` and consume nothing, but a token spent on a round trip we
        // already know the answer to is a token an operator has to mint again.
        return Err(EnrollError::UnsupportedTier(facts.capture_tier.clone()));
    };

    // The CSR is built before anything is written, so a key that cannot sign fails
    // before the single-use token is spent.
    let csr_pem = crate::csr::build_csr_pem(key, &facts.machine_guid)?;

    if !key.kind().meets_non_exportable_requirement() {
        tracing::error!(
            target: "sentinel.telemetry",
            event = "enroll.software_key",
            key_kind = key.kind().as_str(),
            meets_non_exportable_requirement = false,
            "enrolling with a SOFTWARE device key: the private key is a file on disk \
             and does not meet the non-exportable-key requirement (spec 7.2)"
        );
    }

    let request = EnrollRequest {
        enrollment_token: token.trim().to_string(),
        csr_pem,
        machine_guid: facts.machine_guid.clone(),
        hw_fingerprint: facts.hw_fingerprint.clone(),
        os_build: facts.os_build.clone(),
        capture_tier: tier,
        agent_version: facts.agent_version.clone(),
    };

    let url = format!("{}/v1/devices/enroll", api_base.trim_end_matches('/'));
    // No token, no fingerprint and no CSR in this log line. The token is single-use
    // but still a credential for its 24 hours, and `MsiHiddenProperties` keeps it out
    // of the MSI log for exactly this reason.
    tracing::info!(
        target: "sentinel.telemetry",
        event = "enroll.request",
        key_kind = key.kind().as_str(),
        capture_tier = %request.capture_tier,
        os_build = %request.os_build,
        "requesting a device certificate"
    );
    let response = transport.post(&url, &request)?;

    let identity = persist(dir, facts, &response)?;
    let spool_key = spoolkey::generate_and_store(dir, key_wrapper)?;

    tracing::info!(
        target: "sentinel.telemetry",
        event = "enroll.succeeded",
        key_kind = key.kind().as_str(),
        meets_non_exportable_requirement = key.kind().meets_non_exportable_requirement(),
        // device_id is machine state and an acceptable telemetry attribute; nothing
        // here is user- or audio-derived.
        device_id = %identity.device_id,
        not_after = %identity.not_after,
        "device enrolled"
    );
    Ok(Enrolled { identity, key_kind: key.kind(), spool_key })
}

/// Write the certificate, the chain and the identity record.
///
/// The identity record is written **last**, and it is the only thing any reader treats
/// as "this machine is enrolled". A crash between the certificate and the record leaves
/// files that are ignored and re-enrolled over; a crash the other way round would leave
/// a machine that claims to be enrolled and has no certificate to present.
fn persist(
    dir: &Path,
    facts: &MachineFacts,
    response: &EnrollResponse,
) -> Result<DeviceIdentity, EnrollError> {
    if !looks_like_certificate_pem(&response.certificate_pem) {
        return Err(EnrollError::BadCertificate);
    }
    std::fs::create_dir_all(dir)?;
    write_atomic(&dir.join(CERT_FILE), response.certificate_pem.as_bytes())?;
    write_atomic(&dir.join(CHAIN_FILE), response.ca_chain_pem.as_bytes())?;

    let identity = DeviceIdentity {
        device_id: response.device_id.clone(),
        machine_guid: facts.machine_guid.clone(),
        hw_fingerprint: facts.hw_fingerprint.clone(),
        not_after: response.not_after.clone(),
        cert_path: CERT_FILE.to_string(),
        ca_chain_path: CHAIN_FILE.to_string(),
    };
    save_identity(dir, &identity)?;
    Ok(identity)
}

fn write_atomic(path: &Path, bytes: &[u8]) -> std::io::Result<()> {
    let tmp = path.with_extension("tmp");
    std::fs::write(&tmp, bytes)?;
    std::fs::rename(&tmp, path)
}

/// A cheap sanity check on what came back.
///
/// Not a parse: the certificate is parsed where it is used, by rustls, which is the
/// component that has to agree with it. This only catches the case where the gateway
/// returned a 201 with an error body or an empty string, which would otherwise be
/// written to disk and fail much later as a confusing TLS error.
fn looks_like_certificate_pem(pem: &str) -> bool {
    pem.contains("-----BEGIN CERTIFICATE-----") && pem.contains("-----END CERTIFICATE-----")
}

/// What [`ensure_enrolled`] did, or refused to do.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum EnsureOutcome {
    /// A valid certificate is already on disk, comfortably inside its lifetime.
    AlreadyEnrolled { device_id: String, not_after: String },
    /// First enrollment, or a renewal, succeeded.
    Enrolled { device_id: String, not_after: String, renewed: bool, key_kind: KeyKind },
    /// This machine has no identity and no token to get one with. The normal state of
    /// a machine installed without `ENROLLMENTTOKEN`, and the reason capture will not
    /// start on it.
    NotEnrolledNoToken,
    /// The certificate is inside its renewal window and no fresh token has been
    /// dropped in. Nothing the client can do: minting a token needs the `manage_fleet`
    /// capability, deliberately, because a device that could re-certify itself forever
    /// is a device revocation cannot stop. The gateway already knows every device's
    /// `not_after` from `RegisterDevice`, so the alerting lives there.
    RenewalBlockedNoToken { not_after: String },
    /// The exchange was attempted and failed. The previous credential, if there was
    /// one, is untouched and still valid until it expires.
    Failed(String),
}

/// Parse the stored `not_after` into epoch milliseconds.
///
/// `None` for anything unparseable, which [`crate::device::needs_renewal`] treats as
/// "renew" rather than as "valid forever" — the safe direction, because the failure
/// then costs a renewal attempt rather than a fleet of expired certificates.
pub fn not_after_ms(not_after: &str) -> Option<i64> {
    time::OffsetDateTime::parse(not_after.trim(), &time::format_description::well_known::Rfc3339)
        .ok()
        .map(|t| (t.unix_timestamp_nanos() / 1_000_000) as i64)
}

/// Bring this machine's device credential up to date.
///
/// Called by the service at start-up. It is the only orchestration point for
/// enrollment and renewal, and it is platform-neutral so the decision table is tested
/// rather than observed once on a VM:
///
/// | On disk | Token | Outcome |
/// |---|---|---|
/// | nothing | none | [`EnsureOutcome::NotEnrolledNoToken`] |
/// | nothing | present | enroll |
/// | current certificate | either | [`EnsureOutcome::AlreadyEnrolled`] |
/// | inside the 30-day window | none | [`EnsureOutcome::RenewalBlockedNoToken`] |
/// | inside the 30-day window | present | renew |
///
/// Renewal is re-enrollment. There is no renewal endpoint in
/// `contracts/openapi.yaml` and this does not invent one: it sends the same
/// `POST /v1/devices/enroll` with a fresh single-use token, and because the CSR is
/// built against the **same** key
/// ([`crate::devicekey::KEY_NAME`] is stable) the machine keeps one identity across
/// renewals. A renewal that fails leaves the old certificate in place — it is still
/// valid, that is what a 30-day window is for.
#[allow(clippy::too_many_arguments)]
pub fn ensure_enrolled(
    dir: &Path,
    api_base: &str,
    facts: &MachineFacts,
    now_ms: i64,
    token: Option<&str>,
    key: &dyn DeviceKey,
    transport: &dyn EnrollTransport,
    key_wrapper: &dyn spoolkey::KeyWrapper,
) -> EnsureOutcome {
    let identity = crate::device::load_identity(dir).ok().flatten();
    let expiry = identity.as_ref().and_then(|i| not_after_ms(&i.not_after));
    let token = token.map(str::trim).filter(|t| !t.is_empty());

    let decision = renewal_decision(identity.as_ref(), expiry, now_ms, token.is_some());
    let renewing = match decision {
        RenewalDecision::Current => {
            let id = identity.expect("Current implies an identity");
            return EnsureOutcome::AlreadyEnrolled {
                device_id: id.device_id,
                not_after: id.not_after,
            };
        }
        RenewalDecision::NotEnrolled if token.is_none() => {
            return EnsureOutcome::NotEnrolledNoToken
        }
        RenewalDecision::RenewBlockedNoToken => {
            let id = identity.expect("RenewBlockedNoToken implies an identity");
            tracing::warn!(
                target: crate::telemetry::TARGET,
                event = "device.renewal_blocked",
                device_id = %id.device_id,
                not_after = %id.not_after,
                "the device certificate is inside its renewal window and no enrollment \
                 token is available; an operator must mint one"
            );
            return EnsureOutcome::RenewalBlockedNoToken { not_after: id.not_after };
        }
        RenewalDecision::NotEnrolled => false,
        RenewalDecision::RenewNow => true,
    };

    let token = token.expect("both remaining branches require a token");
    match enroll(api_base, token, facts, key, transport, key_wrapper, dir) {
        Ok(out) => EnsureOutcome::Enrolled {
            device_id: out.identity.device_id,
            not_after: out.identity.not_after,
            renewed: renewing,
            key_kind: out.key_kind,
        },
        Err(e) => {
            tracing::error!(
                target: crate::telemetry::TARGET,
                event = "enroll.failed",
                renewing,
                error = %e,
                "device enrollment failed"
            );
            EnsureOutcome::Failed(e.to_string())
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::devicekey::software::SoftwareDeviceKey;
    use std::cell::RefCell;

    struct FakeTransport {
        response: RefCell<Vec<Result<EnrollResponse, EnrollError>>>,
        seen: RefCell<Vec<(String, EnrollRequest)>>,
    }

    impl FakeTransport {
        fn ok() -> Self {
            FakeTransport {
                response: RefCell::new(vec![Ok(EnrollResponse {
                    device_id: "1b4e28ba-2fa1-11d2-883f-0016d3cca427".into(),
                    certificate_pem: "-----BEGIN CERTIFICATE-----\nAAAA\n-----END CERTIFICATE-----\n"
                        .into(),
                    ca_chain_pem: "-----BEGIN CERTIFICATE-----\nBBBB\n-----END CERTIFICATE-----\n"
                        .into(),
                    not_after: "2027-09-03T00:00:00Z".into(),
                })]),
                seen: RefCell::new(Vec::new()),
            }
        }

        fn failing(e: EnrollError) -> Self {
            FakeTransport {
                response: RefCell::new(vec![Err(e)]),
                seen: RefCell::new(Vec::new()),
            }
        }
    }

    impl EnrollTransport for FakeTransport {
        fn post(&self, url: &str, body: &EnrollRequest) -> Result<EnrollResponse, EnrollError> {
            self.seen.borrow_mut().push((url.to_string(), body.clone()));
            self.response.borrow_mut().remove(0)
        }
    }

    fn facts() -> MachineFacts {
        MachineFacts {
            machine_guid: "{4c4c4544-0037-4a10-8043-b2c04f483233}".into(),
            hw_fingerprint: crate::device::hw_fingerprint("a", "b", "c"),
            os_build: "10.0.22631".into(),
            capture_tier: Some("A".into()),
            agent_version: "0.1.0".into(),
        }
    }

    struct Reversing;
    impl spoolkey::KeyWrapper for Reversing {
        fn wrap(&self, p: &[u8]) -> spoolkey::Result<Vec<u8>> {
            let mut v = p.to_vec();
            v.reverse();
            Ok(v)
        }
        fn unwrap_blob(&self, b: &[u8]) -> spoolkey::Result<Vec<u8>> {
            let mut v = b.to_vec();
            v.reverse();
            Ok(v)
        }
        fn binds_to_machine(&self) -> bool {
            false
        }
    }

    #[test]
    fn a_successful_enrollment_writes_the_certificate_chain_identity_and_spool_key() {
        let dir = tempfile::tempdir().unwrap();
        let key = SoftwareDeviceKey::generate(dir.path()).unwrap();
        let t = FakeTransport::ok();
        let out = enroll(
            "https://api.example.com/",
            "  the-token  ",
            &facts(),
            &key,
            &t,
            &Reversing,
            dir.path(),
        )
        .unwrap();

        assert_eq!(out.identity.device_id, "1b4e28ba-2fa1-11d2-883f-0016d3cca427");
        assert_eq!(out.identity.not_after, "2027-09-03T00:00:00Z");
        assert_eq!(out.key_kind, KeyKind::Software);
        assert_eq!(out.spool_key.len(), 64);

        assert!(dir.path().join(CERT_FILE).exists());
        assert!(dir.path().join(CHAIN_FILE).exists());
        assert!(dir.path().join("identity.json").exists());
        assert!(spoolkey::key_path(dir.path()).exists());
        // The identity record is what marks the machine enrolled, and it reloads.
        assert_eq!(
            crate::device::load_identity(dir.path()).unwrap().unwrap(),
            out.identity
        );
        // The spool key unwraps to the same value the caller was handed.
        assert_eq!(spoolkey::resolve(dir.path(), &Reversing).unwrap(), out.spool_key);
    }

    #[test]
    fn the_request_carries_the_contracts_fields_and_only_the_csr_of_the_key() {
        let dir = tempfile::tempdir().unwrap();
        let key = SoftwareDeviceKey::generate(dir.path()).unwrap();
        let t = FakeTransport::ok();
        enroll("https://api.example.com", "tok", &facts(), &key, &t, &Reversing, dir.path())
            .unwrap();

        let (url, body) = t.seen.borrow()[0].clone();
        assert_eq!(url, "https://api.example.com/v1/devices/enroll");
        assert_eq!(body.enrollment_token, "tok");
        assert_eq!(body.capture_tier, "A");
        assert_eq!(body.machine_guid, facts().machine_guid);
        assert_eq!(body.hw_fingerprint.len(), 64);
        assert!(body.csr_pem.starts_with("-----BEGIN CERTIFICATE REQUEST-----"));

        // The one property this whole module exists for: no private key material on
        // the wire. The software key's own PEM file is right there in `dir`, so a
        // regression that serialised it would show up here.
        let key_pem = std::fs::read_to_string(key.path()).unwrap();
        let json = serde_json::to_string(&body).unwrap();
        assert!(!json.contains("PRIVATE KEY"), "the request carries a private key");
        for line in key_pem.lines().filter(|l| !l.starts_with("-----") && l.len() > 16) {
            assert!(!json.contains(line), "the request carries private key material");
        }
    }

    #[test]
    fn a_tier_c_machine_is_refused_before_the_token_is_spent() {
        // The gateway would answer `unsupported_tier` and consume nothing, but a round
        // trip we already know the answer to still costs an operator a fresh token if
        // anything about that changes.
        let dir = tempfile::tempdir().unwrap();
        let key = SoftwareDeviceKey::generate(dir.path()).unwrap();
        let t = FakeTransport::ok();
        for tier in [None, Some("C".to_string()), Some("".to_string())] {
            let f = MachineFacts { capture_tier: tier.clone(), ..facts() };
            assert!(matches!(
                enroll("https://api", "tok", &f, &key, &t, &Reversing, dir.path()),
                Err(EnrollError::UnsupportedTier(_))
            ));
        }
        assert!(t.seen.borrow().is_empty(), "nothing was sent");
    }

    #[test]
    fn an_empty_token_fails_before_anything_is_generated() {
        let dir = tempfile::tempdir().unwrap();
        let key = SoftwareDeviceKey::generate(dir.path()).unwrap();
        let t = FakeTransport::ok();
        assert!(matches!(
            enroll("https://api", "   ", &facts(), &key, &t, &Reversing, dir.path()),
            Err(EnrollError::NoToken)
        ));
        assert!(t.seen.borrow().is_empty());
    }

    #[test]
    fn a_rejected_enrollment_leaves_no_partial_credential_on_disk() {
        // Half a credential is worse than none: a certificate with no identity record
        // is ignored, but a machine that thinks it is enrolled and cannot present a
        // certificate spools forever without saying why.
        let dir = tempfile::tempdir().unwrap();
        let key = SoftwareDeviceKey::generate(dir.path()).unwrap();
        let t = FakeTransport::failing(EnrollError::TokenUnusable);
        assert!(matches!(
            enroll("https://api", "spent", &facts(), &key, &t, &Reversing, dir.path()),
            Err(EnrollError::TokenUnusable)
        ));
        assert!(!dir.path().join(CERT_FILE).exists());
        assert!(!dir.path().join("identity.json").exists());
        assert!(!spoolkey::key_path(dir.path()).exists());
        assert!(crate::device::load_identity(dir.path()).unwrap().is_none());
    }

    #[test]
    fn a_201_carrying_something_that_is_not_a_certificate_is_refused() {
        let dir = tempfile::tempdir().unwrap();
        let key = SoftwareDeviceKey::generate(dir.path()).unwrap();
        let t = FakeTransport {
            response: RefCell::new(vec![Ok(EnrollResponse {
                device_id: "d".into(),
                certificate_pem: "{\"code\":\"internal\"}".into(),
                ca_chain_pem: String::new(),
                not_after: "2027-09-03T00:00:00Z".into(),
            })]),
            seen: RefCell::new(Vec::new()),
        };
        assert!(matches!(
            enroll("https://api", "tok", &facts(), &key, &t, &Reversing, dir.path()),
            Err(EnrollError::BadCertificate)
        ));
        assert!(!dir.path().join(CERT_FILE).exists());
    }

    #[test]
    fn the_renewal_decision_follows_the_thirty_day_window_and_the_token() {
        let day = 24 * 3600 * 1000i64;
        let now = 1_800_000_000_000i64;
        let id = DeviceIdentity {
            device_id: "d".into(),
            machine_guid: "{g}".into(),
            hw_fingerprint: "f".into(),
            not_after: "2027-09-03T00:00:00Z".into(),
            cert_path: CERT_FILE.into(),
            ca_chain_path: CHAIN_FILE.into(),
        };
        assert_eq!(renewal_decision(None, None, now, true), RenewalDecision::NotEnrolled);
        assert_eq!(
            renewal_decision(Some(&id), Some(now + 90 * day), now, false),
            RenewalDecision::Current
        );
        assert_eq!(
            renewal_decision(Some(&id), Some(now + 10 * day), now, true),
            RenewalDecision::RenewNow
        );
        assert_eq!(
            renewal_decision(Some(&id), Some(now + 10 * day), now, false),
            RenewalDecision::RenewBlockedNoToken
        );
        // An expired certificate with no token is still blocked, not "current".
        assert_eq!(
            renewal_decision(Some(&id), Some(now - day), now, false),
            RenewalDecision::RenewBlockedNoToken
        );
        // An unparseable expiry means renew, never "valid forever".
        assert_eq!(
            renewal_decision(Some(&id), None, now, true),
            RenewalDecision::RenewNow
        );
    }

    #[test]
    fn the_renewal_token_is_read_trimmed_and_cleared_idempotently() {
        let dir = tempfile::tempdir().unwrap();
        assert_eq!(read_renewal_token(dir.path()), None);
        std::fs::write(dir.path().join(RENEWAL_TOKEN_FILE), b"  tok-123\n").unwrap();
        assert_eq!(read_renewal_token(dir.path()).as_deref(), Some("tok-123"));
        std::fs::write(dir.path().join(RENEWAL_TOKEN_FILE), b"   \n").unwrap();
        assert_eq!(read_renewal_token(dir.path()), None, "whitespace is not a token");

        clear_renewal_token(dir.path()).unwrap();
        clear_renewal_token(dir.path()).unwrap();
        assert_eq!(read_renewal_token(dir.path()), None);
    }

    fn transport_ok() -> FakeTransport {
        FakeTransport::ok()
    }

    #[test]
    fn an_unenrolled_machine_with_no_token_does_nothing_and_says_so() {
        // The normal state of a machine installed without ENROLLMENTTOKEN, and the
        // reason capture will not start on it.
        let dir = tempfile::tempdir().unwrap();
        let key = SoftwareDeviceKey::generate(dir.path()).unwrap();
        let t = transport_ok();
        assert_eq!(
            ensure_enrolled("https://api", &facts(), &key, &t, dir.path(), 0, None),
            EnsureOutcome::NotEnrolledNoToken
        );
        assert!(t.seen.borrow().is_empty(), "no token means no round trip");
    }

    /// Argument order shim so the tests read in the order a human thinks about it.
    fn ensure_enrolled(
        api_base: &str,
        facts: &MachineFacts,
        key: &dyn crate::devicekey::DeviceKey,
        transport: &dyn EnrollTransport,
        dir: &Path,
        now_ms: i64,
        token: Option<&str>,
    ) -> EnsureOutcome {
        super::ensure_enrolled(dir, api_base, facts, now_ms, token, key, transport, &Reversing)
    }

    #[test]
    fn a_token_on_an_unenrolled_machine_enrolls_it() {
        let dir = tempfile::tempdir().unwrap();
        let key = SoftwareDeviceKey::generate(dir.path()).unwrap();
        let t = transport_ok();
        let out = ensure_enrolled("https://api", &facts(), &key, &t, dir.path(), 0, Some("tok"));
        assert_eq!(
            out,
            EnsureOutcome::Enrolled {
                device_id: "1b4e28ba-2fa1-11d2-883f-0016d3cca427".into(),
                not_after: "2027-09-03T00:00:00Z".into(),
                renewed: false,
                key_kind: KeyKind::Software,
            }
        );
    }

    #[test]
    fn an_enrolled_machine_with_a_current_certificate_does_not_spend_a_token() {
        // A start-up that re-enrolled would burn a single-use token on every reboot
        // and mint a second device row for a machine that already has one.
        let dir = tempfile::tempdir().unwrap();
        let key = SoftwareDeviceKey::generate(dir.path()).unwrap();
        let t = transport_ok();
        ensure_enrolled("https://api", &facts(), &key, &t, dir.path(), 0, Some("tok"));
        let seen_before = t.seen.borrow().len();

        // Well inside the certificate's life.
        let now = not_after_ms("2027-09-03T00:00:00Z").unwrap() - 200 * 24 * 3600 * 1000;
        let out = ensure_enrolled("https://api", &facts(), &key, &t, dir.path(), now, Some("tok2"));
        assert!(matches!(out, EnsureOutcome::AlreadyEnrolled { .. }));
        assert_eq!(t.seen.borrow().len(), seen_before, "nothing was sent");
    }

    #[test]
    fn a_certificate_inside_the_renewal_window_renews_against_the_same_key() {
        let dir = tempfile::tempdir().unwrap();
        let key = SoftwareDeviceKey::generate(dir.path()).unwrap();
        let t = FakeTransport {
            response: RefCell::new(vec![
                Ok(EnrollResponse {
                    device_id: "d1".into(),
                    certificate_pem: "-----BEGIN CERTIFICATE-----\nA\n-----END CERTIFICATE-----\n".into(),
                    ca_chain_pem: String::new(),
                    not_after: "2027-09-03T00:00:00Z".into(),
                }),
                Ok(EnrollResponse {
                    device_id: "d1".into(),
                    certificate_pem: "-----BEGIN CERTIFICATE-----\nB\n-----END CERTIFICATE-----\n".into(),
                    ca_chain_pem: String::new(),
                    not_after: "2028-09-03T00:00:00Z".into(),
                }),
            ]),
            seen: RefCell::new(Vec::new()),
        };
        ensure_enrolled("https://api", &facts(), &key, &t, dir.path(), 0, Some("tok")).clone();

        // Ten days out: inside the 30-day window.
        let now = not_after_ms("2027-09-03T00:00:00Z").unwrap() - 10 * 24 * 3600 * 1000;
        let out = ensure_enrolled("https://api", &facts(), &key, &t, dir.path(), now, Some("tok2"));
        assert!(matches!(out, EnsureOutcome::Enrolled { renewed: true, .. }), "{out:?}");

        // Same key, so the machine keeps one identity across the renewal.
        let first = t.seen.borrow()[0].1.csr_pem.clone();
        let second = t.seen.borrow()[1].1.csr_pem.clone();
        let point = crate::devicekey::DeviceKey::public_point(&key).unwrap();
        for csr in [&first, &second] {
            let der = base64_body(csr);
            assert!(der.windows(65).any(|w| w == point), "the CSR re-certifies the same key");
        }
        // ...and the new certificate replaced the old one on disk.
        assert!(std::fs::read_to_string(dir.path().join(CERT_FILE)).unwrap().contains("\nB\n"));
    }

    fn base64_body(pem: &str) -> Vec<u8> {
        use base64::Engine as _;
        let body: String = pem
            .lines()
            .filter(|l| !l.starts_with("-----"))
            .collect();
        base64::engine::general_purpose::STANDARD.decode(body).unwrap()
    }

    #[test]
    fn a_certificate_inside_the_renewal_window_with_no_token_is_reported_not_dropped() {
        let dir = tempfile::tempdir().unwrap();
        let key = SoftwareDeviceKey::generate(dir.path()).unwrap();
        let t = transport_ok();
        ensure_enrolled("https://api", &facts(), &key, &t, dir.path(), 0, Some("tok"));
        let now = not_after_ms("2027-09-03T00:00:00Z").unwrap() - 10 * 24 * 3600 * 1000;
        assert_eq!(
            ensure_enrolled("https://api", &facts(), &key, &t, dir.path(), now, None),
            EnsureOutcome::RenewalBlockedNoToken { not_after: "2027-09-03T00:00:00Z".into() }
        );
    }

    #[test]
    fn a_failed_renewal_leaves_the_old_certificate_in_place() {
        // Which is the point of renewing 30 days early: a failure is not an outage.
        let dir = tempfile::tempdir().unwrap();
        let key = SoftwareDeviceKey::generate(dir.path()).unwrap();
        let t = FakeTransport {
            response: RefCell::new(vec![
                Ok(EnrollResponse {
                    device_id: "d1".into(),
                    certificate_pem: "-----BEGIN CERTIFICATE-----\nORIGINAL\n-----END CERTIFICATE-----\n".into(),
                    ca_chain_pem: String::new(),
                    not_after: "2027-09-03T00:00:00Z".into(),
                }),
                Err(EnrollError::TokenUnusable),
            ]),
            seen: RefCell::new(Vec::new()),
        };
        ensure_enrolled("https://api", &facts(), &key, &t, dir.path(), 0, Some("tok"));
        let now = not_after_ms("2027-09-03T00:00:00Z").unwrap() - 10 * 24 * 3600 * 1000;
        assert!(matches!(
            ensure_enrolled("https://api", &facts(), &key, &t, dir.path(), now, Some("spent")),
            EnsureOutcome::Failed(_)
        ));
        assert!(std::fs::read_to_string(dir.path().join(CERT_FILE)).unwrap().contains("ORIGINAL"));
        assert_eq!(
            crate::device::load_identity(dir.path()).unwrap().unwrap().not_after,
            "2027-09-03T00:00:00Z"
        );
    }

    #[test]
    fn an_expiry_that_cannot_be_parsed_means_renew_rather_than_trust() {
        assert_eq!(not_after_ms("2027-09-03T00:00:00Z"), Some(1_819_929_600_000));
        assert_eq!(not_after_ms("  2027-09-03T00:00:00Z  "), Some(1_819_929_600_000));
        assert_eq!(not_after_ms("next tuesday"), None);
        assert_eq!(not_after_ms(""), None);
    }
}
