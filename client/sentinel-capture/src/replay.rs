//! `WavReplaySource` — tests only.
//!
//! Replays fixture WAVs with realistic timing so the state machine, spool and uplink
//! have CI coverage with no sound card. "Realistic timing" matters: a source that
//! hands over the whole file at once would never exercise the partial-read, glitch
//! and backpressure paths that the real WASAPI loop spends all its time in.

use crate::device::{AudioDevice, DeviceEvent, DeviceId, Direction};
use crate::source::{CaptureError, CaptureSource, Result, StreamHandle};
use crate::{FRAME_SAMPLES, SAMPLE_RATE};
use std::collections::HashMap;
use std::path::Path;
use std::sync::mpsc::Sender;
use std::time::Instant;

struct Stream {
    samples: Vec<i16>,
    pos: usize,
    started: Instant,
    /// Sample indices at which to report a glitch gap, simulating
    /// `AUDCLNT_BUFFERFLAGS_DATA_DISCONTINUITY`.
    glitch_at: Vec<usize>,
    direction: Direction,
}

/// A fixture device backed by a WAV file.
pub struct ReplayDevice {
    pub device: AudioDevice,
    pub samples: Vec<i16>,
}

pub struct WavReplaySource {
    devices: Vec<ReplayDevice>,
    streams: HashMap<u64, Stream>,
    next_handle: u64,
    subscribers: Vec<Sender<DeviceEvent>>,
    /// When set, `read_frames` returns data as fast as it is asked for instead of
    /// pacing to the wall clock. Used by tests that assert on content rather than
    /// timing.
    realtime: bool,
    glitch_every: Option<usize>,
    /// Set by `simulate_device_loss`; the next read returns `DeviceInvalidated`.
    invalidated: bool,
}

impl WavReplaySource {
    pub fn new() -> Self {
        WavReplaySource {
            devices: Vec::new(),
            streams: HashMap::new(),
            next_handle: 1,
            subscribers: Vec::new(),
            realtime: true,
            glitch_every: None,
            invalidated: false,
        }
    }

    /// Replay as fast as the caller reads. Tests that assert on sample content
    /// should use this; tests that assert on the state machine's timing should not.
    pub fn without_realtime_pacing(mut self) -> Self {
        self.realtime = false;
        self
    }

    /// Inject a discontinuity every `n` samples so the silence-insertion path is
    /// exercised.
    pub fn with_glitch_every(mut self, n: usize) -> Self {
        self.glitch_every = Some(n);
        self
    }

    pub fn add_wav(&mut self, id: &str, container: &str, name: &str, dir: Direction, path: &Path) -> Result<()> {
        let samples = read_wav_16k_mono(path)?;
        self.add_samples(id, container, name, dir, samples);
        Ok(())
    }

    pub fn add_samples(&mut self, id: &str, container: &str, name: &str, dir: Direction, samples: Vec<i16>) {
        self.devices.push(ReplayDevice {
            device: AudioDevice {
                id: DeviceId(id.into()),
                container_id: Some(container.into()),
                friendly_name: name.into(),
                direction: dir,
                is_default: false,
                active: true,
            },
            samples,
        });
    }

    /// Simulate a headset being unplugged: emit the event and fail subsequent reads
    /// the way `AUDCLNT_E_DEVICE_INVALIDATED` does.
    pub fn simulate_device_loss(&mut self, id: &DeviceId) {
        self.invalidated = true;
        if let Some(d) = self.devices.iter_mut().find(|d| &d.device.id == id) {
            d.device.active = false;
        }
        self.broadcast(DeviceEvent::StateChanged { id: id.clone(), active: false });
        self.broadcast(DeviceEvent::Removed(id.clone()));
    }

    /// Simulate the headset coming back on a different endpoint but the same
    /// container, which is what actually happens on a replug.
    pub fn simulate_device_restore(&mut self, id: &DeviceId) {
        self.invalidated = false;
        if let Some(d) = self.devices.iter_mut().find(|d| &d.device.id == id) {
            d.device.active = true;
            let dev = d.device.clone();
            self.broadcast(DeviceEvent::Added(dev));
        }
    }

