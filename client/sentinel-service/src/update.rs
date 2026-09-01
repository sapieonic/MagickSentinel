//! Update staging.
//!
//! The service downloads an MSI into a staging directory, verifies its SHA-256
//! against the manifest, and hands it to `msiexec` at a moment the agent is not in a
//! call. Nothing is applied here without a hash match: an unverified MSI executed by
//! LocalSystem is a remote-code-execution path straight through the bank's security
//! review.
//!
//! Version comparison and the "is it safe to apply now" decision are pure and tested;
//! the download and the `msiexec` invocation are not, because there is nothing here
//! to test them against.

use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::path::{Path, PathBuf};

/// An update the server is offering, as published in the update manifest.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct UpdateManifest {
    pub version: String,
    pub msi_url: String,
    /// Lowercase hex SHA-256 of the MSI.
    pub sha256: String,
    /// Set by an operator rolling out a fix that cannot wait for an idle moment.
    #[serde(default)]
    pub mandatory: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ApplyDecision {
    /// Run `msiexec` now.
    Apply,
    /// Hold: applying would interrupt a call.
    DeferInCall,
    /// Nothing staged, or the staged build is not newer.
    Nothing,
}

/// A dotted numeric version, compared field by field.
///
/// Deliberately not a general semver parser: our versions are `major.minor.patch`
/// produced by the build, and string comparison would rank `0.10.0` below `0.9.0`.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord)]
pub struct Version(pub Vec<u64>);

impl Version {
    pub fn parse(s: &str) -> Option<Version> {
        let parts: Vec<u64> = s
            .split('.')
            .map(|p| p.trim().parse::<u64>().ok())
            .collect::<Option<Vec<_>>>()?;
        if parts.is_empty() {
            return None;
        }
        // Pad so `0.1` and `0.1.0` compare equal rather than by length.
        let mut parts = parts;
        while parts.len() < 3 {
            parts.push(0);
        }
        Some(Version(parts))
    }
}

/// Should the staged update be applied right now?
pub fn decide(
    current: &str,
    staged: Option<&str>,
    agent_in_call: bool,
    mandatory: bool,
) -> ApplyDecision {
    let (Some(staged), Some(current)) = (staged.and_then(Version::parse), Version::parse(current))
    else {
        return ApplyDecision::Nothing;
    };
    if staged <= current {
        return ApplyDecision::Nothing;
    }
    // A mandatory update still waits for the call to end. Cutting a live recording is
    // data loss in a compliance product, and no fix is worth a hole in the evidence.
    if agent_in_call {
        return ApplyDecision::DeferInCall;
    }
    let _ = mandatory;
    ApplyDecision::Apply
}

/// Verify a staged file against the manifest hash.
pub fn verify_sha256(path: &Path, expected_hex: &str) -> std::io::Result<bool> {
    let bytes = std::fs::read(path)?;
    let digest = Sha256::digest(&bytes);
    Ok(hex_lower(&digest) == expected_hex.trim().to_ascii_lowercase())
}

pub fn hex_lower(bytes: &[u8]) -> String {
    let mut s = String::with_capacity(bytes.len() * 2);
    for b in bytes {
        use std::fmt::Write;
        let _ = write!(s, "{b:02x}");
    }
    s
}

/// Where a version's MSI is staged.
pub fn staging_path(root: &Path, version: &str) -> PathBuf {
    // The version is interpolated into a path, so it must not be able to escape the
    // staging directory. The manifest comes from the server over TLS, but "trusted
    // input" is exactly how directory traversal gets shipped.
    let safe: String = version
        .chars()
        .filter(|c| c.is_ascii_alphanumeric() || *c == '.' || *c == '-')
        .collect();
    root.join(format!("SentinelAgent-{safe}.msi"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn versions_compare_numerically_not_lexically() {
        assert!(Version::parse("0.10.0") > Version::parse("0.9.0"));
        assert!(Version::parse("1.0.0") > Version::parse("0.99.99"));
        assert_eq!(Version::parse("0.1"), Version::parse("0.1.0"));
        assert_eq!(Version::parse("not-a-version"), None);
        assert_eq!(Version::parse(""), None);
    }

    #[test]
    fn an_older_or_equal_staged_build_is_never_applied() {
        assert_eq!(decide("0.2.0", Some("0.1.9"), false, false), ApplyDecision::Nothing);
        assert_eq!(decide("0.2.0", Some("0.2.0"), false, false), ApplyDecision::Nothing);
        assert_eq!(decide("0.2.0", None, false, false), ApplyDecision::Nothing);
    }

    #[test]
    fn an_update_waits_for_the_call_to_end_even_when_mandatory() {
        assert_eq!(decide("0.1.0", Some("0.2.0"), true, false), ApplyDecision::DeferInCall);
        assert_eq!(decide("0.1.0", Some("0.2.0"), true, true), ApplyDecision::DeferInCall);
        assert_eq!(decide("0.1.0", Some("0.2.0"), false, false), ApplyDecision::Apply);
    }

    #[test]
    fn a_hash_mismatch_fails_verification() {
        let dir = tempfile::tempdir().unwrap();
        let f = dir.path().join("x.msi");
        std::fs::write(&f, b"pretend msi").unwrap();
        let real = hex_lower(&Sha256::digest(b"pretend msi"));
        assert!(verify_sha256(&f, &real).unwrap());
        assert!(verify_sha256(&f, &real.to_uppercase()).unwrap(), "hex case must not matter");
        assert!(!verify_sha256(&f, &"0".repeat(64)).unwrap());
    }

    #[test]
    fn a_version_string_cannot_escape_the_staging_directory() {
        let root = Path::new("/staging");
        let p = staging_path(root, "../../windows/system32/evil");
        assert_eq!(p.parent().unwrap(), root);
        assert!(!p.to_string_lossy().contains(".."));
    }
}
