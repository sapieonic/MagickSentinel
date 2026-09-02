//! Windows implementations of [`crate::CaptureSource`].
//!
//! Everything under this module is `cfg(windows)`. It is the only place in the
//! workspace that touches COM, and it is deliberately thin: the state machine, the
//! spool and the uplink are all platform-neutral and tested elsewhere, so the code
//! that cannot run in CI is kept as small as it can be.

pub mod endpoint_loopback;
pub mod notification;
pub mod os;
pub mod process_loopback;
pub mod session;

use windows::Win32::System::Com::{
    CoInitializeEx, CoUninitialize, COINIT_MULTITHREADED,
};

/// RAII guard for per-thread COM initialisation.
///
/// Every thread that touches WASAPI needs this, and every one of them must
/// uninitialise on the way out or the audio engine leaks endpoint references and the
/// next `IMMDeviceEnumerator` on that thread fails in a way that reads like a driver
/// bug.
pub struct ComGuard;

impl ComGuard {
    pub fn new() -> windows::core::Result<Self> {
        unsafe { CoInitializeEx(None, COINIT_MULTITHREADED).ok()? };
        Ok(ComGuard)
    }
}

impl Drop for ComGuard {
    fn drop(&mut self) {
        unsafe { CoUninitialize() };
    }
}
