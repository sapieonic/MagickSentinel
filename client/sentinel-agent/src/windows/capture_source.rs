//! `CaptureSource` over the WASAPI streams in `sentinel-capture`.
//!
//! The capture crate provides the two tier implementations as concrete streams;
//! this adapter is the glue that lets the pipeline drive either of them through the
//! same trait it drives `WavReplaySource` through in CI. It is the only part of the
//! audio path that cannot be exercised without a sound card, which is why it is kept
//! this thin.
//!
//! Two behaviours worth stating, because both are silent when wrong:
//!
//! * **Tier A falls back to tier B for the session.** If
//!   `ActivateAudioInterfaceAsync` fails — an OS build that turns out not to support
//!   process loopback, or a softphone whose PID we resolved wrongly — capturing
//!   nothing would look identical to a quiet shift. The adapter downgrades, and the
//!   caller emits `tier_downgrade`.
//! * **A discontinuity becomes silence, not a shorter stream.**
//!   `AUDCLNT_BUFFERFLAGS_DATA_DISCONTINUITY` means samples were lost. Dropping them
//!   silently shortens one channel relative to the other and shifts every word after
//!   the glitch in the transcript alignment.

use sentinel_capture::device::{AudioDevice, DeviceEvent, DeviceId, Direction};
use sentinel_capture::source::{CaptureError, CaptureSource, Result, StreamHandle};
use sentinel_capture::tier::CaptureTier;
use sentinel_capture::windows::endpoint_loopback::{enumerate_endpoints, EndpointLoopbackStream};
use sentinel_capture::windows::process_loopback::ProcessLoopbackStream;
use sentinel_capture::windows::ComGuard;
use sentinel_capture::FRAME_SAMPLES;
use std::collections::{HashMap, VecDeque};
use std::sync::mpsc::Sender;

/// Milliseconds to wait for a buffer before returning empty. Shorter than one Opus
/// frame so the pipeline's read loop stays responsive on a silent line.
const READ_TIMEOUT_MS: u32 = 10;

enum Stream {
    Process(ProcessLoopbackStream),
    Endpoint(EndpointLoopbackStream),
}

impl Stream {
    fn read(&mut self) -> Result<Option<sentinel_capture::windows::process_loopback::CapturedBuffer>> {
        match self {
            Stream::Process(s) => s.read(READ_TIMEOUT_MS),
            Stream::Endpoint(s) => s.read(READ_TIMEOUT_MS),
        }
    }

    fn start(&mut self) -> Result<()> {
        match self {
            Stream::Process(s) => s.start(),
            Stream::Endpoint(s) => s.start(),
        }
    }
}

// The WASAPI interfaces behind these streams are apartment-affine: they are created,
// read and dropped on the one thread that called `CoInitializeEx`, and the source is
// moved to that thread whole rather than shared. `CaptureSource` requires `Send`
// because the pipeline owns it, not because it is used from two threads.
unsafe impl Send for WindowsCaptureSource {}

struct Open {
    stream: Stream,
    /// Samples read from the engine but not yet handed to the caller. WASAPI delivers
    /// whatever is in the endpoint buffer, which is never a neat multiple of the
    /// caller's request.
    buffered: VecDeque<i16>,
}

/// The live capture source.
pub struct WindowsCaptureSource {
    /// COM must be initialised on this thread for as long as any stream is open, and
    /// uninitialised on the way out, or the audio engine leaks endpoint references.
    _com: ComGuard,
    tier: CaptureTier,
    /// Resolved softphone PID, for tier A.
    softphone_pid: Option<u32>,
    streams: HashMap<u64, Open>,
    next_handle: u64,
    subscribers: Vec<Sender<DeviceEvent>>,
    /// Set when tier A activation failed and this session ran on tier B instead.
    downgraded: bool,
}

