//! Opus encoding and segmentation (spec 6.5).
//!
//! 16 kHz mono, 24 kbps, 20 ms frames; 50 frames packed into a one-second segment;
//! 10 segments batched per WebSocket message. One encoder per channel — the two are
//! never mixed, because separate channels give exact speaker attribution with no
//! diarization step.
//!
//! Encoder state is per-stream and stateful: Opus frames depend on their predecessors,
//! so feeding both channels through one encoder would produce a decodable stream that
//! is quietly wrong. Two encoders, always.

use sentinel_capture::{FRAME_MS, FRAME_SAMPLES, SAMPLE_RATE};
use sentinel_core::protocol::{pack_segment, Channel, MediaFlags, FRAMES_PER_SEGMENT};

/// Target bitrate per channel. 24 kbps is ≈60 bytes per 20 ms frame; two channels are
/// ≈6 KB/s, and 200 concurrent agents ≈1.2 MB/s aggregate upstream.
pub const BITRATE_BPS: i32 = 24_000;

/// Milliseconds of audio in one segment.
pub const SEGMENT_MS: u64 = FRAMES_PER_SEGMENT as u64 * FRAME_MS as u64;

/// Samples in one segment, per channel.
pub const SEGMENT_SAMPLES: usize = FRAMES_PER_SEGMENT * FRAME_SAMPLES;

/// Largest packet Opus can produce at these settings, with headroom.
const MAX_PACKET: usize = 400;

#[derive(Debug, thiserror::Error)]
pub enum EncodeError {
    #[error("opus encoder error: {0}")]
    Opus(String),
}

/// One finished segment, ready to spool.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Segment {
    pub channel: Channel,
    pub seq: u32,
    /// Call-relative, from the single call-scoped clock shared by both channels.
    pub timestamp_ms: u64,
    pub flags: MediaFlags,
    /// 50 length-delimited Opus packets.
    pub payload: Vec<u8>,
}

/// Encodes one channel's PCM into sequenced segments.
pub struct SegmentEncoder {
    encoder: audiopus::coder::Encoder,
    channel: Channel,
    /// Samples not yet forming a whole 20 ms frame.
    pending: Vec<i16>,
    /// Encoded frames accumulating toward the next segment.
    frames: Vec<Vec<u8>>,
    next_seq: u32,
    /// Call-relative timestamp of the first frame in the segment being built.
    segment_start_ms: u64,
    /// Set when a glitch gap was filled inside the segment being built.
    silence_inserted: bool,
    /// Set when the segment being built is tier-B foreign audio.
    foreign: bool,
}

impl SegmentEncoder {
    pub fn new(channel: Channel) -> Result<Self, EncodeError> {
        let mut encoder = audiopus::coder::Encoder::new(
            audiopus::SampleRate::Hz16000,
            audiopus::Channels::Mono,
            // Voip rather than Audio: it is tuned for speech intelligibility over
            // music fidelity, which is the whole content of a collections call, and
            // it is what keeps numeric entities recoverable at 24 kbps.
            audiopus::Application::Voip,
        )
        .map_err(|e| EncodeError::Opus(e.to_string()))?;
        encoder
            .set_bitrate(audiopus::Bitrate::BitsPerSecond(BITRATE_BPS))
            .map_err(|e| EncodeError::Opus(e.to_string()))?;
        // Inband FEC plus an expected packet loss figure: segments travel over a
        // reliable WebSocket, but the decoder side also reads spooled audio that may
        // have had frames dropped by a capture glitch.
        let _ = encoder.set_inband_fec(true);
        let _ = encoder.set_packet_loss_perc(5);

        Ok(SegmentEncoder {
            encoder,
            channel,
            pending: Vec::with_capacity(FRAME_SAMPLES),
            frames: Vec::with_capacity(FRAMES_PER_SEGMENT),
            next_seq: 0,
            segment_start_ms: 0,
            silence_inserted: false,
            foreign: false,
        })
    }

    pub fn channel(&self) -> Channel {
        self.channel
    }

    /// Sequence number the next completed segment will carry.
    pub fn next_seq(&self) -> u32 {
        self.next_seq
    }

    /// Mark the segment currently being built as tier-B foreign audio.
    ///
    /// Sticky for the segment: if any part of a second was foreign, the whole second
    /// is flagged. Erring toward flagging costs a second of transcription; erring the
    /// other way transcribes somebody's music.
    pub fn mark_foreign(&mut self) {
        self.foreign = true;
    }

    /// Feed PCM at 16 kHz mono. Returns any segments completed by this call.
    pub fn push_samples(&mut self, samples: &[i16]) -> Result<Vec<Segment>, EncodeError> {
        let mut out = Vec::new();
        let mut offset = 0;
        while offset < samples.len() {
            let want = FRAME_SAMPLES - self.pending.len();
            let take = want.min(samples.len() - offset);
            self.pending.extend_from_slice(&samples[offset..offset + take]);
            offset += take;
            if self.pending.len() == FRAME_SAMPLES {
                let frame = self.encode_frame()?;
                self.frames.push(frame);
                self.pending.clear();
                if self.frames.len() == FRAMES_PER_SEGMENT {
                    out.push(self.emit());
                }
            }
        }
        Ok(out)
    }

