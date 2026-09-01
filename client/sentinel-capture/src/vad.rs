//! Voice activity detection.
//!
//! Used for two things: confirming a human is present before opening a call (rather
//! than ringback or hold music), and driving foreign-audio suppression on tier B.
//!
//! This is an energy-plus-zero-crossing detector with an adaptive noise floor and
//! hangover. It is deliberately simple and dependency-free: it decides *whether to
//! record*, and the real speech/non-speech judgement happens server-side where a
//! proper model runs. Swapping in WebRTC VAD or Silero later only changes this file.

use crate::FRAME_SAMPLES;

#[derive(Debug, Clone, Copy)]
pub struct VadParams {
    /// How far above the adaptive noise floor a frame must sit to count as speech.
    pub speech_margin_db: f32,
    /// Absolute floor: below this a frame is silence no matter what the noise
    /// estimate says. Prevents a dead-quiet line from adapting itself into hearing
    /// speech in its own dither.
    pub absolute_floor_db: f32,
    /// Frames of speech required before the detector flips to voiced.
    pub onset_frames: u32,
    /// Frames of silence tolerated before it flips back. Speech has gaps; without
    /// hangover every inter-word pause reads as the end of the call.
    pub hangover_frames: u32,
    /// Zero-crossing rate above which a frame is treated as noise rather than voice,
    /// which keeps keyboard clatter and line hiss out.
    pub max_zcr: f32,
}

impl Default for VadParams {
    fn default() -> Self {
        VadParams {
            speech_margin_db: 9.0,
            absolute_floor_db: -55.0,
            onset_frames: 2,
            hangover_frames: 25, // 500 ms at 20 ms frames
            max_zcr: 0.45,
        }
    }
}

#[derive(Debug, Clone)]
pub struct Vad {
    params: VadParams,
    noise_db: f32,
    initialised: bool,
    speech_run: u32,
    silence_run: u32,
    voiced: bool,
}

impl Default for Vad {
    fn default() -> Self {
        Vad::new(VadParams::default())
    }
}

impl Vad {
    pub fn new(params: VadParams) -> Self {
        Vad {
            params,
            noise_db: -60.0,
            initialised: false,
            speech_run: 0,
            silence_run: 0,
            voiced: false,
        }
    }

    pub fn is_voiced(&self) -> bool {
        self.voiced
    }

    pub fn noise_floor_db(&self) -> f32 {
        self.noise_db
    }

    /// Feed one 20 ms frame. Returns the current voiced state.
    pub fn push_frame(&mut self, frame: &[i16]) -> bool {
        let db = frame_db(frame);
        let zcr = zero_crossing_rate(frame);

        if !self.initialised {
            self.noise_db = db;
            self.initialised = true;
        }

        let loud_enough = db > self.params.absolute_floor_db
            && db > self.noise_db + self.params.speech_margin_db;
        let speechlike = loud_enough && zcr <= self.params.max_zcr;

        if speechlike {
            self.speech_run += 1;
            self.silence_run = 0;
            if self.speech_run >= self.params.onset_frames {
                self.voiced = true;
            }
        } else {
            self.silence_run += 1;
            self.speech_run = 0;
            if self.silence_run >= self.params.hangover_frames {
                self.voiced = false;
            }
            // Adapt the noise floor only on non-speech frames, and only downward
            // quickly: a loud stretch of speech must not drag the floor up until the
            // detector goes deaf.
            let alpha = if db < self.noise_db { 0.25 } else { 0.02 };
            self.noise_db += alpha * (db - self.noise_db);
        }

        self.voiced
    }

    /// Convenience: run a whole buffer, frame by frame, and report the milliseconds
    /// judged voiced.
    pub fn voiced_ms(&mut self, samples: &[i16]) -> u64 {
        let mut ms = 0;
        for frame in samples.chunks(FRAME_SAMPLES) {
            if frame.len() < FRAME_SAMPLES {
                break;
            }
            if self.push_frame(frame) {
                ms += crate::FRAME_MS as u64;
            }
        }
        ms
    }

    pub fn reset(&mut self) {
        let params = self.params;
        *self = Vad::new(params);
    }
}

/// RMS level of a frame in dBFS.
pub fn frame_db(frame: &[i16]) -> f32 {
    if frame.is_empty() {
        return -120.0;
    }
    let sum: f64 = frame.iter().map(|&s| {
        let v = s as f64 / i16::MAX as f64;
        v * v
    }).sum();
    let rms = (sum / frame.len() as f64).sqrt();
    if rms <= 1e-9 {
        -120.0
    } else {
        20.0 * rms.log10() as f32
    }
}

