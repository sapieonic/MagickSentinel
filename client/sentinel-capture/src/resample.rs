//! Resampling to 16 kHz mono `i16`.
//!
//! Tier A asks the device for 16 kHz mono directly — process loopback does not honour
//! the device mix format, so we request exactly what we want and get it. Tier B has no
//! such option: endpoint loopback runs at the device mix format, typically 48 kHz
//! stereo float, and must be converted here.
//!
//! The converter is a windowed-sinc polyphase resampler with a persistent history
//! buffer, so consecutive calls stitch without a click at the boundary. That
//! statefulness is the whole point: a per-buffer resampler introduces a discontinuity
//! every 10 ms, which the VAD reads as onsets and the encoder spends bits on.

/// Downmix interleaved channels to mono by averaging.
pub fn downmix_to_mono(interleaved: &[f32], channels: usize) -> Vec<f32> {
    if channels <= 1 {
        return interleaved.to_vec();
    }
    interleaved
        .chunks_exact(channels)
        .map(|f| f.iter().sum::<f32>() / channels as f32)
        .collect()
}

pub fn f32_to_i16(samples: &[f32]) -> Vec<i16> {
    samples
        .iter()
        .map(|&v| (v.clamp(-1.0, 1.0) * i16::MAX as f32) as i16)
        .collect()
}

pub fn i16_to_f32(samples: &[i16]) -> Vec<f32> {
    samples.iter().map(|&v| v as f32 / i16::MAX as f32).collect()
}

/// Half-width of the sinc kernel, in output-rate samples.
const TAPS: usize = 16;

/// A stateful sample-rate converter.
pub struct Resampler {
    src_rate: u32,
    dst_rate: u32,
    /// Fractional read position within `history`, in source samples.
    pos: f64,
    history: Vec<f32>,
    /// Normalised cutoff, relative to the source Nyquist. Below 1.0 when
    /// downsampling, to keep the anti-alias filter below the *output* Nyquist.
    cutoff: f64,
}

impl Resampler {
    pub fn new(src_rate: u32, dst_rate: u32) -> Self {
        let ratio = dst_rate as f64 / src_rate as f64;
        Resampler {
            src_rate,
            dst_rate,
            pos: TAPS as f64,
            history: vec![0.0; TAPS * 2],
            cutoff: ratio.min(1.0) * 0.94,
        }
    }

    pub fn is_identity(&self) -> bool {
        self.src_rate == self.dst_rate
    }

    /// Convert one buffer, carrying state across calls.
    pub fn process(&mut self, input: &[f32]) -> Vec<f32> {
        if self.is_identity() {
            return input.to_vec();
        }
        self.history.extend_from_slice(input);

        let step = self.src_rate as f64 / self.dst_rate as f64;
        let mut out = Vec::with_capacity((input.len() as f64 / step) as usize + 1);

        // Stop early enough that the kernel never reads past what we have.
        let limit = self.history.len() as f64 - TAPS as f64 - 1.0;
        while self.pos < limit {
            out.push(self.sample_at(self.pos));
            self.pos += step;
        }

        // Drop consumed history, keeping the kernel's left context.
        let keep_from = (self.pos as usize).saturating_sub(TAPS);
        if keep_from > 0 {
            self.history.drain(..keep_from);
            self.pos -= keep_from as f64;
        }
        out
    }

    fn sample_at(&self, pos: f64) -> f32 {
        let centre = pos.floor() as isize;
        let frac = pos - centre as f64;
        let mut acc = 0.0f64;
        let mut norm = 0.0f64;
        for k in -(TAPS as isize) + 1..=TAPS as isize {
            let idx = centre + k;
            if idx < 0 || idx as usize >= self.history.len() {
                continue;
            }
            let x = k as f64 - frac;
            let w = blackman(x / TAPS as f64);
            let s = sinc(x * self.cutoff) * self.cutoff * w;
            acc += s * self.history[idx as usize] as f64;
            norm += s;
        }
        if norm.abs() < 1e-12 {
            0.0
        } else {
            (acc / norm) as f32
        }
    }
}

fn sinc(x: f64) -> f64 {
    if x.abs() < 1e-9 {
        1.0
    } else {
        let pix = std::f64::consts::PI * x;
        pix.sin() / pix
    }
}

