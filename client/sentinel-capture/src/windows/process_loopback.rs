//! Tier A capture: process loopback.
//!
//! `AUDIOCLIENT_ACTIVATION_TYPE_PROCESS_LOOPBACK` gives us exactly the softphone's
//! render audio and nothing else — no Spotify, no Teams, no notification sounds. It
//! requires Windows 11 or Server 2022; see `crate::tier` for why "build 20348+" is
//! the wrong threshold to test for.
//!
//! Two API details that are easy to get wrong:
//!
//! * The activation is asynchronous. `ActivateAudioInterfaceAsync` returns before the
//!   interface exists, and the completion handler has to be a real COM object; there
//!   is no synchronous variant.
//! * Process loopback does **not** honour the device mix format. We request 16 kHz
//!   mono 16-bit explicitly and get it, which is why tier A needs no resampler.

use crate::source::{CaptureError, Result};
use crate::{FRAME_SAMPLES, SAMPLE_RATE};
use std::sync::mpsc::{channel, Receiver, Sender};
use windows::core::{implement, Interface, PCWSTR};
use windows::Win32::Foundation::{CloseHandle, HANDLE, S_OK, WAIT_OBJECT_0};
use windows::Win32::Media::Audio::{
    ActivateAudioInterfaceAsync, IActivateAudioInterfaceAsyncOperation,
    IActivateAudioInterfaceCompletionHandler, IActivateAudioInterfaceCompletionHandler_Impl,
    IAudioCaptureClient, IAudioClient, AUDCLNT_BUFFERFLAGS_DATA_DISCONTINUITY,
    AUDCLNT_BUFFERFLAGS_SILENT, AUDCLNT_SHAREMODE_SHARED, AUDCLNT_STREAMFLAGS_EVENTCALLBACK,
    AUDCLNT_STREAMFLAGS_LOOPBACK, AUDIOCLIENT_ACTIVATION_PARAMS,
    AUDIOCLIENT_ACTIVATION_PARAMS_0, AUDIOCLIENT_ACTIVATION_TYPE_PROCESS_LOOPBACK,
    AUDIOCLIENT_PROCESS_LOOPBACK_PARAMS, PROCESS_LOOPBACK_MODE_INCLUDE_TARGET_PROCESS_TREE,
    VIRTUAL_AUDIO_DEVICE_PROCESS_LOOPBACK, WAVEFORMATEX, WAVE_FORMAT_PCM,
};
use windows::Win32::System::Com::BLOB;
use windows::Win32::System::Threading::{CreateEventW, SetEvent, WaitForSingleObject};

/// `windows_core::PROPVARIANT` wraps a private layout, so a BLOB variant cannot be
/// built through its public API. We lay out the same 24-byte structure by hand and
/// hand the activation call a pointer to it. The size assertion below is what keeps
/// this honest across `windows-rs` upgrades.
#[repr(C)]
struct BlobPropVariant {
    vt: u16,
    reserved1: u16,
    reserved2: u16,
    reserved3: u16,
    blob: BLOB,
}

const _: () = assert!(
    std::mem::size_of::<BlobPropVariant>() == std::mem::size_of::<windows_core::PROPVARIANT>(),
    "PROPVARIANT layout changed; the hand-rolled BLOB variant is no longer valid"
);

/// `VT_BLOB`, spelled out so this file does not depend on the Variant feature just
/// for one constant.
const VT_BLOB_U16: u16 = 65;

/// One captured buffer, already in the pipeline's format.
pub struct CapturedBuffer {
    pub samples: Vec<i16>,
    /// A glitch gap preceded this buffer. The caller inserts silence so the two
    /// channels' timestamps stay aligned.
    pub discontinuity: bool,
    /// The device reported the buffer as silent; the samples are zeroed.
    pub silent: bool,
}

/// Completion handler for `ActivateAudioInterfaceAsync`.
///
/// Signals an event the caller waits on. The interface itself is retrieved from the
/// operation, not passed to the handler, so the handler carries no state beyond the
/// event.
#[implement(IActivateAudioInterfaceCompletionHandler)]
struct ActivationHandler {
    done: HANDLE,
}

