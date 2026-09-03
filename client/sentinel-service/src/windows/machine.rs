//! The facts about this machine that the enrollment request carries, and the
//! enrollment token itself.
//!
//! All of it is registry reads. That is a deliberate choice over WMI: a WMI query needs
//! COM initialised on the calling thread, can block for seconds on a machine whose
//! repository needs rebuilding, and is one of the first things an aggressive EDR
//! configuration restricts — and this code runs at service start, before the watchdog
//! has launched anything, on a floor whose endpoint protection product is already
//! suspicious of us (`docs/security.md` section 9).
//!
//! **Nothing here is exercised by any test.** There is no Windows runner in this
//! repository; the cross-compile check is the only thing keeping it from rotting.

use windows::core::{w, PCWSTR};
use windows::Win32::System::Registry::{
    RegDeleteKeyValueW, RegGetValueW, HKEY, HKEY_LOCAL_MACHINE, RRF_RT_REG_SZ,
};

/// Read a `REG_SZ` value, or `None` if it is absent or not a string.
fn read_string(root: HKEY, subkey: PCWSTR, name: PCWSTR) -> Option<String> {
    // 512 UTF-16 units is comfortably more than any value read here; a longer one is
    // truncated rather than growing a buffer, because none of these fields are
    // meaningful at that length and a machine reporting one is not a machine to
    // allocate against.
    let mut buf = [0u16; 512];
    let mut size = (buf.len() * 2) as u32;
    unsafe {
        RegGetValueW(
            root,
            subkey,
            name,
            RRF_RT_REG_SZ,
            None,
            Some(buf.as_mut_ptr() as *mut _),
            Some(&mut size),
        )
        .ok()
        .ok()?;
    }
    // `size` is bytes and includes the terminating NUL.
    let chars = (size as usize / 2).saturating_sub(1);
    let s = String::from_utf16_lossy(&buf[..chars.min(buf.len())]);
    let s = s.trim().to_string();
    if s.is_empty() {
        None
    } else {
        Some(s)
    }
}

/// The Windows machine GUID.
///
/// `HKLM\SOFTWARE\Microsoft\Cryptography\MachineGuid`. Written at OS install and
/// stable for the life of the installation, which is what the gateway's
/// `machine_guid` column wants: a re-image should look like a new machine, and a
/// reboot should not.
pub fn machine_guid() -> Option<String> {
    read_string(
        HKEY_LOCAL_MACHINE,
        w!(r"SOFTWARE\Microsoft\Cryptography"),
        w!("MachineGuid"),
    )
}

/// The baseboard serial number, from the BIOS data the kernel publishes.
///
/// `HKLM\HARDWARE\DESCRIPTION\System\BIOS\BaseBoardSerialNumber` — the same SMBIOS
/// field WMI's `Win32_BaseBoard.SerialNumber` reports, without the WMI round trip.
/// Frequently absent or a placeholder (`"Default string"`, `"To be filled by O.E.M."`)
/// on desktop hardware, which is fine: it is one input to a hash whose only job is to
/// change when the machine does.
pub fn baseboard_serial() -> Option<String> {
    read_string(
        HKEY_LOCAL_MACHINE,
        w!(r"HARDWARE\DESCRIPTION\System\BIOS"),
        w!("BaseBoardSerialNumber"),
    )
    .filter(|s| !is_oem_placeholder(s))
}

/// OEMs ship these strings in the serial field on a large fraction of desktop
/// hardware. Treated as absent, because a fingerprint that is identical across every
/// machine of one model is worse than one field short.
fn is_oem_placeholder(s: &str) -> bool {
    let lower = s.to_ascii_lowercase();
    matches!(
        lower.as_str(),
        "default string"
            | "to be filled by o.e.m."
            | "to be filled by oem"
            | "none"
            | "not applicable"
            | "system serial number"
            | "0"
    )
}

/// The MAC address input to the hardware fingerprint. **Deliberately always empty.**
///
/// `device::hw_fingerprint` takes a primary MAC as its third input, and this does not
/// supply one. That is a decision rather than an omission:
///
/// * A collections desktop has several adapters — the NIC, a dock's NIC, a VPN
///   adapter, Hyper-V's virtual switch, whatever the softphone's headset dongle
///   presents — and "primary" is not a well-defined choice among them. Whichever rule
///   is picked, plugging in a dock changes the answer.
/// * A changed fingerprint is not cosmetic. The gateway answers `409` when a
///   `machine_guid` re-enrolls with a different fingerprint
///   (`contracts/openapi.yaml`), which is exactly the alarm it should raise for a
///   cloned disk — and exactly the wrong thing to raise because an agent moved desks.
/// * `GetAdaptersAddresses` would also mean enabling
///   `Win32_NetworkManagement_IpHelper` for one field, on the start-up path, in a
///   process an EDR product is already watching.
///
/// The machine GUID plus the baseboard serial already changes on a re-image and on a
/// motherboard swap, which are the two events the fingerprint exists to detect. If a
/// MAC is ever wanted, the argument above has to be answered first.
pub fn primary_mac() -> String {
    String::new()
}

/// Everything the enrollment request says about this machine.
pub fn machine_facts(capture_tier: Option<String>, os_build: String) -> crate::enroll::MachineFacts {
    let guid = machine_guid().unwrap_or_default();
    let serial = baseboard_serial().unwrap_or_default();
    crate::enroll::MachineFacts {
        hw_fingerprint: crate::device::hw_fingerprint(&guid, &serial, &primary_mac()),
        machine_guid: guid,
        os_build,
        capture_tier,
        agent_version: crate::VERSION.to_string(),
    }
}

/// Registry key the installer writes the single-use enrollment token into.
///
/// Not the command line of a custom action, which every user on the machine can read
/// out of the process list for as long as it runs, and not the main
/// `SOFTWARE\MagickVoice\Sentinel` key, which `Users` can read. A separate key with an
/// ACL of SYSTEM and Administrators only, so that between the MSI writing it and the
/// service consuming it the token is not readable by the agents on the floor.
///
/// The value is deleted by [`take_enrollment_token`] the moment the exchange succeeds.
/// It is single-use server-side anyway — `enroll.go` consumes it atomically before
/// signing — so a leftover value is not a second certificate, but it is a credential
/// sitting on disk for no reason.
pub const ENROLLMENT_KEY: PCWSTR = w!(r"SOFTWARE\MagickVoice\Sentinel\Enrollment");

/// Read the enrollment token the installer left, if any.
pub fn enrollment_token() -> Option<String> {
    read_string(HKEY_LOCAL_MACHINE, ENROLLMENT_KEY, w!("Token"))
}

/// Delete the enrollment token value.
///
/// Called **after** a successful exchange, never before: deleting first would leave a
/// machine whose enrollment failed for a transient reason — the gateway answering
/// `503 no_ca`, say, which is the current state of the production gateway — with no
/// token and no way to retry without an operator minting another one.
pub fn clear_enrollment_token() {
    unsafe {
        // A missing value is the expected case on every start after the first, so the
        // result is ignored rather than logged.
        let _ = RegDeleteKeyValueW(HKEY_LOCAL_MACHINE, ENROLLMENT_KEY, w!("Token"));
    }
}