    fn broadcast(&self, ev: DeviceEvent) {
        for tx in &self.subscribers {
            let _ = tx.send(ev.clone());
        }
    }

    /// Total samples remaining on a stream.
    pub fn remaining(&self, h: StreamHandle) -> usize {
        self.streams.get(&h.0).map_or(0, |s| s.samples.len().saturating_sub(s.pos))
    }
}

impl Default for WavReplaySource {
    fn default() -> Self {
        Self::new()
    }
}

impl CaptureSource for WavReplaySource {
    fn enumerate(&self) -> Result<Vec<AudioDevice>> {
        Ok(self.devices.iter().map(|d| d.device.clone()).collect())
    }

    fn open(&mut self, device: &DeviceId, dir: Direction) -> Result<StreamHandle> {
        let d = self
            .devices
            .iter()
            .find(|d| &d.device.id == device && d.device.direction == dir)
            .ok_or_else(|| CaptureError::DeviceNotFound(device.0.clone()))?;
        if !d.device.active {
            return Err(CaptureError::DeviceInvalidated);
        }
        let glitch_at = match self.glitch_every {
            Some(n) if n > 0 => (n..d.samples.len()).step_by(n).collect(),
            _ => Vec::new(),
        };
        let h = self.next_handle;
        self.next_handle += 1;
        self.streams.insert(
            h,
            Stream {
                samples: d.samples.clone(),
                pos: 0,
                started: Instant::now(),
                glitch_at,
                direction: dir,
            },
        );
        Ok(StreamHandle(h))
    }

    fn read_frames(&mut self, h: StreamHandle, buf: &mut [i16]) -> Result<usize> {
        if self.invalidated {
            return Err(CaptureError::DeviceInvalidated);
        }
        let realtime = self.realtime;
        let s = self.streams.get_mut(&h.0).ok_or(CaptureError::BadHandle(h.0))?;

        let mut want = buf.len() - (buf.len() % FRAME_SAMPLES);
        if realtime {
            // Only hand over audio that "has happened" by now.
            let elapsed = s.started.elapsed();
            let available = (elapsed.as_secs_f64() * SAMPLE_RATE as f64) as usize;
            let ready = available.saturating_sub(s.pos);
            want = want.min(ready - (ready % FRAME_SAMPLES));
            if want == 0 {
                return Ok(0);
            }
        }

        let end = (s.pos + want).min(s.samples.len());
        let n = end - s.pos;
        if n == 0 {
            return Ok(0);
        }
        buf[..n].copy_from_slice(&s.samples[s.pos..end]);

        // A glitch drops the tail of this read; the caller sees a short read and,
        // in the real client, inserts silence to keep timestamps aligned.
        let dropped = s
            .glitch_at
            .iter()
            .any(|&g| g > s.pos && g <= end);
        s.pos = end;
        let _ = s.direction;
        Ok(if dropped { n.saturating_sub(FRAME_SAMPLES) } else { n })
    }

    fn subscribe_device_changes(&mut self, tx: Sender<DeviceEvent>) -> Result<()> {
        self.subscribers.push(tx);
        Ok(())
    }

    fn close(&mut self, h: StreamHandle) -> Result<()> {
        self.streams.remove(&h.0).map(|_| ()).ok_or(CaptureError::BadHandle(h.0))
    }
}

