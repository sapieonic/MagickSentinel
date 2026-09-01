//! Service recovery configuration (spec 6.1: "restart after 1st, 2nd, and subsequent
//! failures; reset count daily").
//!
//! The installer writes this into the SCM, but the service also asserts it on start.
//! Both are needed: an MSI repair, an in-place upgrade, or an administrator poking at
//! `services.msc` can drop the recovery actions, and a watchdog that does not restart
//! is a watchdog that is not there. Re-applying an identical configuration is a no-op,
//! so doing it on every start costs nothing.

/// Delay before each successive restart, in milliseconds. Three entries because
/// `SERVICE_FAILURE_ACTIONS` applies the last one to every subsequent failure.
pub const RESTART_DELAYS_MS: [u32; 3] = [60_000, 120_000, 300_000];

/// Seconds of trouble-free running after which the failure count returns to zero.
/// The spec says daily; 86 400 is that in the units `SERVICE_FAILURE_ACTIONS.dwResetPeriod`
/// wants (seconds, not milliseconds — mixing the two here silently gives a reset
/// period of a day and a half in milliseconds, i.e. never).
pub const RESET_PERIOD_SECONDS: u32 = 86_400;

/// Service identity, matching the installer.
pub const SERVICE_NAME: &str = "MagickVoiceSentinel";
pub const SERVICE_DISPLAY_NAME: &str = "MagickVoice Sentinel";

#[cfg(windows)]
pub use win::apply_recovery_actions;

#[cfg(windows)]
mod win {
    use super::*;
    use windows::core::PCWSTR;
    use windows::Win32::System::Services::{
        ChangeServiceConfig2W, OpenSCManagerW, OpenServiceW, SC_ACTION, SC_ACTION_RESTART,
        SERVICE_CHANGE_CONFIG, SERVICE_CONFIG_FAILURE_ACTIONS, SERVICE_FAILURE_ACTIONSW,
        SC_MANAGER_CONNECT,
    };

    fn wide(s: &str) -> Vec<u16> {
        s.encode_utf16().chain(std::iter::once(0)).collect()
    }

    /// Apply the recovery actions to our own service entry.
    ///
    /// Requires `SERVICE_CHANGE_CONFIG`, which LocalSystem has on its own service.
    pub fn apply_recovery_actions() -> windows::core::Result<()> {
        unsafe {
            let scm = OpenSCManagerW(PCWSTR::null(), PCWSTR::null(), SC_MANAGER_CONNECT)?;
            let name = wide(SERVICE_NAME);
            let svc = OpenServiceW(scm, PCWSTR(name.as_ptr()), SERVICE_CHANGE_CONFIG)?;

            let mut actions: Vec<SC_ACTION> = RESTART_DELAYS_MS
                .iter()
                .map(|&ms| SC_ACTION { Type: SC_ACTION_RESTART, Delay: ms })
                .collect();

            let mut failure = SERVICE_FAILURE_ACTIONSW {
                dwResetPeriod: RESET_PERIOD_SECONDS,
                // Null reboot message and command: we restart the service, never the
                // machine. A collections agent losing their desktop mid-call because
                // a watchdog rebooted it would be worse than the outage it fixed.
                lpRebootMsg: windows::core::PWSTR::null(),
                lpCommand: windows::core::PWSTR::null(),
                cActions: actions.len() as u32,
                lpsaActions: actions.as_mut_ptr(),
            };

            ChangeServiceConfig2W(
                svc,
                SERVICE_CONFIG_FAILURE_ACTIONS,
                Some(&mut failure as *mut _ as *mut std::ffi::c_void),
            )?;
            Ok(())
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn there_is_an_action_for_the_first_second_and_subsequent_failures() {
        assert_eq!(RESTART_DELAYS_MS.len(), 3);
        assert!(
            RESTART_DELAYS_MS.windows(2).all(|w| w[0] <= w[1]),
            "delays must not shorten as failures accumulate"
        );
    }

    #[test]
    fn the_reset_period_is_a_day_expressed_in_seconds() {
        // dwResetPeriod is in seconds. Passing milliseconds here compiles fine and
        // yields a reset period of 86 400 000 s ≈ 2.7 years, i.e. never.
        assert_eq!(RESET_PERIOD_SECONDS, 24 * 60 * 60);
    }
}
