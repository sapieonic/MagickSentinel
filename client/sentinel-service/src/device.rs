//! Device identity: the machine-scoped half of the two identities capture requires
//! (spec 7.2).
//!
//! It lives in the service crate rather than the agent because the certificate and
//! its key are machine state, not user state: they survive a shift change, they are
//! stored under `%PROGRAMDATA%` with an ACL the agent can read but not write, and the
//! service is the process that renews them. The agent links this module so both
//! processes present the same certificate over mTLS.
//!
//! The private key is generated in CNG and marked non-exportable, so it is never in
//! this process's memory and never in a file — only the certificate and the CNG key
//! *handle* are. On Windows the rustls client config is therefore built against the
//! platform key store; off Windows (development and CI) a PEM key file is accepted so
//! the uplink can be exercised against a local gateway.

use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::path::{Path, PathBuf};
use std::time::Duration;

/// Certificates are issued for a year and renewed with 30 days left (spec 7.2).
pub const RENEW_WHEN_REMAINING: Duration = Duration::from_secs(30 * 24 * 3600);

/// Enrollment state the service persists next to the certificate.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct DeviceIdentity {
    pub device_id: String,
    pub machine_guid: String,
    /// SHA-256 of machine GUID + baseboard serial + primary MAC.
    pub hw_fingerprint: String,
    /// RFC3339. Compared against the clock to decide renewal.
    pub not_after: String,
    pub cert_path: String,
    pub ca_chain_path: String,
}

/// Directory holding the device credential, under `%PROGRAMDATA%`.
pub fn credential_dir() -> PathBuf {
    if cfg!(windows) {
        let base = std::env::var("PROGRAMDATA").unwrap_or_else(|_| "C:\\ProgramData".into());
        PathBuf::from(base).join("MagickVoice").join("Sentinel").join("device")
    } else {
        PathBuf::from("/var/lib/magickvoice-sentinel/device")
    }
}

/// The hardware fingerprint the enrollment request carries.
///
/// Hashed rather than sent in the clear: the server only needs to detect that the
/// machine changed, and a raw MAC plus baseboard serial in a database is inventory
/// data we have no reason to hold. Fields are joined with a separator that cannot
/// occur in any of them, so `("ab","c")` and `("a","bc")` cannot collide.
pub fn hw_fingerprint(machine_guid: &str, baseboard_serial: &str, primary_mac: &str) -> String {
    let mut h = Sha256::new();
    h.update(machine_guid.trim().to_ascii_lowercase().as_bytes());
    h.update([0u8]);
    h.update(baseboard_serial.trim().to_ascii_lowercase().as_bytes());
    h.update([0u8]);
    h.update(primary_mac.trim().to_ascii_lowercase().replace([':', '-'], "").as_bytes());
    crate::update::hex_lower(&h.finalize())
}

/// Should the certificate be renewed?
///
/// `not_after` and `now` are both epoch milliseconds so the caller owns time parsing;
/// an unparseable `not_after` is treated as "renew", never as "valid forever".
pub fn needs_renewal(not_after_ms: Option<i64>, now_ms: i64) -> bool {
    match not_after_ms {
        None => true,
        Some(exp) => exp - now_ms <= RENEW_WHEN_REMAINING.as_millis() as i64,
    }
}

/// Load the identity record written at enrollment.
pub fn load_identity(dir: &Path) -> std::io::Result<Option<DeviceIdentity>> {
    let path = dir.join("identity.json");
    match std::fs::read_to_string(&path) {
        Ok(s) => Ok(serde_json::from_str(&s).ok()),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(e) => Err(e),
    }
}

pub fn save_identity(dir: &Path, id: &DeviceIdentity) -> std::io::Result<()> {
    std::fs::create_dir_all(dir)?;
    let path = dir.join("identity.json");
    // Write-then-rename: a torn identity.json after a power cut would make the
    // machine look unenrolled and mint a second device row for it.
    let tmp = dir.join("identity.json.tmp");
    std::fs::write(&tmp, serde_json::to_vec_pretty(id)?)?;
    std::fs::rename(&tmp, &path)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_fingerprint_is_stable_and_case_insensitive() {
        let a = hw_fingerprint("ABC-123", "BB-9", "00:11:22:33:44:55");
        let b = hw_fingerprint(" abc-123 ", "bb-9", "001122334455");
        assert_eq!(a, b, "casing, spacing and MAC punctuation must not change the hash");
        assert_eq!(a.len(), 64);
    }

    #[test]
    fn field_boundaries_cannot_be_shifted_to_collide() {
        assert_ne!(hw_fingerprint("ab", "c", "d"), hw_fingerprint("a", "bc", "d"));
    }

    #[test]
    fn the_fingerprint_does_not_contain_the_inputs() {
        let fp = hw_fingerprint("ABC-123", "BB-9", "00:11:22:33:44:55");
        assert!(!fp.contains("abc"), "the fingerprint is a hash, not an inventory record");
        assert!(fp.chars().all(|c| c.is_ascii_hexdigit()));
    }

    #[test]
    fn renewal_starts_thirty_days_out() {
        let day = 24 * 3600 * 1000i64;
        let now = 1_000_000_000_000i64;
        assert!(!needs_renewal(Some(now + 31 * day), now));
        assert!(needs_renewal(Some(now + 30 * day), now), "at exactly 30 days, renew");
        assert!(needs_renewal(Some(now + day), now));
        assert!(needs_renewal(Some(now - day), now), "already expired");
    }

    #[test]
    fn an_unknown_expiry_means_renew_not_trust() {
        assert!(needs_renewal(None, 0));
    }

    #[test]
    fn the_identity_record_survives_a_round_trip() {
        let dir = tempfile::tempdir().unwrap();
        let id = DeviceIdentity {
            device_id: "8f1c...".into(),
            machine_guid: "{GUID}".into(),
            hw_fingerprint: hw_fingerprint("a", "b", "c"),
            not_after: "2027-09-01T00:00:00Z".into(),
            cert_path: "device.crt".into(),
            ca_chain_path: "ca.crt".into(),
        };
        assert_eq!(load_identity(dir.path()).unwrap(), None);
        save_identity(dir.path(), &id).unwrap();
        assert_eq!(load_identity(dir.path()).unwrap(), Some(id));
        assert!(!dir.path().join("identity.json.tmp").exists(), "the temp file is renamed away");
    }
}