    /// Fill a capture glitch with synthesised silence, keeping the two channels'
    /// timestamps aligned.
    ///
    /// `AUDCLNT_BUFFERFLAGS_DATA_DISCONTINUITY` says samples were lost. Skipping them
    /// silently shortens one channel relative to the other, and every word after the
    /// glitch lands at the wrong time in the transcript alignment.
    pub fn insert_silence(&mut self, gap_ms: u64) -> Result<Vec<Segment>, EncodeError> {
        if gap_ms == 0 {
            return Ok(Vec::new());
        }
        self.silence_inserted = true;
        let samples = (gap_ms as usize * SAMPLE_RATE as usize) / 1000;
        let silence = vec![0i16; samples];
        self.push_samples(&silence)
    }

    /// Close out a partial segment at the end of a call.
    ///
    /// Padded to a full 50 frames with silence rather than emitted short: the wire
    /// format defines a segment as 50 packets, and a short one would decode as a
    /// second that is not a second, shifting every subsequent timestamp.
    pub fn flush(&mut self) -> Result<Option<Segment>, EncodeError> {
        if self.pending.is_empty() && self.frames.is_empty() {
            return Ok(None);
        }
        if !self.pending.is_empty() {
            self.pending.resize(FRAME_SAMPLES, 0);
            let frame = self.encode_frame()?;
            self.frames.push(frame);
            self.pending.clear();
            self.silence_inserted = true;
        }
        while self.frames.len() < FRAMES_PER_SEGMENT {
            // A zero-length packet is the wire format's "dropped frame"; the decoder
            // inserts 20 ms of silence for it. Cheaper than encoding real silence and
            // unambiguous about what happened.
            self.frames.push(Vec::new());
            self.silence_inserted = true;
        }
        Ok(Some(self.emit()))
    }

    fn encode_frame(&mut self) -> Result<Vec<u8>, EncodeError> {
        let mut buf = vec![0u8; MAX_PACKET];
        let n = self
            .encoder
            .encode(&self.pending, &mut buf)
            .map_err(|e| EncodeError::Opus(e.to_string()))?;
        buf.truncate(n);
        Ok(buf)
    }

