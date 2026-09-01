//! Launching `SentinelAgent.exe` into an interactive session.
//!
//! This is the reason the client is two processes. The service runs as LocalSystem in
//! session 0, where there is no audio endpoint and no desktop; the agent has to run
//! in the user's session to reach either. `CreateProcessAsUserW` with a token from
//! `WTSQueryUserToken` is the supported way to cross that boundary — and notably not
//! a `Run` key, which the user can delete from `regedit` in ten seconds.
//!
//! Details that are easy to get wrong here, each of which fails in a way that is not
//! obviously about the thing you got wrong:
//!
//! * **`lpDesktop` must be `winsta0\default`.** Inherit the service's `STARTUPINFO`
//!   and the process starts on session 0's invisible window station: it runs, it logs
//!   normally, and no window ever appears.
//! * **The environment block must come from `CreateEnvironmentBlock`.** Inheriting
//!   the service's environment gives the agent SYSTEM's `%APPDATA%` and
//!   `%LOCALAPPDATA%`, so the widget's saved position and any per-user state land in
//!   `C:\Windows\system32\config\systemprofile`.
//! * **`CREATE_UNICODE_ENVIRONMENT` is mandatory** when passing that block;
//!   `CreateEnvironmentBlock` always produces UTF-16 and without the flag Windows
//!   reads it as ANSI, which truncates at the first NUL and yields an empty
//!   environment.
//! * **`WTSQueryUserToken` requires `SE_TCB_NAME`**, which LocalSystem has and an
//!   administrator account does not — so this path cannot be tested by running the
//!   service binary interactively as an admin.

use windows::core::PWSTR;
use windows::Win32::Foundation::{CloseHandle, HANDLE};
use windows::Win32::System::Environment::{CreateEnvironmentBlock, DestroyEnvironmentBlock};
use windows::Win32::System::RemoteDesktop::WTSQueryUserToken;
use windows::Win32::System::Threading::{
    CreateProcessAsUserW, GetExitCodeProcess, WaitForSingleObject, CREATE_NEW_CONSOLE,
    CREATE_UNICODE_ENVIRONMENT, NORMAL_PRIORITY_CLASS, PROCESS_INFORMATION, STARTUPINFOW,
};
use windows::Win32::Foundation::{STILL_ACTIVE, WAIT_OBJECT_0};

/// A launched agent process. Closing the handles is the caller's job via `Drop`.
pub struct AgentProcess {
    process: HANDLE,
    thread: HANDLE,
    pub session_id: u32,
    pub pid: u32,
}

impl AgentProcess {
    /// Has the process exited?
    pub fn has_exited(&self) -> bool {
        let mut code = 0u32;
        unsafe { GetExitCodeProcess(self.process, &mut code) }.is_ok() && code != STILL_ACTIVE.0 as u32
    }

    /// Block until the process exits, or until `timeout_ms` elapses.
    pub fn wait(&self, timeout_ms: u32) -> bool {
        unsafe { WaitForSingleObject(self.process, timeout_ms) == WAIT_OBJECT_0 }
    }
}

impl Drop for AgentProcess {
    fn drop(&mut self) {
        unsafe {
            let _ = CloseHandle(self.thread);
            let _ = CloseHandle(self.process);
        }
    }
}

/// Owns the token from `WTSQueryUserToken`.
struct UserToken(HANDLE);

impl Drop for UserToken {
    fn drop(&mut self) {
        unsafe {
            let _ = CloseHandle(self.0);
        }
    }
}

/// Owns the block from `CreateEnvironmentBlock`, which has its own deallocator.
struct EnvBlock(*mut std::ffi::c_void);

impl Drop for EnvBlock {
    fn drop(&mut self) {
        if !self.0.is_null() {
            unsafe {
                let _ = DestroyEnvironmentBlock(self.0);
            }
        }
    }
}

/// Launch the agent in `session_id`.
pub fn launch_agent(session_id: u32, exe_path: &str) -> windows::core::Result<AgentProcess> {
    unsafe {
        let mut token = HANDLE::default();
        WTSQueryUserToken(session_id, &mut token)?;
        let token = UserToken(token);

        let mut env = std::ptr::null_mut();
        // `false`: do not inherit the service's environment. The point of the block
        // is the user's profile variables, and merging SYSTEM's in would reintroduce
        // exactly the wrong %APPDATA%.
        CreateEnvironmentBlock(&mut env, Some(token.0), false)?;
        let env = EnvBlock(env);

        // CreateProcessAsUserW may modify the command line in place, so it must be a
        // writable buffer, not a literal.
        let mut cmdline: Vec<u16> = format!("\"{exe_path}\"")
            .encode_utf16()
            .chain(std::iter::once(0))
            .collect();
        let mut desktop: Vec<u16> = r"winsta0\default"
            .encode_utf16()
            .chain(std::iter::once(0))
            .collect();

        let mut si = STARTUPINFOW {
            cb: std::mem::size_of::<STARTUPINFOW>() as u32,
            lpDesktop: PWSTR(desktop.as_mut_ptr()),
            ..Default::default()
        };
        let mut pi = PROCESS_INFORMATION::default();

        CreateProcessAsUserW(
            Some(token.0),
            None,
            Some(PWSTR(cmdline.as_mut_ptr())),
            None,
            None,
            false,
            CREATE_UNICODE_ENVIRONMENT | CREATE_NEW_CONSOLE | NORMAL_PRIORITY_CLASS,
            Some(env.0),
            None,
            &si,
            &mut pi,
        )?;
        // Touch `si` after the call so the desktop buffer provably outlives it.
        debug_assert!(!si.lpDesktop.is_null());

        Ok(AgentProcess {
            process: pi.hProcess,
            thread: pi.hThread,
            session_id,
            pid: pi.dwProcessId,
        })
    }
}