#[allow(non_snake_case)]
impl IActivateAudioInterfaceCompletionHandler_Impl for ActivationHandler_Impl {
    fn ActivateCompleted(
        &self,
        _op: Option<&IActivateAudioInterfaceAsyncOperation>,
    ) -> windows::core::Result<()> {
        unsafe { SetEvent(self.done) }?;
        Ok(())
    }
}

/// A live tier A capture stream.
pub struct ProcessLoopbackStream {
    client: IAudioClient,
    capture: IAudioCaptureClient,
    event: HANDLE,
    started: bool,
}

impl ProcessLoopbackStream {
    /// Open process loopback on `pid`, including its process tree so a softphone that
    /// spawns audio in a child process is still covered.
    pub fn open(pid: u32) -> Result<Self> {
        let mut params = AUDIOCLIENT_ACTIVATION_PARAMS {
            ActivationType: AUDIOCLIENT_ACTIVATION_TYPE_PROCESS_LOOPBACK,
            Anonymous: AUDIOCLIENT_ACTIVATION_PARAMS_0 {
                ProcessLoopbackParams: AUDIOCLIENT_PROCESS_LOOPBACK_PARAMS {
                    TargetProcessId: pid,
                    ProcessLoopbackMode: PROCESS_LOOPBACK_MODE_INCLUDE_TARGET_PROCESS_TREE,
                },
            },
        };

        let prop = BlobPropVariant {
            vt: VT_BLOB_U16,
            reserved1: 0,
            reserved2: 0,
            reserved3: 0,
            blob: BLOB {
                cbSize: std::mem::size_of::<AUDIOCLIENT_ACTIVATION_PARAMS>() as u32,
                pBlobData: &mut params as *mut _ as *mut u8,
            },
        };
        let prop_ptr = &prop as *const BlobPropVariant as *const windows_core::PROPVARIANT;

        let done = unsafe { CreateEventW(None, true, false, PCWSTR::null()) }
            .map_err(|e| CaptureError::Platform(format!("activation event: {e}")))?;
        let handler: IActivateAudioInterfaceCompletionHandler =
            ActivationHandler { done }.into();

        let op: IActivateAudioInterfaceAsyncOperation = unsafe {
            ActivateAudioInterfaceAsync(
                VIRTUAL_AUDIO_DEVICE_PROCESS_LOOPBACK,
                &IAudioClient::IID,
                Some(prop_ptr),
                &handler,
            )
        }
        .map_err(|e| CaptureError::Platform(format!("ActivateAudioInterfaceAsync: {e}")))?;

        // `prop` and `params` are plain data with no Drop impl, so there is nothing
        // to free and nothing to release here — the activation parameters are read
        // synchronously even though the interface arrives asynchronously, and both
        // locals live to the end of this function. An explicit `drop` here would be
        // a no-op that reads as cleanup.

        if unsafe { WaitForSingleObject(done, 5_000) } != WAIT_OBJECT_0 {
            unsafe { let _ = CloseHandle(done); }
            return Err(CaptureError::Platform("process loopback activation timed out".into()));
        }
        unsafe { let _ = CloseHandle(done); }

        let mut hr = S_OK;
        let mut unknown: Option<windows::core::IUnknown> = None;
        unsafe { op.GetActivateResult(&mut hr, &mut unknown) }
            .map_err(|e| CaptureError::Platform(format!("GetActivateResult: {e}")))?;
        hr.ok().map_err(|e| CaptureError::Platform(format!("activation failed: {e}")))?;
        let client: IAudioClient = unknown
            .ok_or_else(|| CaptureError::Platform("activation returned no interface".into()))?
            .cast()
            .map_err(|e| CaptureError::Platform(format!("cast to IAudioClient: {e}")))?;

        // Process loopback ignores the device mix format, so ask for what we want.
        let format = WAVEFORMATEX {
            wFormatTag: WAVE_FORMAT_PCM as u16,
            nChannels: 1,
            nSamplesPerSec: SAMPLE_RATE,
            wBitsPerSample: 16,
            nBlockAlign: 2,
            nAvgBytesPerSec: SAMPLE_RATE * 2,
            cbSize: 0,
        };

        unsafe {
            client.Initialize(
                AUDCLNT_SHAREMODE_SHARED,
                AUDCLNT_STREAMFLAGS_LOOPBACK | AUDCLNT_STREAMFLAGS_EVENTCALLBACK,
                // 200 ms buffer: long enough to ride out a scheduling hiccup on a
                // loaded collections desktop, short enough that a crash loses little.
                2_000_000,
                0,
                &format,
                None,
            )
        }
        .map_err(|e| CaptureError::Platform(format!("IAudioClient::Initialize: {e}")))?;

        let event = unsafe { CreateEventW(None, false, false, PCWSTR::null()) }
            .map_err(|e| CaptureError::Platform(format!("capture event: {e}")))?;
        unsafe { client.SetEventHandle(event) }
            .map_err(|e| CaptureError::Platform(format!("SetEventHandle: {e}")))?;

        let capture: IAudioCaptureClient = unsafe { client.GetService() }
            .map_err(|e| CaptureError::Platform(format!("GetService(IAudioCaptureClient): {e}")))?;

        Ok(ProcessLoopbackStream { client, capture, event, started: false })
    }

