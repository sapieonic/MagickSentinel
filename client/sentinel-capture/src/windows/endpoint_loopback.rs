//! Tier B capture: endpoint loopback on a pinned device.
//!
//! Endpoint loopback captures everything rendered to the device, which is why the two
//! mitigations in spec section 3 are not optional:
//!
//! * the device is **pinned by container ID** in tenant policy and never resolved
//!   through `GetDefaultAudioEndpoint`, and
//! * everything captured while the softphone session is Inactive is marked
//!   `foreign` (see [`crate::foreign`]) and never transcribed.
//!
//! Unlike tier A, this path must take the device mix format — typically 48 kHz stereo
//! float — and convert it, so [`crate::resample`] is in the hot path here.

use crate::device::{AudioDevice, DeviceId, Direction};
use crate::resample::{downmix_to_mono, f32_to_i16, Resampler};
use crate::source::{CaptureError, Result};
use crate::windows::process_loopback::CapturedBuffer;
use crate::SAMPLE_RATE;
use windows::core::PCWSTR;
use windows::Win32::Devices::FunctionDiscovery::PKEY_Device_FriendlyName;
use windows::Win32::Foundation::{CloseHandle, HANDLE, WAIT_OBJECT_0};
use windows::Win32::Media::Audio::{
    eCapture, eRender, IAudioCaptureClient, IAudioClient, IMMDevice, IMMDeviceEnumerator,
    MMDeviceEnumerator, AUDCLNT_BUFFERFLAGS_DATA_DISCONTINUITY, AUDCLNT_BUFFERFLAGS_SILENT,
    AUDCLNT_E_DEVICE_INVALIDATED, AUDCLNT_SHAREMODE_SHARED, AUDCLNT_STREAMFLAGS_EVENTCALLBACK,
    AUDCLNT_STREAMFLAGS_LOOPBACK, DEVICE_STATE, DEVICE_STATE_ACTIVE, DEVICE_STATE_DISABLED,
    DEVICE_STATE_NOTPRESENT, DEVICE_STATE_UNPLUGGED, WAVEFORMATEX, WAVEFORMATEXTENSIBLE,
};

/// `WAVE_FORMAT_EXTENSIBLE` and `WAVE_FORMAT_IEEE_FLOAT` from mmreg.h, which
/// `windows-rs` does not surface in the Audio module.
const WAVE_FORMAT_EXTENSIBLE: u16 = 0xFFFE;
const WAVE_FORMAT_IEEE_FLOAT: u16 = 0x0003;
/// `KSDATAFORMAT_SUBTYPE_IEEE_FLOAT`.
const SUBTYPE_IEEE_FLOAT: windows::core::GUID =
    windows::core::GUID::from_u128(0x00000003_0000_0010_8000_00aa00389b71);
use windows::Win32::System::Com::{CoCreateInstance, CoTaskMemFree, CLSCTX_ALL, STGM_READ};
use windows::Win32::System::Threading::{CreateEventW, WaitForSingleObject};
use windows::Win32::UI::Shell::PropertiesSystem::PROPERTYKEY;

/// `PKEY_Device_ContainerId`. Not exported by `windows-rs`, so it is spelled out.
/// Container ID is what survives a replug; the endpoint ID does not, and agents
/// unplug USB headsets constantly.
const PKEY_DEVICE_CONTAINER_ID: PROPERTYKEY = PROPERTYKEY {
    fmtid: windows::core::GUID::from_u128(0x8c7ed206_3f8a_4827_b3ab_ae9e1faefc6c),
    pid: 2,
};

/// Enumerate every render and capture endpoint the machine knows about, in whatever
/// state, so the widget can say "headset not detected" rather than showing an empty
/// list.
pub fn enumerate_endpoints() -> Result<Vec<AudioDevice>> {
    let enumerator: IMMDeviceEnumerator =
        unsafe { CoCreateInstance(&MMDeviceEnumerator, None, CLSCTX_ALL) }
            .map_err(|e| CaptureError::Platform(format!("device enumerator: {e}")))?;

    let states = DEVICE_STATE(
        DEVICE_STATE_ACTIVE.0
            | DEVICE_STATE_DISABLED.0
            | DEVICE_STATE_NOTPRESENT.0
            | DEVICE_STATE_UNPLUGGED.0,
    );
    let mut out = Vec::new();

    for (flow, direction) in [(eRender, Direction::Render), (eCapture, Direction::Capture)] {
        let collection = unsafe { enumerator.EnumAudioEndpoints(flow, states) }
            .map_err(|e| CaptureError::Platform(format!("EnumAudioEndpoints: {e}")))?;
        let count = unsafe { collection.GetCount() }.unwrap_or(0);
        for i in 0..count {
            let Ok(device) = (unsafe { collection.Item(i) }) else { continue };
            out.push(describe(&device, direction)?);
        }
    }
    Ok(out)
}

