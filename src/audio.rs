//! Stage 4b, Opus.
//!
//! `rusty-opus` is a pure-Rust implementation, which after the NVENC episode is
//! worth calling out: it pulls in no C toolchain, no vendored source tree, and
//! no transitive dependencies at all. It also means everything in this module
//! is genuinely testable on any machine, with no GPU and no audio hardware.
//!
//! The one structural job here is *framing*. WASAPI hands us buffers of
//! whatever size it feels like; Opus insists on exact frame durations. So we
//! accumulate into a residue buffer and emit only whole 20 ms frames.
//!
//! Timestamps come from the running sample count rather than a wall clock, and
//! that is only correct because `loopback::Capture::pump` gap-fills silence.
//! Because the sample stream is continuous by construction, counting samples
//! *is* counting time, and it cannot drift the way repeatedly reading a clock
//! would.

use rusty_opus::{Application, OpusEncoder};

use crate::pipeline::AudioEncoder;

pub const SAMPLE_RATE: i32 = 48_000;
pub const CHANNELS: usize = 2;

/// 20 ms at 48 kHz, per channel. Opus accepts 2.5/5/10/20/40/60 ms; 20 is the
/// usual WebRTC choice and the best latency-to-overhead trade here.
pub const FRAME_SAMPLES: usize = 960;

/// Opus packets never exceed 1275 bytes per frame, but the encoder is entitled
/// to the room, so give it slack.
const MAX_PACKET: usize = 4000;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct OpusPacket {
    pub data: Vec<u8>,
    /// Microseconds from the start of the stream, derived from sample count.
    pub timestamp_us: u64,
}

pub struct OpusStream {
    enc: OpusEncoder,
    /// Interleaved samples not yet forming a whole frame.
    residue: Vec<f32>,
    /// Frames emitted so far, the clock.
    frames_emitted: u64,
    bytes: u64,
    scratch: Vec<u8>,
}

impl OpusStream {
    pub fn new(bitrate_bps: i32) -> Result<Self, String> {
        // Audio, not Voip: this carries game audio and music, not speech, and
        // the Voip mode's speech tuning would audibly hurt it.
        let mut enc = OpusEncoder::new(SAMPLE_RATE, CHANNELS, Application::Audio)
            .map_err(|e| format!("could not create Opus encoder: {e}"))?;

        enc.bitrate_bps = bitrate_bps;

        // CBR keeps packet sizes predictable, which keeps the congestion
        // controller's job simple and avoids bitrate spikes competing with
        // the video stream for the same uplink.
        enc.use_cbr = true;

        // Inband FEC costs a little bitrate and buys recovery from isolated
        // packet loss. Audio dropouts are far more noticeable than the cost.
        enc.use_inband_fec = true;

        // DTX would emit 1-byte packets during silence. That directly undoes
        // the gap-filling in loopback.rs, the whole point of which is a
        // continuous, evenly-paced stream. Off.
        enc.use_dtx = false;

        Ok(Self {
            enc,
            residue: Vec::with_capacity(FRAME_SAMPLES * CHANNELS * 2),
            frames_emitted: 0,
            bytes: 0,
            scratch: vec![0u8; MAX_PACKET],
        })
    }

    /// Feeds interleaved 16-bit PCM straight from the loopback capture and
    /// returns whatever whole frames that completed. A short buffer yields
    /// nothing and is held until the rest arrives.
    pub fn push(&mut self, pcm: &[i16]) -> Result<Vec<OpusPacket>, String> {
        self.residue.extend(pcm.iter().map(|s| *s as f32 / 32768.0));

        let frame_len = FRAME_SAMPLES * CHANNELS;
        let mut out = Vec::new();

        while self.residue.len() >= frame_len {
            let n = self
                .enc
                .encode(&self.residue[..frame_len], FRAME_SAMPLES, &mut self.scratch)
                .map_err(|e| format!("Opus encode failed: {e}"))?;

            out.push(OpusPacket {
                data: self.scratch[..n].to_vec(),
                timestamp_us: self.frames_emitted * frame_duration_us(),
            });

            self.residue.drain(..frame_len);
            self.frames_emitted += 1;
            self.bytes += n as u64;
        }

        Ok(out)
    }

    /// Samples buffered but not yet forming a whole frame. Never more than one
    /// frame's worth. Diagnostic, the framing tests assert on it.
    #[allow(dead_code)]
    pub fn pending_samples(&self) -> usize {
        self.residue.len()
    }

