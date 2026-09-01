//! Audio capture for the Sentinel endpoint agent.
//!
//! The two channels are kept separate end to end — channel 0 (`far`) is render
//! loopback carrying the borrower, channel 1 (`near`) is microphone capture carrying
//! the agent. They are never mixed: separate channels give exact speaker attribution
//! with no diarization step, which is the single biggest quality advantage over
//! competitors analysing mono recordings.

pub mod device;
pub mod foreign;
pub mod replay;
pub mod resample;
pub mod source;
pub mod tier;
pub mod vad;

#[cfg(windows)]
pub mod windows;

pub use device::{AudioDevice, DeviceEvent, DeviceId, Direction};
pub use foreign::ForeignAudioSuppressor;
pub use replay::WavReplaySource;
pub use source::{CaptureError, CaptureSource, StreamHandle};
pub use tier::{CaptureTier, TierDetection};
pub use vad::Vad;

/// Everything downstream of capture works at this rate, mono, `i16`.
pub const SAMPLE_RATE: u32 = 16_000;
/// Opus frame duration.
pub const FRAME_MS: u32 = 20;
/// Samples in one 20 ms frame at 16 kHz.
pub const FRAME_SAMPLES: usize = (SAMPLE_RATE as usize * FRAME_MS as usize) / 1000;
