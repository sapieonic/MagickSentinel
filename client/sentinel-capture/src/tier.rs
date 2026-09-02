//! Capture tier detection (spec section 3).
//!
//! `AUDIOCLIENT_ACTIVATION_TYPE_PROCESS_LOOPBACK`, reached through
//! `ActivateAudioInterfaceAsync`, requires **Windows 11 or Server 2022 or later**.
//! Windows 10 client builds top out at 19045 and do not support it. Any prior
//! guidance suggesting "build 20348+" conflates the Server 2022 build number with a
//! Windows 10 build; it is wrong, and taking it at face value would ship a client
//! that silently fails to arm on every Windows 10 desktop on the floor. Windows 10 is
//! always tier B.
//!
//! Detection runs at install time **and** on every service start, because in-place
//! upgrades change the answer.

use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum CaptureTier {
    /// Process loopback. Windows 11 (22000+) and Server 2022+.
    A,
    /// Endpoint loopback with a pinned device plus foreign-audio suppression.
    /// Windows 10 1903 (18362) – 22H2 (19045).
    B,
}

impl CaptureTier {
    pub fn as_str(self) -> &'static str {
        match self {
            CaptureTier::A => "A",
            CaptureTier::B => "B",
        }
    }
}

/// The outcome of tier detection, including the unsupported case.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum TierDetection {
    Supported { tier: CaptureTier, os_build: String },
    /// Tier C: the installer MUST block, and a running service MUST NOT capture.
    Unsupported { os_build: String, reason: String },
}

impl TierDetection {
    pub fn tier(&self) -> Option<CaptureTier> {
        match self {
            TierDetection::Supported { tier, .. } => Some(*tier),
            TierDetection::Unsupported { .. } => None,
        }
    }

    pub fn os_build(&self) -> &str {
        match self {
            TierDetection::Supported { os_build, .. } => os_build,
            TierDetection::Unsupported { os_build, .. } => os_build,
        }
    }
}

/// Processor architecture, as reported by the OS.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Arch {
    X64,
    X86,
    Arm64,
    Other,
}

/// Classify an OS build into a capture tier.
///
/// Split out from the Windows API call so the whole support matrix is testable on any
/// platform. `is_server` distinguishes Server 2022 (build 20348, tier A) from a
/// Windows 10 client build in the same numeric neighbourhood.
pub fn classify(major: u32, minor: u32, build: u32, is_server: bool, arch: Arch) -> TierDetection {
    let os_build = format!("{major}.{minor}.{build}");

    if !matches!(arch, Arch::X64) {
        return TierDetection::Unsupported {
            os_build,
            reason: format!("unsupported architecture {arch:?}; x64 only"),
        };
    }
    if major < 10 {
        return TierDetection::Unsupported {
            os_build,
            reason: "Windows 8.1 and earlier are not supported".into(),
        };
    }

    // Server 2022 and later: process loopback is available.
    if is_server {
        return if build >= 20348 {
            TierDetection::Supported { tier: CaptureTier::A, os_build }
        } else {
            TierDetection::Unsupported {
                os_build,
                reason: "Windows Server before 2022 has no process loopback".into(),
            }
        };
    }

    // Client builds. 22000 is the Windows 11 floor; 20348 is a *server* build number
    // and must never be used as a client threshold.
    if build >= 22000 {
        TierDetection::Supported { tier: CaptureTier::A, os_build }
    } else if build >= 18362 {
        TierDetection::Supported { tier: CaptureTier::B, os_build }
    } else {
        TierDetection::Unsupported {
            os_build,
            reason: "Windows 10 before 1903 (build 18362) is not supported".into(),
        }
    }
}

/// Detect the tier of the machine we are running on.
#[cfg(windows)]
pub fn detect() -> TierDetection {
    crate::windows::os::detect_tier()
}

/// Off-Windows this is a development stub. The shipping client is Windows-only; this
/// exists so the rest of the workspace builds and tests in CI on Linux.
#[cfg(not(windows))]
pub fn detect() -> TierDetection {
    TierDetection::Unsupported {
        os_build: "0.0.0".into(),
        reason: "not running on Windows".into(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tier_of(build: u32, is_server: bool) -> Option<CaptureTier> {
        classify(10, 0, build, is_server, Arch::X64).tier()
    }

    #[test]
    fn windows_eleven_is_tier_a() {
        assert_eq!(tier_of(22000, false), Some(CaptureTier::A));
        assert_eq!(tier_of(22631, false), Some(CaptureTier::A));
        assert_eq!(tier_of(26100, false), Some(CaptureTier::A));
    }

    #[test]
    fn every_windows_ten_client_build_is_tier_b() {
        for build in [18362, 19041, 19044, 19045] {
            assert_eq!(tier_of(build, false), Some(CaptureTier::B), "build {build}");
        }
    }

    #[test]
    fn the_server_2022_build_number_must_not_promote_a_windows_10_client() {
        // 20348 is Server 2022. As a *client* build number it is between 19045 and
        // 22000, which does not exist in the wild, but a threshold of "20348+" would
        // wrongly classify anything above it as tier A. Guard the boundary.
        assert_eq!(tier_of(20348, false), Some(CaptureTier::B));
        assert_eq!(tier_of(21390, false), Some(CaptureTier::B));
        assert_eq!(tier_of(20348, true), Some(CaptureTier::A), "Server 2022 is tier A");
    }

    #[test]
    fn old_windows_is_blocked_not_degraded() {
        assert!(matches!(
            classify(10, 0, 17763, false, Arch::X64),
            TierDetection::Unsupported { .. }
        ));
        assert!(matches!(
            classify(6, 3, 9600, false, Arch::X64),
            TierDetection::Unsupported { .. }
        ));
    }

    #[test]
    fn non_x64_architectures_are_blocked() {
        for arch in [Arch::X86, Arch::Arm64, Arch::Other] {
            assert!(
                matches!(classify(10, 0, 22631, false, arch), TierDetection::Unsupported { .. }),
                "{arch:?} should be blocked"
            );
        }
    }

    #[test]
    fn os_build_is_reported_even_when_unsupported() {
        let d = classify(10, 0, 17763, false, Arch::X64);
        assert_eq!(d.os_build(), "10.0.17763");
    }
}
