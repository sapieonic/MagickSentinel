//! OS build and architecture detection.
//!
//! `GetVersionEx` lies to unmanifested processes, and the MSI cannot be relied on to
//! carry a compatibility manifest into every deployment tool a customer uses. We read
//! the build directly from the registry instead, which no shim rewrites.

use crate::tier::{classify, Arch, TierDetection};
use windows::core::w;
use windows::Win32::System::Registry::{
    RegCloseKey, RegGetValueW, RegOpenKeyExW, HKEY, HKEY_LOCAL_MACHINE, KEY_READ, RRF_RT_REG_DWORD,
    RRF_RT_REG_SZ,
};
use windows::Win32::System::SystemInformation::{
    GetNativeSystemInfo, SYSTEM_INFO, PROCESSOR_ARCHITECTURE_AMD64, PROCESSOR_ARCHITECTURE_ARM64,
    PROCESSOR_ARCHITECTURE_INTEL,
};

const CURRENT_VERSION: windows::core::PCWSTR =
    w!(r"SOFTWARE\Microsoft\Windows NT\CurrentVersion");

pub fn detect_tier() -> TierDetection {
    let build = registry_build_number().unwrap_or(0);
    let (major, minor) = registry_major_minor().unwrap_or((0, 0));
    let is_server = is_server_sku();
    classify(major, minor, build, is_server, native_arch())
}

fn native_arch() -> Arch {
    let mut info = SYSTEM_INFO::default();
    unsafe { GetNativeSystemInfo(&mut info) };
    match unsafe { info.Anonymous.Anonymous.wProcessorArchitecture } {
        PROCESSOR_ARCHITECTURE_AMD64 => Arch::X64,
        PROCESSOR_ARCHITECTURE_INTEL => Arch::X86,
        PROCESSOR_ARCHITECTURE_ARM64 => Arch::Arm64,
        _ => Arch::Other,
    }
}

fn registry_build_number() -> Option<u32> {
    // UBR is the patch level; CurrentBuildNumber is what the support matrix keys on.
    reg_dword(CURRENT_VERSION, w!("CurrentBuildNumber"))
        .or_else(|| reg_string(CURRENT_VERSION, w!("CurrentBuildNumber"))?.parse().ok())
}

fn registry_major_minor() -> Option<(u32, u32)> {
    Some((
        reg_dword(CURRENT_VERSION, w!("CurrentMajorVersionNumber"))?,
        reg_dword(CURRENT_VERSION, w!("CurrentMinorVersionNumber")).unwrap_or(0),
    ))
}

/// Server SKUs report an InstallationType of "Server" or "Server Core". This is what
/// separates Server 2022 (build 20348, tier A) from a client build in the same
/// numeric range, which is tier B.
fn is_server_sku() -> bool {
    matches!(
        reg_string(CURRENT_VERSION, w!("InstallationType")).as_deref(),
        Some("Server") | Some("Server Core")
    )
}

fn reg_dword(subkey: windows::core::PCWSTR, value: windows::core::PCWSTR) -> Option<u32> {
    let mut out: u32 = 0;
    let mut size = std::mem::size_of::<u32>() as u32;
    let ok = unsafe {
        RegGetValueW(
            HKEY_LOCAL_MACHINE,
            subkey,
            value,
            RRF_RT_REG_DWORD,
            None,
            Some(&mut out as *mut u32 as *mut _),
            Some(&mut size),
        )
    };
    ok.is_ok().then_some(out)
}

fn reg_string(subkey: windows::core::PCWSTR, value: windows::core::PCWSTR) -> Option<String> {
    let mut size: u32 = 0;
    unsafe {
        RegGetValueW(HKEY_LOCAL_MACHINE, subkey, value, RRF_RT_REG_SZ, None, None, Some(&mut size))
            .ok()
            .ok()?;
    }
    let mut buf = vec![0u16; (size as usize / 2) + 1];
    unsafe {
        RegGetValueW(
            HKEY_LOCAL_MACHINE,
            subkey,
            value,
            RRF_RT_REG_SZ,
            None,
            Some(buf.as_mut_ptr() as *mut _),
            Some(&mut size),
        )
        .ok()
        .ok()?;
    }
    let s = String::from_utf16_lossy(&buf);
    Some(s.trim_end_matches('\0').to_string())
}

/// Open and immediately close the key, as a cheap probe that the hive is readable.
pub fn registry_readable() -> bool {
    let mut key = HKEY::default();
    let ok = unsafe {
        RegOpenKeyExW(HKEY_LOCAL_MACHINE, CURRENT_VERSION, 0, KEY_READ, &mut key)
    };
    if ok.is_ok() {
        unsafe { let _ = RegCloseKey(key); };
        true
    } else {
        false
    }
}