impl WindowsCaptureSource {
    /// `softphone_pid` is required for tier A and ignored on tier B.
    pub fn new(tier: CaptureTier, softphone_pid: Option<u32>) -> Result<Self> {
        Ok(WindowsCaptureSource {
            _com: ComGuard::new()
                .map_err(|e| CaptureError::Platform(format!("CoInitializeEx: {e}")))?,
            tier,
            softphone_pid,
            streams: HashMap::new(),
            next_handle: 1,
            subscribers: Vec::new(),
            downgraded: false,
        })
    }

    /// True if tier A activation failed and the session downgraded. The caller emits
    /// a `tier_downgrade` event and reports tier B in the heartbeat.
    pub fn downgraded(&self) -> bool {
        self.downgraded
    }

    /// The tier actually in effect.
    pub fn effective_tier(&self) -> CaptureTier {
        if self.downgraded {
            CaptureTier::B
        } else {
            self.tier
        }
    }
}

impl CaptureSource for WindowsCaptureSource {
    fn enumerate(&self) -> Result<Vec<AudioDevice>> {
        enumerate_endpoints()
    }

    fn open(&mut self, device: &DeviceId, dir: Direction) -> Result<StreamHandle> {
        // Tier A only replaces the *far* channel: process loopback captures what the
        // softphone renders. The near channel is an ordinary microphone capture on the
        // pinned endpoint either way.
        let stream = if self.tier == CaptureTier::A
            && dir == Direction::Render
            && !self.downgraded
        {
            match self.softphone_pid.map(ProcessLoopbackStream::open) {
                Some(Ok(s)) => Stream::Process(s),
                Some(Err(e)) => {
                    tracing::warn!(error = %e, "process loopback unavailable; downgrading to tier B");
                    self.downgraded = true;
                    Stream::Endpoint(EndpointLoopbackStream::open(&device.0, dir)?)
                }
                None => {
                    tracing::warn!("no softphone PID resolved; downgrading to tier B");
                    self.downgraded = true;
                    Stream::Endpoint(EndpointLoopbackStream::open(&device.0, dir)?)
                }
            }
        } else {
            Stream::Endpoint(EndpointLoopbackStream::open(&device.0, dir)?)
        };

        let mut open = Open { stream, buffered: VecDeque::new() };
        open.stream.start()?;

        let handle = self.next_handle;
        self.next_handle += 1;
        self.streams.insert(handle, open);
        Ok(StreamHandle(handle))
    }

    fn read_frames(&mut self, h: StreamHandle, buf: &mut [i16]) -> Result<usize> {
        let open = self.streams.get_mut(&h.0).ok_or(CaptureError::BadHandle(h.0))?;

        // Top up from the engine before serving, so a caller asking for exactly one
        // frame is not starved by a buffer that arrived in 480-sample chunks.
        while open.buffered.len() < buf.len() {
            match open.stream.read()? {
                Some(captured) => {
                    if captured.discontinuity {
                        // Samples were lost. Fill the gap so the two channels stay
                        // aligned; one frame is the smallest unit the encoder can
                        // account for, and the engine does not tell us how much was
                        // dropped.
                        tracing::debug!("capture discontinuity; inserting silence");
                        open.buffered.extend(std::iter::repeat(0i16).take(FRAME_SAMPLES));
                    }
                    if captured.samples.is_empty() {
                        break;
                    }
                    open.buffered.extend(captured.samples);
                }
                // Timeout. Not end of stream: a silent line still ticks.
                None => break,
            }
        }

        let n = buf.len().min(open.buffered.len());
        for slot in buf.iter_mut().take(n) {
            *slot = open.buffered.pop_front().unwrap_or(0);
        }
        Ok(n)
    }

    fn subscribe_device_changes(&mut self, tx: Sender<DeviceEvent>) -> Result<()> {
        // The `IMMNotificationClient` registration itself lives with the enumerator
        // the agent's device thread owns; this records the sink so a re-resolve can
        // be driven from here too.
        self.subscribers.push(tx);
        Ok(())
    }

    fn close(&mut self, h: StreamHandle) -> Result<()> {
        // Dropping the stream stops the client and closes its event handle.
        self.streams.remove(&h.0);
        Ok(())
    }
}
