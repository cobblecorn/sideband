//! Making the captured application audible on the other end.
//!
//! Process loopback hands over exactly what the application rendered, and that
//! is quieter than it sounds to the person sitting in front of it. None of the
//! gain between the application and their ears is in the captured stream: not
//! the master volume, not the per-application slider in the mixer, not the
//! headset's own amplifier. Measured here, a game sat at -26 dBFS peak while a
//! chat application on the same machine sat at -15, and both sounded normal
//! locally.
//!
//! Sent as captured, that is a viewer turning their volume to maximum and
//! still straining, which is what "basically no audio" turns out to mean most
//! of the time. So the level is brought up to somewhere broadcast-shaped, and
//! held there as the source changes.
//!
//! This is a levelling amplifier, not a normaliser: it reacts to what has
//! happened recently rather than to the whole recording, because a live stream
//! has no whole recording. The shape is the usual one:
//!
//!   * A peak envelope with an instant attack and a slow release, so one
//!     gunshot sets the ceiling and a quiet minute afterwards does not
//!     immediately claw it back.
//!   * Gain that falls fast and rises slowly. Falling late means clipping;
//!     rising quickly means audibly pumping between every pause in speech.
//!   * A hard ceiling at the end regardless, because arithmetic that can
//!     exceed full scale has to be told what to do when it does.
//!
//! Everything here is pure integer and float arithmetic over a slice, so the
//! whole policy is testable without an audio device, which is the only reason
//! the constants below can be trusted at all.

/// Where the loudest recent sample is aimed, as a fraction of full scale.
///
/// About -3 dBFS. Not 1.0: a peak sitting exactly at full scale leaves the
/// limiter working on every transient, and the difference is inaudible.
const TARGET_PEAK: f32 = 0.71;

/// The most the signal may be lifted, about +24 dB.
///
/// A ceiling exists because the alternative is that a silent application gets
/// amplified until its noise floor is the loudest thing on the stream. Room
/// tone and電 hum are not what anybody is trying to hear.
const MAX_GAIN: f32 = 16.0;

/// Never quieter than what the application produced.
///
/// An application that is already loud does not want turning down, it wants
/// leaving alone and catching if it overshoots, which is what the ceiling at
/// the end is for.
const MIN_GAIN: f32 = 1.0;

/// Below this the envelope is treated as nothing at all, and the gain holds
/// where it is instead of climbing.
///
/// About -66 dBFS. Without it, every pause would wind the gain to maximum and
/// the next sound would arrive as a slam that the limiter then has to flatten.
const NOISE_FLOOR: f32 = 0.0005;

/// Per-sample release of the peak envelope, a time constant of about 1.5
/// seconds at 48 kHz.
const ENVELOPE_RELEASE: f32 = 0.999_986;

/// Per-sample rate at which gain comes *down*, a time constant of a couple of
/// milliseconds. Fast, because this is what stops the signal clipping.
const GAIN_ATTACK: f32 = 0.01;

/// Per-sample rate at which gain goes *up*, a time constant of about a second.
/// Slow, because this is what makes it inaudible.
const GAIN_RELEASE: f32 = 0.000_02;

/// A levelling amplifier with a limiter on the end.
pub struct Gain {
    /// Applied gain, smoothed per sample so there is never a step in the
    /// waveform to hear.
    current: f32,
    /// Peak of the recent past, decaying.
    envelope: f32,
}

impl Default for Gain {
    fn default() -> Self {
        // Starting at unity rather than at the ceiling: the first moment of a
        // stream is the worst possible time to be at maximum gain, because
        // nothing has been heard yet to justify it.
        Self { current: 1.0, envelope: 0.0 }
    }
}