fn blackman(t: f64) -> f64 {
    // t in [-1, 1]
    if t.abs() >= 1.0 {
        return 0.0;
    }
    let u = std::f64::consts::PI * (t + 1.0);
    0.42 - 0.5 * u.cos() + 0.08 * (2.0 * u).cos()
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::f64::consts::TAU;

    fn tone(freq: f64, rate: u32, n: usize) -> Vec<f32> {
        (0..n)
            .map(|i| (TAU * freq * i as f64 / rate as f64).sin() as f32)
            .collect()
    }

    fn rms(v: &[f32]) -> f32 {
        (v.iter().map(|x| x * x).sum::<f32>() / v.len().max(1) as f32).sqrt()
    }

    #[test]
    fn downmix_averages_channels() {
        let stereo = vec![1.0, 0.0, 0.5, 0.5, -1.0, 1.0];
        assert_eq!(downmix_to_mono(&stereo, 2), vec![0.5, 0.5, 0.0]);
        assert_eq!(downmix_to_mono(&[0.25, 0.5], 1), vec![0.25, 0.5]);
    }

    #[test]
    fn identity_rate_passes_through_untouched() {
        let mut r = Resampler::new(16_000, 16_000);
        assert!(r.is_identity());
        let input = tone(440.0, 16_000, 320);
        assert_eq!(r.process(&input), input);
    }

    #[test]
    fn downsampling_48k_to_16k_produces_a_third_of_the_samples() {
        let mut r = Resampler::new(48_000, 16_000);
        let mut total = 0;
        for _ in 0..10 {
            total += r.process(&tone(300.0, 48_000, 480)).len();
        }
        // 10 × 480 input samples at 3:1 is 1600 output samples, within a tap or two.
        assert!((total as i64 - 1600).abs() <= 4, "got {total}");
    }

    #[test]
    fn a_speech_band_tone_survives_downsampling() {
        let mut r = Resampler::new(48_000, 16_000);
        let mut out = Vec::new();
        for _ in 0..20 {
            out.extend(r.process(&tone(400.0, 48_000, 480)));
        }
        // Skip the filter's start-up transient.
        let steady = &out[100..];
        let level = rms(steady);
        assert!((level - 0.707).abs() < 0.06, "amplitude not preserved: rms {level}");
    }

    #[test]
    fn content_above_the_output_nyquist_is_attenuated() {
        // 10 kHz cannot be represented at 16 kHz; it must be filtered, not aliased
        // down into the speech band where it would corrupt the VAD and the ASR.
        let mut r = Resampler::new(48_000, 16_000);
        let mut out = Vec::new();
        for _ in 0..20 {
            out.extend(r.process(&tone(10_000.0, 48_000, 480)));
        }
        let level = rms(&out[100..]);
        assert!(level < 0.1, "aliasing not suppressed: rms {level}");
    }

    #[test]
    fn buffer_boundaries_do_not_click() {
        // Process one continuous tone in small chunks and check there is no
        // discontinuity where the chunks meet.
        let full = tone(300.0, 48_000, 48_000 / 10);
        let mut r = Resampler::new(48_000, 16_000);
        let mut out = Vec::new();
        for chunk in full.chunks(480) {
            out.extend(r.process(chunk));
        }
        let steady = &out[64..];
        let max_step = steady
            .windows(2)
            .map(|w| (w[1] - w[0]).abs())
            .fold(0.0f32, f32::max);
        // A 300 Hz tone at 16 kHz moves at most ~0.12 per sample; a click would be
        // far larger.
        assert!(max_step < 0.25, "discontinuity at a chunk boundary: {max_step}");
    }

    #[test]
    fn conversion_between_i16_and_f32_round_trips() {
        let orig: Vec<i16> = vec![0, 1, -1, 1000, -1000, i16::MAX, i16::MIN + 1];
        let back = f32_to_i16(&i16_to_f32(&orig));
        for (a, b) in orig.iter().zip(back.iter()) {
            assert!((a - b).abs() <= 1, "{a} vs {b}");
        }
    }

    #[test]
    fn clipping_is_clamped_not_wrapped() {
        assert_eq!(f32_to_i16(&[2.0, -2.0]), vec![i16::MAX, -i16::MAX]);
    }
}
