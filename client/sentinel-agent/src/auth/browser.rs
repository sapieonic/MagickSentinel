//! Opening the system default browser (spec 7.3 step 4).
//!
//! `ShellExecuteW` with the `open` verb hands the URL to whatever the user's profile
//! has registered for `https`, which is the point: corporate IdPs block embedded
//! webviews, and the user's own browser is where their SSO session, their managed
//! browser policy and their hardware MFA already live.

/// Anything that can put a URL in front of the user. A trait so the sign-in flow can
/// be exercised without a browser.
pub trait BrowserLauncher: Send + Sync {
    fn open(&self, url: &str) -> anyhow::Result<()>;
}

/// Records the URLs it was asked to open. Used by tests and by `--headless`.
#[derive(Debug, Default)]
pub struct RecordingLauncher {
    pub opened: std::sync::Mutex<Vec<String>>,
}

impl BrowserLauncher for RecordingLauncher {
    fn open(&self, url: &str) -> anyhow::Result<()> {
        self.opened.lock().unwrap().push(url.to_string());
        Ok(())
    }
}

#[cfg(windows)]
pub use win::SystemBrowser;

#[cfg(windows)]
mod win {
    use super::BrowserLauncher;
    use windows::core::{HSTRING, PCWSTR};
    use windows::Win32::UI::Shell::ShellExecuteW;
    use windows::Win32::UI::WindowsAndMessaging::SW_SHOWNORMAL;

    pub struct SystemBrowser;

    impl BrowserLauncher for SystemBrowser {
        fn open(&self, url: &str) -> anyhow::Result<()> {
            // Refuse anything that is not an https URL. `ShellExecuteW` will happily
            // launch a local executable or a `file:` path, so passing it a string that
            // ultimately came from configuration without checking the scheme turns a
            // config edit into code execution.
            if !url.starts_with("https://") {
                anyhow::bail!("refusing to open a non-https URL in the browser");
            }
            let wide = HSTRING::from(url);
            let verb = HSTRING::from("open");
            let result = unsafe {
                ShellExecuteW(
                    None,
                    PCWSTR(verb.as_ptr()),
                    PCWSTR(wide.as_ptr()),
                    PCWSTR::null(),
                    PCWSTR::null(),
                    SW_SHOWNORMAL,
                )
            };
            // ShellExecuteW's HINSTANCE return is an error code disguised as a
            // handle: values of 32 or less mean failure. It does not set last-error
            // in the usual way, so checking `GetLastError` here reports nothing.
            if result.0 as usize <= 32 {
                anyhow::bail!("the system browser could not be opened (code {})", result.0 as usize);
            }
            Ok(())
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_recording_launcher_captures_what_it_was_asked_to_open() {
        let l = RecordingLauncher::default();
        l.open("https://idp.example.com/auth?x=1").unwrap();
        assert_eq!(l.opened.lock().unwrap().as_slice(), ["https://idp.example.com/auth?x=1"]);
    }
}
