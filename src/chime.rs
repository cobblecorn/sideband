//! Short sounds for things that happen while the window is out of sight.
//!
//! The window spends a stream behind whatever is being shared, usually a
//! fullscreen game, so nothing drawn in it is seen when somebody arrives, or
//! when a hotkey turns the microphone on. A sound is the only notice that
//! reaches the person at the keyboard.
//!
//! Synthesised rather than borrowed from the system sound scheme: plenty of
//! machines run with system sounds off, and the stock ones mean "a USB device
//! was plugged in", which is not what anybody should think is happening.
//!
//! None of this reaches the viewers. Only the shared application's own audio
//! is captured, and these are played by this process.

use std::sync::OnceLock;

use windows::core::PCWSTR;
use windows::Win32::Media::Audio::{PlaySoundW, SND_ASYNC, SND_MEMORY, SND_NODEFAULT};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Chime {
    /// Somebody started watching. Rising.
    Joined,
    /// Somebody stopped. Falling, the same two notes.
    Left,
    MicOn,
    MicOff,
    /// The picture was covered.
    Hidden,
    /// And uncovered.
    Shown,
}

const RATE: u32 = 44_100;

/// Quiet. These play over a game, and the point is to be noticed, not to be
/// the loudest thing in the room.
const LEVEL: f32 = 0.22;

/// Plays it and returns at once. A newer sound cuts off an older one, which
/// is what should happen when two things happen together.
pub fn play(which: Chime) {
    let bytes = sound(which);
    // SND_MEMORY reads the buffer for as long as the sound plays, so it has
    // to outlive the call: every one is built once and kept for the life of
    // the process.
    unsafe {
        let _ = PlaySoundW(
            PCWSTR(bytes.as_ptr() as *const u16),
            None,
            SND_MEMORY | SND_ASYNC | SND_NODEFAULT,
        );
    }
}

fn sound(which: Chime) -> &'static [u8] {
    static JOINED: OnceLock<Vec<u8>> = OnceLock::new();
    static LEFT: OnceLock<Vec<u8>> = OnceLock::new();
    static MIC_ON: OnceLock<Vec<u8>> = OnceLock::new();
    static MIC_OFF: OnceLock<Vec<u8>> = OnceLock::new();
    static HIDDEN: OnceLock<Vec<u8>> = OnceLock::new();
    static SHOWN: OnceLock<Vec<u8>> = OnceLock::new();

    const E5: f32 = 659.25;
    const B5: f32 = 987.77;
    const C5: f32 = 523.25;
    const G5: f32 = 783.99;
    const C6: f32 = 1046.50;

    match which {
        Chime::Joined => JOINED.get_or_init(|| wav(&notes(&[(E5, 110), (B5, 190)]))),
        Chime::Left => LEFT.get_or_init(|| wav(&notes(&[(B5, 110), (E5, 190)]))),
        Chime::MicOn => MIC_ON.get_or_init(|| wav(&notes(&[(C6, 80)]))),
        Chime::MicOff => MIC_OFF.get_or_init(|| wav(&notes(&[(C5, 80)]))),
        Chime::Hidden => HIDDEN.get_or_init(|| wav(&notes(&[(G5, 60), (C5, 100)]))),
        Chime::Shown => SHOWN.get_or_init(|| wav(&notes(&[(C5, 60), (G5, 100)]))),
    }
}

/// A run of soft bell-ish notes, as mono samples.
///
/// A little of the octave above for colour, a fast start so it is heard at
/// once, and a decay rather than a flat tone, which is the difference between
/// a chime and a test signal. Each note fades to nothing before the next, so
/// there is never a click at a join.
fn notes(seq: &[(f32, u32)]) -> Vec<i16> {
    let mut out = Vec::new();
    for &(freq, ms) in seq {
        let len = (RATE * ms / 1000) as usize;
        let attack = (RATE as f32 * 0.004) as usize;
        let fade = (RATE as f32 * 0.010) as usize;
        let decay = len as f32 * 0.45;

        for i in 0..len {
            let t = i as f32 / RATE as f32;
            let phase = std::f32::consts::TAU * freq * t;
            let tone = phase.sin() + 0.25 * (2.0 * phase).sin();

            let mut env = (-(i as f32) / decay).exp();
            if i < attack {
                env *= i as f32 / attack as f32;
            }
            if i + fade > len {
                env *= (len - i) as f32 / fade as f32;
            }
            out.push((tone / 1.25 * env * LEVEL * i16::MAX as f32) as i16);
        }
        // A breath between notes.
        out.extend(std::iter::repeat_n(0, (RATE / 100) as usize));
    }
    out
}

/// A complete 16-bit mono WAV file in memory, which is what `SND_MEMORY` takes.
fn wav(samples: &[i16]) -> Vec<u8> {
    let data = samples.len() as u32 * 2;
    let mut out = Vec::with_capacity(44 + data as usize);
    out.extend_from_slice(b"RIFF");
    out.extend_from_slice(&(36 + data).to_le_bytes());
    out.extend_from_slice(b"WAVE");
    out.extend_from_slice(b"fmt ");
    out.extend_from_slice(&16u32.to_le_bytes());
    out.extend_from_slice(&1u16.to_le_bytes()); // PCM
    out.extend_from_slice(&1u16.to_le_bytes()); // mono
    out.extend_from_slice(&RATE.to_le_bytes());
    out.extend_from_slice(&(RATE * 2).to_le_bytes());
    out.extend_from_slice(&2u16.to_le_bytes());
    out.extend_from_slice(&16u16.to_le_bytes());
    out.extend_from_slice(b"data");
    out.extend_from_slice(&data.to_le_bytes());
    for s in samples {
        out.extend_from_slice(&s.to_le_bytes());
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_sound_is_a_well_formed_wav() {
        for which in [
            Chime::Joined,
            Chime::Left,
            Chime::MicOn,
            Chime::MicOff,
            Chime::Hidden,
            Chime::Shown,
        ] {
            let bytes = sound(which);
            assert_eq!(&bytes[..4], b"RIFF");
            let riff = u32::from_le_bytes(bytes[4..8].try_into().unwrap()) as usize;
            assert_eq!(riff + 8, bytes.len(), "{which:?}: the size field matches");
            let data = u32::from_le_bytes(bytes[40..44].try_into().unwrap()) as usize;
            assert_eq!(data + 44, bytes.len(), "{which:?}: the data size matches");
        }
    }

    #[test]
    fn notes_stay_well_inside_full_scale_and_end_in_silence() {
        let s = notes(&[(987.77, 190)]);
        let peak = s.iter().map(|x| x.unsigned_abs()).max().unwrap() as f32 / i16::MAX as f32;
        assert!(peak <= LEVEL + 0.01, "peak {peak}");
        assert!(peak > LEVEL * 0.5, "and loud enough to hear, peak {peak}");
        assert_eq!(*s.last().unwrap(), 0, "no click at the end");
    }

    #[test]
    fn arriving_and_leaving_sound_different() {
        assert_ne!(sound(Chime::Joined), sound(Chime::Left));
    }
}