fn describe(device: &IMMDevice, direction: Direction) -> Result<AudioDevice> {
    let id_ptr = unsafe { device.GetId() }
        .map_err(|e| CaptureError::Platform(format!("GetId: {e}")))?;
    let id = unsafe { id_ptr.to_string() }.unwrap_or_default();
    unsafe { CoTaskMemFree(Some(id_ptr.0 as *const _)) };

    let state = unsafe { device.GetState() }.unwrap_or(DEVICE_STATE_NOTPRESENT);
    let store = unsafe { device.OpenPropertyStore(STGM_READ) }
        .map_err(|e| CaptureError::Platform(format!("OpenPropertyStore: {e}")))?;

    let friendly_name = unsafe { store.GetValue(&PKEY_Device_FriendlyName) }
        .ok()
        .map(|v| v.to_string())
        .filter(|s| !s.is_empty())
        .unwrap_or_else(|| id.clone());
    let container_id = unsafe { store.GetValue(&PKEY_DEVICE_CONTAINER_ID) }
        .ok()
        .map(|v| v.to_string())
        .filter(|s| !s.is_empty());

    Ok(AudioDevice {
        id: DeviceId(id),
        container_id,
        friendly_name,
        direction,
        // Deliberately never set: policy pins a device, and "is default" must not
        // influence selection on tier B.
        is_default: false,
        active: state == DEVICE_STATE_ACTIVE,
    })
}

/// A live tier B stream on a pinned endpoint.
pub struct EndpointLoopbackStream {
    client: IAudioClient,
    capture: IAudioCaptureClient,
    event: HANDLE,
    resampler: Resampler,
    channels: usize,
    /// True when the mix format is float; otherwise 16-bit PCM.
    float_format: bool,
    started: bool,
}

impl EndpointLoopbackStream {
    /// Open loopback on a specific endpoint id.
    ///
    /// `GetDevice`, never `GetDefaultAudioEndpoint`: on this tier the default device
    /// is whatever Windows last decided, which on a collections desktop is as likely
    /// to be the monitor speakers as the agent's headset.
    pub fn open(endpoint_id: &str, direction: Direction) -> Result<Self> {
        let enumerator: IMMDeviceEnumerator =
            unsafe { CoCreateInstance(&MMDeviceEnumerator, None, CLSCTX_ALL) }
                .map_err(|e| CaptureError::Platform(format!("device enumerator: {e}")))?;
        let wide: Vec<u16> = endpoint_id.encode_utf16().chain(std::iter::once(0)).collect();
        let device = unsafe { enumerator.GetDevice(PCWSTR(wide.as_ptr())) }
            .map_err(|_| CaptureError::DeviceNotFound(endpoint_id.to_string()))?;

        let client: IAudioClient = unsafe { device.Activate(CLSCTX_ALL, None) }
            .map_err(|e| CaptureError::Platform(format!("Activate(IAudioClient): {e}")))?;

        let mix = unsafe { client.GetMixFormat() }
            .map_err(|e| CaptureError::Platform(format!("GetMixFormat: {e}")))?;
        let (rate, channels, float_format) = unsafe { describe_format(mix) };

        // Loopback only applies to a render endpoint; the near channel is an ordinary
        // capture stream on the headset microphone.
        let flags = if direction == Direction::Render {
            AUDCLNT_STREAMFLAGS_LOOPBACK | AUDCLNT_STREAMFLAGS_EVENTCALLBACK
        } else {
            AUDCLNT_STREAMFLAGS_EVENTCALLBACK
        };

        unsafe { client.Initialize(AUDCLNT_SHAREMODE_SHARED, flags, 2_000_000, 0, mix, None) }
            .map_err(|e| CaptureError::Platform(format!("IAudioClient::Initialize: {e}")))?;
        unsafe { CoTaskMemFree(Some(mix as *const _)) };

        let event = unsafe { CreateEventW(None, false, false, PCWSTR::null()) }
            .map_err(|e| CaptureError::Platform(format!("capture event: {e}")))?;
        unsafe { client.SetEventHandle(event) }
            .map_err(|e| CaptureError::Platform(format!("SetEventHandle: {e}")))?;

        let capture: IAudioCaptureClient = unsafe { client.GetService() }
            .map_err(|e| CaptureError::Platform(format!("GetService: {e}")))?;

        Ok(EndpointLoopbackStream {
            client,
            capture,
            event,
            resampler: Resampler::new(rate, SAMPLE_RATE),
            channels,
            float_format,
            started: false,
        })
    }

