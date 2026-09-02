//! Softphone process resolution and audio session state (spec section 6.4, signal 1).
//!
//! The audio session state is the **primary** call-detection signal.
//! `IAudioSessionEvents::OnStateChanged` fires `AudioSessionStateActive` when call
//! audio begins and `AudioSessionStateInactive` when it ends. Most implementations
//! skip this and rely on VAD alone, which cannot tell a live call from hold music and
//! cannot tell a hangup from a pause for breath.

use crate::source::{CaptureError, Result};
use std::sync::mpsc::Sender;
use windows::core::{implement, Interface};
use windows::Win32::Foundation::{CloseHandle, MAX_PATH};
use windows::Win32::Media::Audio::{
    eMultimedia, eRender, AudioSessionStateActive, AudioSessionState, IAudioSessionControl,
    IAudioSessionControl2, IAudioSessionEvents, IAudioSessionEvents_Impl, IAudioSessionManager2,
    IMMDeviceEnumerator, MMDeviceEnumerator,
};
use windows::Win32::System::Com::{CoCreateInstance, CLSCTX_ALL};
use windows::Win32::System::Diagnostics::ToolHelp::{
    CreateToolhelp32Snapshot, Process32FirstW, Process32NextW, PROCESSENTRY32W,
    TH32CS_SNAPPROCESS,
};

/// Resolve the softphone's process id from the tenant's configured process names,
/// in preference order (OPEN-8: the names are tenant config; this is the lookup).
pub fn resolve_softphone_pid(process_names: &[String]) -> Result<u32> {
    if process_names.is_empty() {
        return Err(CaptureError::SoftphoneNotFound("no process names configured".into()));
    }
    let running = enumerate_processes()?;
    for want in process_names {
        if let Some((pid, _)) = running
            .iter()
            .find(|(_, name)| name.eq_ignore_ascii_case(want))
        {
            return Ok(*pid);
        }
    }
    Err(CaptureError::SoftphoneNotFound(process_names.join(", ")))
}

fn enumerate_processes() -> Result<Vec<(u32, String)>> {
    let snapshot = unsafe { CreateToolhelp32Snapshot(TH32CS_SNAPPROCESS, 0) }
        .map_err(|e| CaptureError::Platform(format!("process snapshot: {e}")))?;
    let mut entry = PROCESSENTRY32W {
        dwSize: std::mem::size_of::<PROCESSENTRY32W>() as u32,
        ..Default::default()
    };
    let mut out = Vec::new();
    unsafe {
        if Process32FirstW(snapshot, &mut entry).is_ok() {
            loop {
                let len = entry.szExeFile.iter().position(|&c| c == 0).unwrap_or(MAX_PATH as usize);
                out.push((entry.th32ProcessID, String::from_utf16_lossy(&entry.szExeFile[..len])));
                if Process32NextW(snapshot, &mut entry).is_err() {
                    break;
                }
            }
        }
        let _ = CloseHandle(snapshot);
    }
    Ok(out)
}

/// Whether the softphone currently holds an active render session.
///
/// Polled at startup to establish the initial state; after that the callback below
/// carries the transitions, because polling alone is too coarse to catch a hold that
/// lasts less than the poll interval.
pub fn softphone_session_active(pid: u32) -> Result<bool> {
    let enumerator: IMMDeviceEnumerator =
        unsafe { CoCreateInstance(&MMDeviceEnumerator, None, CLSCTX_ALL) }
            .map_err(|e| CaptureError::Platform(format!("device enumerator: {e}")))?;
    let device = unsafe { enumerator.GetDefaultAudioEndpoint(eRender, eMultimedia) }
        .map_err(|e| CaptureError::Platform(format!("default endpoint: {e}")))?;
    let manager: IAudioSessionManager2 = unsafe { device.Activate(CLSCTX_ALL, None) }
        .map_err(|e| CaptureError::Platform(format!("session manager: {e}")))?;
    let sessions = unsafe { manager.GetSessionEnumerator() }
        .map_err(|e| CaptureError::Platform(format!("session enumerator: {e}")))?;
    let count = unsafe { sessions.GetCount() }.unwrap_or(0);

    for i in 0..count {
        let Ok(ctl) = (unsafe { sessions.GetSession(i) }) else { continue };
        let Ok(ctl2) = ctl.cast::<IAudioSessionControl2>() else { continue };
        if unsafe { ctl2.GetProcessId() }.unwrap_or(0) != pid {
            continue;
        }
        if unsafe { ctl.GetState() }.unwrap_or(AudioSessionState(0)) == AudioSessionStateActive {
            return Ok(true);
        }
    }
    Ok(false)
}