    pub fn frames_emitted(&self) -> u64 {
        self.frames_emitted
    }

    pub fn bytes_emitted(&self) -> u64 {
        self.bytes
    }

    /// Measured over everything emitted so far.
    pub fn average_bitrate_bps(&self) -> f64 {
        if self.frames_emitted == 0 {
            return 0.0;
        }
        let seconds = (self.frames_emitted * frame_duration_us()) as f64 / 1_000_000.0;
        (self.bytes * 8) as f64 / seconds
    }
}

const fn frame_duration_us() -> u64 {
    (FRAME_SAMPLES as u64) * 1_000_000 / (SAMPLE_RATE as u64)
}

impl AudioEncoder for OpusStream {
    fn submit(&mut self, pcm: &[i16]) -> Result<(), String> {
        self.push(pcm).map(|_| ())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const FRAME_LEN: usize = FRAME_SAMPLES * CHANNELS;

    /// A quiet tone, real signal, so the encoder has something to chew on.
    fn tone(samples: usize) -> Vec<i16> {
        (0..samples)
            .map(|i| {
                let t = i as f32 / SAMPLE_RATE as f32;
                ((t * 440.0 * std::f32::consts::TAU).sin() * 8000.0) as i16
            })
            .collect()
    }

    #[test]
    fn frame_duration_is_20ms() {
        assert_eq!(frame_duration_us(), 20_000);
    }

    #[test]
    fn exact_frame_yields_exactly_one_packet() {
        let mut s = OpusStream::new(128_000).unwrap();
        let packets = s.push(&tone(FRAME_LEN)).unwrap();
        assert_eq!(packets.len(), 1);
        assert!(!packets[0].data.is_empty(), "packet should carry data");
        assert_eq!(packets[0].timestamp_us, 0);
        assert_eq!(s.pending_samples(), 0);
    }

    #[test]
    fn short_buffer_is_held_not_dropped() {
        let mut s = OpusStream::new(128_000).unwrap();
        let packets = s.push(&tone(FRAME_LEN / 2)).unwrap();
        assert!(packets.is_empty(), "half a frame should emit nothing");
        assert_eq!(s.pending_samples(), FRAME_LEN / 2);

        // The other half completes it, nothing was lost across the boundary.
        let packets = s.push(&tone(FRAME_LEN / 2)).unwrap();
        assert_eq!(packets.len(), 1);
        assert_eq!(s.pending_samples(), 0);
    }

    #[test]
    fn oversized_buffer_yields_several_packets() {
        let mut s = OpusStream::new(128_000).unwrap();
        let packets = s.push(&tone(FRAME_LEN * 3 + 100)).unwrap();
        assert_eq!(packets.len(), 3);
        assert_eq!(s.pending_samples(), 100, "remainder is carried");
    }

    #[test]
    fn timestamps_advance_by_exactly_one_frame() {
        let mut s = OpusStream::new(128_000).unwrap();
        let packets = s.push(&tone(FRAME_LEN * 4)).unwrap();
        let stamps: Vec<u64> = packets.iter().map(|p| p.timestamp_us).collect();
        assert_eq!(stamps, vec![0, 20_000, 40_000, 60_000]);
    }

    #[test]
    fn timestamps_continue_across_separate_pushes() {
        let mut s = OpusStream::new(128_000).unwrap();
        let a = s.push(&tone(FRAME_LEN * 2)).unwrap();
        let b = s.push(&tone(FRAME_LEN * 2)).unwrap();
        assert_eq!(a.last().unwrap().timestamp_us, 20_000);
        assert_eq!(b.first().unwrap().timestamp_us, 40_000);
    }

    #[test]
    fn silence_still_produces_packets() {
        // The gap-filled silence from loopback.rs must still encode, or the
        // stream stalls exactly when we were trying to prevent it stalling.
        let mut s = OpusStream::new(128_000).unwrap();
        let packets = s.push(&vec![0i16; FRAME_LEN * 2]).unwrap();
        assert_eq!(packets.len(), 2);
        assert!(packets.iter().all(|p| !p.data.is_empty()));
    }

    #[test]
    fn bitrate_lands_in_a_sane_range() {
        let mut s = OpusStream::new(128_000).unwrap();
        // One second of audio.
        s.push(&tone(FRAME_LEN * 50)).unwrap();
        let rate = s.average_bitrate_bps();
        assert!(
            (32_000.0..400_000.0).contains(&rate),
            "unexpected bitrate {rate} bps"
        );
    }
}