impl Gain {
    /// Lifts one buffer of interleaved 16-bit PCM in place.
    ///
    /// Returns the loudest sample after the lift, as a fraction of full scale,
    /// which is what the level meter should show: the meter is there to answer
    /// "will they hear this", and that is a question about what is being sent,
    /// not about what arrived from the application.
    pub fn apply(&mut self, pcm: &mut [i16]) -> f32 {
        let mut loudest = 0.0f32;

        for sample in pcm.iter_mut() {
            let x = *sample as f32 / 32768.0;
            let magnitude = x.abs();

            // Instant attack, slow release. `max` rather than a filter so a
            // single transient is never averaged away into inaudibility.
            self.envelope = (self.envelope * ENVELOPE_RELEASE).max(magnitude);

            // Only chase a target there is evidence for. In near silence the
            // gain holds, which is what keeps a pause from winding it up.
            if self.envelope > NOISE_FLOOR {
                let wanted = (TARGET_PEAK / self.envelope).clamp(MIN_GAIN, MAX_GAIN);
                let rate = if wanted < self.current { GAIN_ATTACK } else { GAIN_RELEASE };
                self.current += (wanted - self.current) * rate;
            }

            // The ceiling. Everything above is an estimate; this is the part
            // that is not allowed to be wrong.
            let lifted = (x * self.current).clamp(-1.0, 1.0);
            loudest = loudest.max(lifted.abs());
            *sample = (lifted * 32767.0) as i16;
        }

        loudest
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// `seconds` of a sine at `amplitude`, as interleaved stereo.
    fn tone(amplitude: f32, seconds: f32) -> Vec<i16> {
        let frames = (48_000.0 * seconds) as usize;
        let mut out = Vec::with_capacity(frames * 2);
        for i in 0..frames {
            let t = i as f32 / 48_000.0;
            let v = ((t * 440.0 * std::f32::consts::TAU).sin() * amplitude * 32767.0) as i16;
            out.push(v);
            out.push(v);
        }
        out
    }

    fn peak(pcm: &[i16]) -> f32 {
        pcm.iter().map(|s| s.unsigned_abs() as f32).fold(0.0, f32::max) / 32767.0
    }

    /// The last tenth of a buffer, once the gain has had time to settle.
    fn settled(pcm: &[i16]) -> f32 {
        peak(&pcm[pcm.len() * 9 / 10..])
    }

    #[test]
    fn a_quiet_source_is_brought_up() {
        // The measured case: a game at about -26 dBFS, which is a viewer at
        // full volume hearing almost nothing.
        let mut g = Gain::default();
        let mut pcm = tone(0.05, 6.0);
        g.apply(&mut pcm);

        let out = settled(&pcm);
        assert!(out > 0.4, "should be audible, reached {out}");
        assert!(out <= 1.0, "must not exceed full scale, reached {out}");
    }

    #[test]
    fn a_loud_source_is_left_alone() {
        // Already broadcast level. Turning this down would be a regression in
        // the other direction, and clipping it would be worse.
        let mut g = Gain::default();
        let mut pcm = tone(0.8, 3.0);
        g.apply(&mut pcm);

        let out = settled(&pcm);
        assert!(out >= 0.75, "should not have been attenuated, got {out}");
        assert!(out <= 1.0, "must not clip, got {out}");
    }

    #[test]
    fn nothing_ever_exceeds_full_scale() {
        // The limiter is the one part that is not allowed to be approximately
        // right. Worst case: a long quiet passage winding the gain up, then a
        // sudden full scale transient.
        let mut g = Gain::default();
        let mut quiet = tone(0.01, 5.0);
        g.apply(&mut quiet);

        let mut loud = tone(1.0, 1.0);
        let reported = g.apply(&mut loud);

        assert!(peak(&loud) <= 1.0);
        assert!(reported <= 1.0, "reported level should be a fraction, got {reported}");
    }

    #[test]
    fn silence_stays_silent() {
        // The failure this guards: a gain that chases a target through a
        // silent passage arrives at maximum, and then amplifies the room.
        let mut g = Gain::default();
        let mut pcm = vec![0i16; 48_000 * 2];
        let reported = g.apply(&mut pcm);

        assert_eq!(reported, 0.0);
        assert!(pcm.iter().all(|&s| s == 0), "silence in, silence out");
    }

    #[test]
    fn the_gain_does_not_step() {
        // Zipper noise: a gain that jumps between buffers is audible as a
        // click on every boundary. Consecutive output samples of a smooth
        // input have to stay smooth.
        let mut g = Gain::default();
        let mut pcm = tone(0.03, 2.0);
        g.apply(&mut pcm);

        // A 440 Hz sine at 48 kHz moves by at most a few hundred counts
        // between samples at full scale; anything far above that is the gain
        // moving, not the signal.
        let worst = pcm
            .chunks(2)
            .zip(pcm.chunks(2).skip(1))
            .map(|(a, b)| (b[0] as i32 - a[0] as i32).abs())
            .max()
            .unwrap_or(0);
        assert!(worst < 3_000, "output should stay smooth, biggest step was {worst}");
    }

    #[test]
    fn the_reported_level_is_what_is_actually_sent() {
        // The meter reads this, and it has to describe the lifted signal. A
        // meter showing the raw capture would sit near zero on exactly the
        // sources this module exists to rescue, which is the reading that
        // sent everybody looking in the wrong place.
        let mut g = Gain::default();
        let mut pcm = tone(0.05, 6.0);
        let reported = g.apply(&mut pcm);
        assert!((reported - peak(&pcm)).abs() < 0.01, "reported {reported}, actual {}", peak(&pcm));
    }
}