fn zero_crossing_rate(frame: &[i16]) -> f32 {
    if frame.len() < 2 {
        return 0.0;
    }
    let crossings = frame
        .windows(2)
        .filter(|w| (w[0] >= 0) != (w[1] >= 0))
        .count();
    crossings as f32 / (frame.len() - 1) as f32
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::f32::consts::TAU;

    fn tone(freq: f32, amplitude: f32, frames: usize) -> Vec<i16> {
        let n = frames * FRAME_SAMPLES;
        (0..n)
            .map(|i| {
                let t = i as f32 / crate::SAMPLE_RATE as f32;
                (amplitude * i16::MAX as f32 * (TAU * freq * t).sin()) as i16
            })
            .collect()
    }

    fn silence(frames: usize) -> Vec<i16> {
        vec![0; frames * FRAME_SAMPLES]
    }

    fn quiet_noise(frames: usize) -> Vec<i16> {
        // Deterministic low-level dither, roughly -60 dBFS.
        let n = frames * FRAME_SAMPLES;
        (0..n).map(|i| (((i * 7919) % 61) as i16) - 30).collect()
    }

    #[test]
    fn silence_is_never_voiced() {
        let mut vad = Vad::default();
        assert_eq!(vad.voiced_ms(&silence(100)), 0);
        assert!(!vad.is_voiced());
    }

    #[test]
    fn a_speech_level_tone_is_voiced() {
        let mut vad = Vad::default();
        vad.voiced_ms(&quiet_noise(50)); // let the floor settle
        let ms = vad.voiced_ms(&tone(300.0, 0.25, 50));
        assert!(ms >= 900, "expected most of 1 s to be voiced, got {ms} ms");
    }

    #[test]
    fn hangover_bridges_the_gaps_between_words() {
        let mut vad = Vad::default();
        vad.voiced_ms(&quiet_noise(50));
        let mut speech = tone(300.0, 0.25, 10);
        speech.extend(silence(8)); // 160 ms pause, shorter than the 500 ms hangover
        speech.extend(tone(300.0, 0.25, 10));
        let ms = vad.voiced_ms(&speech);
        assert!(ms >= 500, "hangover should carry through a short pause, got {ms} ms");
        assert!(vad.is_voiced());
    }

    #[test]
    fn a_long_silence_ends_the_voiced_run() {
        let mut vad = Vad::default();
        vad.voiced_ms(&quiet_noise(50));
        vad.voiced_ms(&tone(300.0, 0.3, 20));
        assert!(vad.is_voiced());
        vad.voiced_ms(&silence(60)); // 1.2 s, well past the hangover
        assert!(!vad.is_voiced());
    }

    #[test]
    fn the_noise_floor_adapts_without_going_deaf_to_speech() {
        // A noisy collections floor: constant background, then someone talks.
        let mut vad = Vad::default();
        let background: Vec<i16> = (0..50 * FRAME_SAMPLES)
            .map(|i| (((i * 7919) % 2001) as i16) - 1000)
            .collect();
        vad.voiced_ms(&background);
        assert!(!vad.is_voiced(), "steady background must not read as speech");
        let ms = vad.voiced_ms(&tone(280.0, 0.35, 40));
        assert!(ms >= 600, "speech over background should still register, got {ms} ms");
    }

    #[test]
    fn high_zero_crossing_hiss_is_rejected() {
        let mut vad = Vad::default();
        vad.voiced_ms(&quiet_noise(50));
        // Alternating full-scale samples: loud, but ZCR of 1.0.
        let hiss: Vec<i16> = (0..40 * FRAME_SAMPLES)
            .map(|i| if i % 2 == 0 { 12000 } else { -12000 })
            .collect();
        assert_eq!(vad.voiced_ms(&hiss), 0);
    }

    #[test]
    fn frame_db_is_monotonic_in_amplitude() {
        let quiet = frame_db(&tone(300.0, 0.01, 1));
        let loud = frame_db(&tone(300.0, 0.5, 1));
        assert!(loud > quiet + 20.0, "{loud} vs {quiet}");
        assert_eq!(frame_db(&[]), -120.0);
        assert_eq!(frame_db(&[0; 320]), -120.0);
    }
}