    pub fn start(&mut self) -> Result<()> {
        unsafe { self.client.Start() }
            .map_err(|e| CaptureError::Platform(format!("IAudioClient::Start: {e}")))?;
        self.started = true;
        Ok(())
    }

    /// Read, downmix, resample. Returns `Ok(None)` on timeout.
    pub fn read(&mut self, timeout_ms: u32) -> Result<Option<CapturedBuffer>> {
        if unsafe { WaitForSingleObject(self.event, timeout_ms) } != WAIT_OBJECT_0 {
            return Ok(None);
        }
        let mut interleaved: Vec<f32> = Vec::new();
        let mut discontinuity = false;
        let mut silent = false;

        loop {
            let mut data: *mut u8 = std::ptr::null_mut();
            let mut frames: u32 = 0;
            let mut flags: u32 = 0;
            match unsafe { self.capture.GetBuffer(&mut data, &mut frames, &mut flags, None, None) }
            {
                Ok(()) => {}
                Err(e) if e.code() == AUDCLNT_E_DEVICE_INVALIDATED => {
                    return Err(CaptureError::DeviceInvalidated)
                }
                Err(_) => break,
            }
            if frames == 0 {
                unsafe { let _ = self.capture.ReleaseBuffer(0); }
                break;
            }
            if flags & AUDCLNT_BUFFERFLAGS_DATA_DISCONTINUITY.0 as u32 != 0 {
                discontinuity = true;
            }
            let count = frames as usize * self.channels;
            if flags & AUDCLNT_BUFFERFLAGS_SILENT.0 as u32 != 0 {
                silent = true;
                interleaved.extend(std::iter::repeat(0.0f32).take(count));
            } else if self.float_format {
                interleaved
                    .extend_from_slice(unsafe { std::slice::from_raw_parts(data as *const f32, count) });
            } else {
                let pcm = unsafe { std::slice::from_raw_parts(data as *const i16, count) };
                interleaved.extend(pcm.iter().map(|&v| v as f32 / i16::MAX as f32));
            }
            unsafe { self.capture.ReleaseBuffer(frames) }
                .map_err(|e| CaptureError::Platform(format!("ReleaseBuffer: {e}")))?;
        }

        if interleaved.is_empty() {
            return Ok(None);
        }
        let mono = downmix_to_mono(&interleaved, self.channels);
        let resampled = self.resampler.process(&mono);
        Ok(Some(CapturedBuffer {
            samples: f32_to_i16(&resampled),
            discontinuity,
            silent,
        }))
    }
}

impl Drop for EndpointLoopbackStream {
    fn drop(&mut self) {
        if self.started {
            unsafe { let _ = self.client.Stop(); }
        }
        unsafe { let _ = CloseHandle(self.event); }
    }
}

/// Pull rate, channel count and sample type out of a mix format, handling the
/// extensible case that every modern driver actually returns.
unsafe fn describe_format(fmt: *const WAVEFORMATEX) -> (u32, usize, bool) {
    let base = &*fmt;
    let float = if base.wFormatTag == WAVE_FORMAT_EXTENSIBLE {
        // WAVEFORMATEXTENSIBLE is packed, so the GUID must be read unaligned rather
        // than referenced.
        let subformat = std::ptr::addr_of!((*(fmt as *const WAVEFORMATEXTENSIBLE)).SubFormat)
            .read_unaligned();
        subformat == SUBTYPE_IEEE_FLOAT
    } else {
        base.wFormatTag == WAVE_FORMAT_IEEE_FLOAT
    };
    (base.nSamplesPerSec, base.nChannels as usize, float)
}