/// Read a WAV file, requiring exactly the format the pipeline works in.
///
/// Fixtures are converted once, at authoring time, rather than resampled here: a
/// replay source that quietly resamples would hide a real bug in the capture path's
/// own resampler.
pub fn read_wav_16k_mono(path: &Path) -> Result<Vec<i16>> {
    let reader = hound::WavReader::open(path)
        .map_err(|e| CaptureError::Platform(format!("{}: {e}", path.display())))?;
    let spec = reader.spec();
    if spec.channels != 1 || spec.sample_rate != SAMPLE_RATE || spec.bits_per_sample != 16 {
        return Err(CaptureError::Platform(format!(
            "{}: fixtures must be 16 kHz mono 16-bit, got {} ch / {} Hz / {} bit",
            path.display(),
            spec.channels,
            spec.sample_rate,
            spec.bits_per_sample
        )));
    }
    reader
        .into_samples::<i16>()
        .collect::<std::result::Result<Vec<_>, _>>()
        .map_err(|e| CaptureError::Platform(e.to_string()))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::mpsc::channel;

    fn ramp(n: usize) -> Vec<i16> {
        (0..n).map(|i| (i % 1000) as i16).collect()
    }

    fn source() -> WavReplaySource {
        let mut s = WavReplaySource::new().without_realtime_pacing();
        s.add_samples("far-1", "cont-far", "Headset (loopback)", Direction::Render, ramp(16_000));
        s.add_samples("near-1", "cont-near", "Headset (mic)", Direction::Capture, ramp(16_000));
        s
    }

    #[test]
    fn replays_the_whole_fixture_in_order() {
        let mut s = source();
        let h = s.open(&DeviceId("far-1".into()), Direction::Render).unwrap();
        let mut got = Vec::new();
        let mut buf = vec![0i16; FRAME_SAMPLES * 4];
        loop {
            let n = s.read_frames(h, &mut buf).unwrap();
            if n == 0 {
                break;
            }
            got.extend_from_slice(&buf[..n]);
        }
        assert_eq!(got, ramp(16_000));
    }

    #[test]
    fn opening_the_wrong_direction_fails() {
        let mut s = source();
        assert!(matches!(
            s.open(&DeviceId("far-1".into()), Direction::Capture),
            Err(CaptureError::DeviceNotFound(_))
        ));
    }

    #[test]
    fn device_loss_invalidates_reads_and_notifies_subscribers() {
        let mut s = source();
        let (tx, rx) = channel();
        s.subscribe_device_changes(tx).unwrap();
        let h = s.open(&DeviceId("far-1".into()), Direction::Render).unwrap();
        assert!(s.read_frames(h, &mut vec![0i16; FRAME_SAMPLES]).unwrap() > 0);

        s.simulate_device_loss(&DeviceId("far-1".into()));
        assert!(matches!(
            s.read_frames(h, &mut vec![0i16; FRAME_SAMPLES]),
            Err(CaptureError::DeviceInvalidated)
        ));
        let events: Vec<_> = rx.try_iter().collect();
        assert!(events.iter().any(|e| matches!(e, DeviceEvent::StateChanged { active: false, .. })));
        assert!(events.iter().any(|e| matches!(e, DeviceEvent::Removed(_))));
    }

    #[test]
    fn a_replugged_device_can_be_reopened() {
        let mut s = source();
        let id = DeviceId("far-1".into());
        s.simulate_device_loss(&id);
        assert!(matches!(s.open(&id, Direction::Render), Err(CaptureError::DeviceInvalidated)));
        s.simulate_device_restore(&id);
        assert!(s.open(&id, Direction::Render).is_ok());
    }

    #[test]
    fn glitches_produce_short_reads() {
        let mut s = WavReplaySource::new().without_realtime_pacing().with_glitch_every(3_200);
        s.add_samples("far-1", "c", "n", Direction::Render, ramp(16_000));
        let h = s.open(&DeviceId("far-1".into()), Direction::Render).unwrap();
        let mut buf = vec![0i16; FRAME_SAMPLES * 10];
        let mut short_reads = 0;
        loop {
            let n = s.read_frames(h, &mut buf).unwrap();
            if n == 0 {
                break;
            }
            if n < buf.len() && s.remaining(h) > 0 {
                short_reads += 1;
            }
        }
        assert!(short_reads > 0, "glitch injection should produce short reads");
    }

    #[test]
    fn realtime_pacing_does_not_hand_over_the_future() {
        let mut s = WavReplaySource::new();
        s.add_samples("far-1", "c", "n", Direction::Render, ramp(16_000 * 5));
        let h = s.open(&DeviceId("far-1".into()), Direction::Render).unwrap();
        let mut buf = vec![0i16; 16_000 * 5];
        let n = s.read_frames(h, &mut buf).unwrap();
        assert!(n < 16_000, "a paced source cannot deliver 5 s of audio immediately, got {n}");
    }

    #[test]
    fn closing_an_unknown_handle_is_an_error_not_a_panic() {
        let mut s = source();
        assert!(matches!(s.close(StreamHandle(999)), Err(CaptureError::BadHandle(999))));
    }
}