    pub fn start(&mut self) -> Result<()> {
        unsafe { self.client.Start() }
            .map_err(|e| CaptureError::Platform(format!("IAudioClient::Start: {e}")))?;
        self.started = true;
        Ok(())
    }

    /// Wait for the next buffer and drain everything the engine has.
    ///
    /// Returns `Ok(None)` on timeout, which is normal: a silent call still ticks.
    pub fn read(&mut self, timeout_ms: u32) -> Result<Option<CapturedBuffer>> {
        if unsafe { WaitForSingleObject(self.event, timeout_ms) } != WAIT_OBJECT_0 {
            return Ok(None);
        }
        let mut samples = Vec::with_capacity(FRAME_SAMPLES * 4);
        let mut discontinuity = false;
        let mut silent = false;

        loop {
            let mut data: *mut u8 = std::ptr::null_mut();
            let mut frames: u32 = 0;
            let mut flags: u32 = 0;
            let hr = unsafe {
                self.capture.GetBuffer(&mut data, &mut frames, &mut flags, None, None)
            };
            match hr {
                Ok(()) => {}
                Err(e) if e.code() == windows::Win32::Media::Audio::AUDCLNT_E_DEVICE_INVALIDATED => {
                    return Err(CaptureError::DeviceInvalidated)
                }
                Err(e) if e.code() == windows::Win32::Foundation::S_FALSE => break,
                Err(e) => return Err(CaptureError::Platform(format!("GetBuffer: {e}"))),
            }
            if frames == 0 {
                unsafe { let _ = self.capture.ReleaseBuffer(0); }
                break;
            }
            if flags & AUDCLNT_BUFFERFLAGS_DATA_DISCONTINUITY.0 as u32 != 0 {
                discontinuity = true;
            }
            if flags & AUDCLNT_BUFFERFLAGS_SILENT.0 as u32 != 0 {
                silent = true;
                samples.extend(std::iter::repeat(0i16).take(frames as usize));
            } else {
                let slice =
                    unsafe { std::slice::from_raw_parts(data as *const i16, frames as usize) };
                samples.extend_from_slice(slice);
            }
            unsafe { self.capture.ReleaseBuffer(frames) }
                .map_err(|e| CaptureError::Platform(format!("ReleaseBuffer: {e}")))?;
        }

        if samples.is_empty() {
            return Ok(None);
        }
        Ok(Some(CapturedBuffer { samples, discontinuity, silent }))
    }
}

impl Drop for ProcessLoopbackStream {
    fn drop(&mut self) {
        if self.started {
            unsafe { let _ = self.client.Stop(); }
        }
        unsafe { let _ = CloseHandle(self.event); }
    }
}

/// Try tier A, and say plainly when it is unavailable so the caller can downgrade and
/// log `tier_downgrade` rather than silently capturing nothing.
pub fn try_open(pid: u32) -> std::result::Result<ProcessLoopbackStream, CaptureError> {
    ProcessLoopbackStream::open(pid)
}

/// Channel plumbing helper: the capture loop runs on its own thread and hands buffers
/// over a channel, because `IAudioClient` is apartment-affine and must not be shared.
pub fn spawn_pair() -> (Sender<CapturedBuffer>, Receiver<CapturedBuffer>) {
    channel()
}
