//! Stage 4, part one, the pacing spine the encoder plugs into.
//!
//! Two findings from stages 2 and 3 turn out to be the same bug wearing
//! different hats:
//!
//!   * process loopback delivers no packets while the app is silent, and
//!   * Windows.Graphics.Capture delivers no frames while the window is still.
//!
//! In both cases "no data" means *keep going with what you had*, never *stall*.
//! An encoder fed at an irregular cadence produces a stream whose timestamps
//! drift against the other track, and A/V sync walks off within a minute.
//!
//! So the source is not allowed to drive the clock. `Pacer` does: it emits a
//! frame on every deadline whether or not the window redrew, repeating the
//! previous frame when nothing new arrived. Audio does the equivalent by
//! synthesising silence (see `loopback::Capture::pump`).
//!
//! `Pacer` is generic over the frame type purely so it can be tested without a
//! GPU, in the real pipeline `T` is `ID3D11Texture2D`, which is a refcounted
//! COM pointer, so cloning it to repeat a frame is a refcount bump and not a
//! copy of any pixels.

use std::time::Instant;

/// The single epoch both the audio and video tracks timestamp against.
/// Sharing one clock is what lets the receiver line the two up.
#[derive(Clone)]
pub struct MediaClock {
    epoch: Instant,
}

impl MediaClock {
    pub fn start() -> Self {
        Self { epoch: Instant::now() }
    }

    pub fn now_us(&self) -> u64 {
        self.epoch.elapsed().as_micros() as u64
    }
}

impl Default for MediaClock {
    fn default() -> Self {
        Self::start()
    }
}

/// A frame handed downstream on a deadline.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Paced<T> {
    pub frame: T,
    /// Microseconds from the shared epoch. Evenly spaced by construction,
    /// this is the slot time, not the wall-clock time the frame was emitted,
    /// so jitter in the caller's polling never reaches the encoder.
    pub timestamp_us: u64,
    /// The source produced nothing new; this repeats the previous frame.
    pub repeated: bool,
}

pub struct Pacer<T> {
    interval_us: u64,
    next_deadline_us: u64,
    last: Option<T>,
    /// Whether a new frame has arrived since the last emission. This is the
    /// question the repeat statistic is actually asking, *not* whether a
    /// frame happened to arrive on the same call as the deadline, which for
    /// any realistic polling interval is almost never true.
    fresh_since_emit: bool,
    delivered: u64,
    repeated: u64,
    dropped_slots: u64,
}

impl<T: Clone> Pacer<T> {
    pub fn new(target_fps: u32) -> Self {
        assert!(target_fps > 0, "target_fps must be non-zero");
        let interval_us = 1_000_000 / target_fps as u64;
        Self {
            interval_us,
            next_deadline_us: 0,
            last: None,
            fresh_since_emit: false,
            delivered: 0,
            repeated: 0,
            dropped_slots: 0,
        }
    }

    /// Feed in whatever the source produced since the last call (`None` if it
    /// produced nothing) and get back a frame if one is due.
    ///
    /// Safe to call as often as you like, it emits at most one frame per
    /// call, on the deadline. Time is a parameter rather than read from a
    /// clock so this stays deterministic under test.
    pub fn tick_at(&mut self, now_us: u64, fresh: Option<T>) -> Option<Paced<T>> {
        // Newest wins: if the source produced several frames between
        // deadlines, the intermediate ones are never sent. That is correct,
        // we are streaming current state, not recording every frame.
        if let Some(f) = fresh {
            self.last = Some(f);
            self.fresh_since_emit = true;
        }

        if now_us < self.next_deadline_us {
            return None;
        }

        // Nothing has ever arrived, there is no previous frame to repeat, so
        // hold the deadline rather than emitting garbage.
        let Some(frame) = self.last.clone() else {
            self.next_deadline_us = now_us + self.interval_us;
            return None;
        };

        let slot = self.next_deadline_us;

        // If we fell more than one interval behind (a stall, a scheduler
        // hiccup), skip the missed slots outright instead of emitting a burst
        // to catch up. A burst would only push the receiver's jitter buffer
        // around for no benefit.
        let behind = now_us.saturating_sub(self.next_deadline_us);
        if behind >= self.interval_us {
            let skipped = behind / self.interval_us;
            self.dropped_slots += skipped;
            self.next_deadline_us += self.interval_us * (skipped + 1);
        } else {
            self.next_deadline_us += self.interval_us;
        }

        let is_repeat = !self.fresh_since_emit;
        self.fresh_since_emit = false;

        self.delivered += 1;
        if is_repeat {
            self.repeated += 1;
        }

        Some(Paced { frame, timestamp_us: slot, repeated: is_repeat })
    }