/// What the detector consumes.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SessionEvent {
    StateChanged { active: bool },
    Disconnected,
}

/// `IAudioSessionEvents` sink.
///
/// Only `OnStateChanged` and `OnSessionDisconnected` carry information we act on; the
/// volume and display-name callbacks are required by the interface and deliberately
/// do nothing.
#[implement(IAudioSessionEvents)]
pub struct SessionWatcher {
    tx: Sender<SessionEvent>,
}

impl SessionWatcher {
    pub fn new(tx: Sender<SessionEvent>) -> Self {
        SessionWatcher { tx }
    }
}

#[allow(non_snake_case)]
impl IAudioSessionEvents_Impl for SessionWatcher_Impl {
    fn OnDisplayNameChanged(
        &self,
        _new: &windows::core::PCWSTR,
        _ctx: *const windows::core::GUID,
    ) -> windows::core::Result<()> {
        Ok(())
    }

    fn OnIconPathChanged(
        &self,
        _new: &windows::core::PCWSTR,
        _ctx: *const windows::core::GUID,
    ) -> windows::core::Result<()> {
        Ok(())
    }

    fn OnSimpleVolumeChanged(
        &self,
        _volume: f32,
        _muted: windows::Win32::Foundation::BOOL,
        _ctx: *const windows::core::GUID,
    ) -> windows::core::Result<()> {
        Ok(())
    }

    fn OnChannelVolumeChanged(
        &self,
        _count: u32,
        _volumes: *const f32,
        _channel: u32,
        _ctx: *const windows::core::GUID,
    ) -> windows::core::Result<()> {
        Ok(())
    }

    fn OnGroupingParamChanged(
        &self,
        _group: *const windows::core::GUID,
        _ctx: *const windows::core::GUID,
    ) -> windows::core::Result<()> {
        Ok(())
    }

    fn OnStateChanged(&self, state: AudioSessionState) -> windows::core::Result<()> {
        let _ = self.tx.send(SessionEvent::StateChanged {
            active: state == AudioSessionStateActive,
        });
        Ok(())
    }

    fn OnSessionDisconnected(
        &self,
        _reason: windows::Win32::Media::Audio::AudioSessionDisconnectReason,
    ) -> windows::core::Result<()> {
        // The softphone exited or the endpoint went away. Treated as Inactive by the
        // detector, which then closes the call after the usual silence window rather
        // than truncating it on the spot.
        let _ = self.tx.send(SessionEvent::Disconnected);
        Ok(())
    }
}

/// Register a [`SessionWatcher`] on the softphone's session.
///
/// Returns the control so the caller can keep it alive and unregister on the way out;
/// dropping it without unregistering leaves the audio engine holding a dangling sink.
pub fn watch_softphone_session(
    pid: u32,
    tx: Sender<SessionEvent>,
) -> Result<(IAudioSessionControl, IAudioSessionEvents)> {
    let enumerator: IMMDeviceEnumerator =
        unsafe { CoCreateInstance(&MMDeviceEnumerator, None, CLSCTX_ALL) }
            .map_err(|e| CaptureError::Platform(format!("device enumerator: {e}")))?;
    let device = unsafe { enumerator.GetDefaultAudioEndpoint(eRender, eMultimedia) }
        .map_err(|e| CaptureError::Platform(format!("default endpoint: {e}")))?;
    let manager: IAudioSessionManager2 = unsafe { device.Activate(CLSCTX_ALL, None) }
        .map_err(|e| CaptureError::Platform(format!("session manager: {e}")))?;
    let sessions = unsafe { manager.GetSessionEnumerator() }
        .map_err(|e| CaptureError::Platform(format!("session enumerator: {e}")))?;
    let count = unsafe { sessions.GetCount() }.unwrap_or(0);

    for i in 0..count {
        let Ok(ctl) = (unsafe { sessions.GetSession(i) }) else { continue };
        let Ok(ctl2) = ctl.cast::<IAudioSessionControl2>() else { continue };
        if unsafe { ctl2.GetProcessId() }.unwrap_or(0) != pid {
            continue;
        }
        let sink: IAudioSessionEvents = SessionWatcher::new(tx).into();
        unsafe { ctl.RegisterAudioSessionNotification(&sink) }
            .map_err(|e| CaptureError::Platform(format!("register session sink: {e}")))?;
        return Ok((ctl, sink));
    }
    Err(CaptureError::SoftphoneNotFound(format!("pid {pid} has no render session")))
}
