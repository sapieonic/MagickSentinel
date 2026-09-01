//! The capture abstraction.
//!
//! **This trait exists for testability, not portability.** It is here so tests can
//! inject a WAV-replay source and give the state machine, spool and uplink CI
//! coverage with no sound card. Do not add platform-neutral concepts to it, and do
//! not treat it as the seam for a future macOS or Linux client — there isn't one.

use crate::device::{AudioDevice, DeviceEvent, DeviceId, Direction};
use std::sync::mpsc::Sender;

#[derive(Debug, thiserror::Error)]
pub enum CaptureError {
    #[error("audio device {0} not found")]
    DeviceNotFound(String),
    #[error("stream handle {0} is not open")]
    BadHandle(u64),
    #[error("the audio device was invalidated and must be reopened")]
    DeviceInvalidated,
    #[error("capture tier {0:?} is not supported on this OS build")]
    UnsupportedTier(crate::tier::CaptureTier),
    #[error("softphone process not found: {0}")]
    SoftphoneNotFound(String),
    #[error("audio subsystem error: {0}")]
    Platform(String),
    #[error(transparent)]
    Io(#[from] std::io::Error),
}

pub type Result<T> = std::result::Result<T, CaptureError>;

/// An opaque handle to an open stream.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct StreamHandle(pub u64);

pub trait CaptureSource: Send {
    fn enumerate(&self) -> Result<Vec<AudioDevice>>;
    fn open(&mut self, device: &DeviceId, dir: Direction) -> Result<StreamHandle>;
    /// Fill `buf` with 16 kHz mono `i16` samples, returning how many were written.
    /// Zero means no data is available yet, not end of stream.
    fn read_frames(&mut self, h: StreamHandle, buf: &mut [i16]) -> Result<usize>;
    fn subscribe_device_changes(&mut self, tx: Sender<DeviceEvent>) -> Result<()>;
    fn close(&mut self, h: StreamHandle) -> Result<()>;

    /// Whether the audio engine reported a gap since the last call, clearing the
    /// flag.
    ///
    /// `AUDCLNT_BUFFERFLAGS_DATA_DISCONTINUITY` has to reach the encoder or the
    /// `silence_inserted` bit in the frame header can never be set on a real
    /// capture, and the server has no way to tell synthesised silence from a
    /// borrower saying nothing. Reported out-of-band rather than through
    /// `read_frames` because a glitch is a property of the stream between two reads,
    /// not of the samples either read returned.
    ///
    /// The default is `false` so a source with no notion of glitches — the WAV
    /// replay source in most of its configurations — needs no implementation.
    fn take_discontinuity(&mut self, _h: StreamHandle) -> Result<bool> {
        Ok(false)
    }
}