    fn emit(&mut self) -> Segment {
        let payload = pack_segment(&self.frames);
        let seg = Segment {
            channel: self.channel,
            seq: self.next_seq,
            timestamp_ms: self.segment_start_ms,
            flags: MediaFlags {
                foreign: self.foreign,
                silence_inserted: self.silence_inserted,
            },
            payload,
        };
        self.frames.clear();
        self.next_seq += 1;
        self.segment_start_ms += SEGMENT_MS;
        self.silence_inserted = false;
        self.foreign = false;
        seg
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use sentinel_core::protocol::unpack_segment;
    use std::f32::consts::TAU;

    fn tone(ms: usize) -> Vec<i16> {
        let n = ms * SAMPLE_RATE as usize / 1000;
        (0..n)
            .map(|i| {
                let t = i as f32 / SAMPLE_RATE as f32;
                (0.3 * i16::MAX as f32 * (TAU * 300.0 * t).sin()) as i16
            })
            .collect()
    }

    #[test]
    fn one_second_of_audio_is_exactly_one_segment_of_fifty_frames() {
        let mut e = SegmentEncoder::new(Channel::Far).unwrap();
        let segs = e.push_samples(&tone(1000)).unwrap();
        assert_eq!(segs.len(), 1);
        let frames = unpack_segment(&segs[0].payload).unwrap();
        assert_eq!(frames.len(), FRAMES_PER_SEGMENT);
        assert!(frames.iter().all(|f| !f.is_empty()), "no dropped frames in clean audio");
        assert_eq!(segs[0].seq, 0);
        assert_eq!(segs[0].timestamp_ms, 0);
        assert_eq!(segs[0].channel, Channel::Far);
        assert_eq!(segs[0].flags, MediaFlags::default());
    }

    #[test]
    fn sequence_numbers_and_timestamps_advance_by_one_second() {
        let mut e = SegmentEncoder::new(Channel::Near).unwrap();
        let segs = e.push_samples(&tone(3000)).unwrap();
        assert_eq!(segs.len(), 3);
        assert_eq!(segs.iter().map(|s| s.seq).collect::<Vec<_>>(), [0, 1, 2]);
        assert_eq!(
            segs.iter().map(|s| s.timestamp_ms).collect::<Vec<_>>(),
            [0, 1000, 2000]
        );
        assert_eq!(e.next_seq(), 3);
    }

    #[test]
    fn samples_arriving_in_awkward_chunks_still_produce_whole_segments() {
        // WASAPI hands over whatever is in the endpoint buffer, which is never a neat
        // multiple of a 20 ms frame.
        let mut e = SegmentEncoder::new(Channel::Far).unwrap();
        let audio = tone(2000);
        let mut all = Vec::new();
        for chunk in audio.chunks(377) {
            all.extend(e.push_samples(chunk).unwrap());
        }
        assert_eq!(all.len(), 2);
        for s in &all {
            assert_eq!(unpack_segment(&s.payload).unwrap().len(), FRAMES_PER_SEGMENT);
        }
    }

    #[test]
    fn the_bitrate_lands_near_the_specified_24_kbps() {
        // ≈60 bytes per 20 ms frame; the packing overhead is 2 bytes per frame.
        let mut e = SegmentEncoder::new(Channel::Far).unwrap();
        let segs = e.push_samples(&tone(5000)).unwrap();
        let payload_bytes: usize = segs.iter().map(|s| s.payload.len()).sum();
        let bits_per_second = (payload_bytes as f64 * 8.0) / 5.0;
        assert!(
            (15_000.0..40_000.0).contains(&bits_per_second),
            "expected roughly 24 kbps, got {bits_per_second:.0} bps"
        );
    }

    #[test]
    fn a_glitch_gap_is_filled_and_flagged() {
        // Skipping the lost samples instead would shorten one channel relative to the
        // other and shift every later word in the transcript alignment.
        let mut e = SegmentEncoder::new(Channel::Far).unwrap();
        e.push_samples(&tone(500)).unwrap();
        let segs = e.insert_silence(500).unwrap();
        assert_eq!(segs.len(), 1, "the gap completes the first second");
        assert!(segs[0].flags.silence_inserted);
        assert!(!segs[0].flags.foreign);

        // The flag does not leak into the next segment.
        let next = e.push_samples(&tone(1000)).unwrap();
        assert_eq!(next.len(), 1);
        assert!(!next[0].flags.silence_inserted);
    }

    #[test]
    fn a_second_containing_any_foreign_audio_is_flagged_whole() {
        // Tier B: erring toward flagging costs a second of transcription; erring the
        // other way transcribes somebody's music.
        let mut e = SegmentEncoder::new(Channel::Far).unwrap();
        e.push_samples(&tone(200)).unwrap();
        e.mark_foreign();
        let segs = e.push_samples(&tone(800)).unwrap();
        assert_eq!(segs.len(), 1);
        assert!(segs[0].flags.foreign);

        let next = e.push_samples(&tone(1000)).unwrap();
        assert!(!next[0].flags.foreign, "the flag is per segment, not sticky forever");
    }

    #[test]
    fn flush_pads_a_partial_segment_to_a_full_second() {
        // The wire format defines a segment as 50 packets. A short one would decode
        // as a second that is not a second.
        let mut e = SegmentEncoder::new(Channel::Near).unwrap();
        e.push_samples(&tone(230)).unwrap();
        let seg = e.flush().unwrap().expect("a partial segment is flushed");
        let frames = unpack_segment(&seg.payload).unwrap();
        assert_eq!(frames.len(), FRAMES_PER_SEGMENT);
        assert!(seg.flags.silence_inserted, "the padding is declared, not hidden");
        assert!(frames[0].len() > 0, "the real audio survives");
        assert!(frames[FRAMES_PER_SEGMENT - 1].is_empty(), "the tail is dropped-frame padding");
    }

    #[test]
    fn flushing_an_empty_encoder_produces_nothing() {
        let mut e = SegmentEncoder::new(Channel::Far).unwrap();
        assert_eq!(e.flush().unwrap(), None);
        e.push_samples(&tone(1000)).unwrap();
        assert_eq!(e.flush().unwrap(), None, "a segment boundary leaves nothing pending");
    }

    #[test]
    fn the_two_channels_keep_independent_sequence_numbers() {
        let mut far = SegmentEncoder::new(Channel::Far).unwrap();
        let mut near = SegmentEncoder::new(Channel::Near).unwrap();
        far.push_samples(&tone(3000)).unwrap();
        let n = near.push_samples(&tone(1000)).unwrap();
        assert_eq!(far.next_seq(), 3);
        assert_eq!(near.next_seq(), 1);
        assert_eq!(n[0].seq, 0, "the near channel starts at 0 regardless of the far channel");
    }

    #[test]
    fn silence_encodes_to_something_much_smaller_than_speech() {
        // Not a correctness requirement, but it is what keeps the spool inside its
        // caps on a floor where agents spend half a call listening.
        let mut quiet = SegmentEncoder::new(Channel::Far).unwrap();
        let mut loud = SegmentEncoder::new(Channel::Far).unwrap();
        let q = quiet.push_samples(&vec![0i16; SEGMENT_SAMPLES]).unwrap();
        let l = loud.push_samples(&tone(1000)).unwrap();
        assert!(
            q[0].payload.len() * 4 < l[0].payload.len(),
            "silence {} bytes vs speech {} bytes",
            q[0].payload.len(),
            l[0].payload.len()
        );
    }
}