    /// Changes the cadence mid-stream, for when the connection cannot carry
    /// the frame rate it started at.
    ///
    /// The deadline already set is left alone: it was calculated under the old
    /// interval and is at most one frame away, so honouring it and applying
    /// the new spacing from there avoids a gap or a burst at the changeover.
    pub fn set_fps(&mut self, target_fps: u32) {
        assert!(target_fps > 0, "target_fps must be non-zero");
        self.interval_us = 1_000_000 / target_fps as u64;
    }

    /// Drops the cached frame. Call when the source geometry changes: the
    /// held frame is the old size, and replaying it into a re-sized encoder
    /// would be a dimension mismatch.
    pub fn reset(&mut self) {
        self.last = None;
        self.fresh_since_emit = false;
    }

    pub fn stats(&self) -> PacerStats {
        PacerStats {
            delivered: self.delivered,
            repeated: self.repeated,
            dropped_slots: self.dropped_slots,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PacerStats {
    pub delivered: u64,
    pub repeated: u64,
    pub dropped_slots: u64,
}

impl PacerStats {
    /// Share of emitted frames that were repeats. High is not automatically
    /// bad, a paused game legitimately repeats, but a high figure during
    /// active play means frames are not arriving and something is wrong.
    pub fn repeat_ratio(&self) -> f64 {
        if self.delivered == 0 {
            0.0
        } else {
            self.repeated as f64 / self.delivered as f64
        }
    }
}

// ---------------------------------------------------------------------------
// The seam stage 4 plugs into
// ---------------------------------------------------------------------------

/// Implemented by the hardware encoder. The frame arrives as a GPU texture and
/// must stay there: reading it back to the CPU costs more latency than the
/// entire network hop.
#[allow(dead_code)] // implemented in stage 4 part two
pub trait VideoEncoder {
    type Frame;
    fn submit(&mut self, frame: &Paced<Self::Frame>) -> Result<(), String>;
    /// Called when the receiver reports picture loss (RTCP PLI) and on
    /// connect. Ignoring this leaves the viewer on a frozen screen that never
    /// recovers.
    fn request_keyframe(&mut self);
}

/// Implemented by the Opus encoder. PCM arrives already gap-filled, so the
/// implementation derives timestamps from its own sample count rather than
/// being told the time, counting a continuous stream cannot drift the way
/// repeatedly reading a clock can.
#[allow(dead_code)] // consumed generically once stage 5 owns the send loop
pub trait AudioEncoder {
    fn submit(&mut self, pcm: &[i16]) -> Result<(), String>;
}

#[cfg(test)]
mod tests {
    use super::*;

    const FPS: u32 = 60;
    const IVL: u64 = 1_000_000 / 60; // 16666 us

    #[test]
    fn emits_nothing_before_the_first_frame_arrives() {
        let mut p: Pacer<u32> = Pacer::new(FPS);
        assert_eq!(p.tick_at(0, None), None);
        assert_eq!(p.tick_at(IVL * 5, None), None);
        assert_eq!(p.stats().delivered, 0);
    }

    #[test]
    fn emits_on_the_deadline_not_on_arrival() {
        let mut p: Pacer<u32> = Pacer::new(FPS);
        // Frame arrives immediately and is emitted at slot 0.
        let out = p.tick_at(0, Some(7)).expect("frame due at slot 0");
        assert_eq!(out.frame, 7);
        assert_eq!(out.timestamp_us, 0);
        assert!(!out.repeated);

        // Nothing due until the next interval.
        assert_eq!(p.tick_at(IVL - 1, None), None);
    }

    #[test]
    fn repeats_the_last_frame_when_the_window_is_still() {
        let mut p: Pacer<u32> = Pacer::new(FPS);
        p.tick_at(0, Some(1)).unwrap();

        // Source produces nothing for three intervals; the stream must not
        // stall. This is the redraw-driven / silent-app case.
        for slot in 1..=3u64 {
            let out = p.tick_at(IVL * slot, None).expect("frame still due");
            assert_eq!(out.frame, 1, "should repeat the previous frame");
            assert!(out.repeated);
            assert_eq!(out.timestamp_us, IVL * slot);
        }

        let s = p.stats();
        assert_eq!(s.delivered, 4);
        assert_eq!(s.repeated, 3);
        assert_eq!(s.repeat_ratio(), 0.75);
    }

    #[test]
    fn timestamps_stay_evenly_spaced_despite_jittery_polling() {
        let mut p: Pacer<u32> = Pacer::new(FPS);
        p.tick_at(0, Some(0)).unwrap();

        // Caller polls late by a few hundred microseconds each time. The
        // emitted timestamps must not inherit that jitter.
        let mut seen = vec![];
        for slot in 1..=4u64 {
            let sloppy = IVL * slot + 900;
            if let Some(out) = p.tick_at(sloppy, Some(slot as u32)) {
                seen.push(out.timestamp_us);
            }
        }
        assert_eq!(seen, vec![IVL, IVL * 2, IVL * 3, IVL * 4]);
    }

    #[test]
    fn frame_arriving_between_deadlines_is_not_a_repeat() {
        // The case the first real encode run exposed: a 1 ms poll loop against
        // a 16 ms deadline means a fresh frame almost never lands on the same
        // call as the deadline. Counting only same-call arrivals reported
        // "100% repeats" while frames were in fact flowing normally.
        let mut p: Pacer<u32> = Pacer::new(FPS);
        p.tick_at(0, Some(1)).unwrap();

        assert_eq!(p.tick_at(1_000, Some(2)), None, "arrives, not yet due");
        let out = p.tick_at(IVL, None).expect("due now");
        assert_eq!(out.frame, 2);
        assert!(!out.repeated, "frame 2 is new since the last emission");

        // With nothing new since, the next one genuinely is a repeat.
        let out = p.tick_at(IVL * 2, None).expect("due now");
        assert!(out.repeated);
        assert_eq!(p.stats().repeated, 1);
    }

    #[test]
    fn newest_frame_wins_between_deadlines() {
        let mut p: Pacer<u32> = Pacer::new(FPS);
        p.tick_at(0, Some(1)).unwrap();

        // Three frames arrive within one interval; only the last is sent.
        assert_eq!(p.tick_at(100, Some(2)), None);
        assert_eq!(p.tick_at(200, Some(3)), None);
        let out = p.tick_at(IVL, Some(4)).unwrap();
        assert_eq!(out.frame, 4);
        assert!(!out.repeated);
    }

    #[test]
    fn skips_missed_slots_instead_of_bursting() {
        let mut p: Pacer<u32> = Pacer::new(FPS);
        p.tick_at(0, Some(1)).unwrap();

        // A 10-interval stall. We should emit one frame, not ten.
        let out = p.tick_at(IVL * 10, None).expect("one frame");
        assert_eq!(out.timestamp_us, IVL);
        assert_eq!(p.stats().delivered, 2);
        assert_eq!(p.stats().dropped_slots, 9);

        // And the next deadline is ahead of now, not still in the past.
        assert_eq!(p.tick_at(IVL * 10, None), None);
    }

    #[test]
    fn changing_the_frame_rate_respaces_without_a_gap_or_a_burst() {
        let mut p: Pacer<u32> = Pacer::new(FPS);
        p.tick_at(0, Some(1)).unwrap();

        // Halving the rate doubles the spacing from the next deadline on.
        p.set_fps(30);
        let slow = 1_000_000 / 30;

        // The deadline already set stands, it is at most one frame away, and
        // moving it would either drop a frame or emit two in a row.
        assert_eq!(p.tick_at(IVL, Some(2)).unwrap().timestamp_us, IVL);
        assert_eq!(p.tick_at(IVL + 1, Some(3)), None, "the next slot is further out now");
        assert_eq!(p.tick_at(IVL + slow, Some(4)).unwrap().timestamp_us, IVL + slow);
    }

    #[test]
    fn clock_is_monotonic_from_its_epoch() {
        let c = MediaClock::start();
        let a = c.now_us();
        let b = c.now_us();
        assert!(b >= a);
    }
}
