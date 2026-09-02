//! Shared state between the streaming threads and whatever is showing it.
//!
//! The capture, encode and send paths do not know whether a window or a
//! terminal is watching — they push facts in here and something else decides
//! how to draw them. That is what lets the same engine back both the GUI and
//! the command line without either one owning the other.

use std::sync::atomic::{AtomicBool, AtomicU32, AtomicU64, Ordering};
use std::sync::Mutex;
use std::time::Instant;

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Phase {
    Idle,
    /// Setting up, with a human-readable note about what is happening.
    Preparing(String),
    /// Ready for a viewer. `code` is present only when a relay is in use.
    Waiting { code: Option<String>, link: String },
    /// Someone answered and is waiting to be let in. Holding here is what
    /// makes a correct code insufficient on its own: whoever has it still
    /// cannot see anything until a person says so.
    Approving { viewer: String },
    Live,
    Failed(String),
    Ended,
}

/// How long a notice stays on screen. Long enough to read, short enough that
/// it never becomes part of the furniture.
const NOTICE_LIFETIME: std::time::Duration = std::time::Duration::from_secs(6);

pub struct Session {
    phase: Mutex<Phase>,
    source: Mutex<String>,
    resolution: Mutex<String>,

    /// The process the stream should be showing right now.
    ///
    /// Not fixed at start-up: the window and the capture threads read it every
    /// pass, so picking a different application mid-stream swaps both the
    /// picture and the audio without disturbing the connection. Zero until a
    /// session has one.
    selected: AtomicU32,

    /// Something worth telling the person at the keyboard, and when it was
    /// said. Short-lived by design — a switch that could not be made needs an
    /// explanation at the moment it fails, not a permanent banner.
    notice: Mutex<Option<(String, Instant)>>,

    pub stop: AtomicBool,

    video_frames: AtomicU64,
    audio_packets: AtomicU64,
    video_bytes: AtomicU64,

    /// Set once the host answers the approval prompt. `None` means still
    /// waiting; a timeout is treated as a refusal by the caller, because the
    /// safe reading of an unanswered prompt is that nobody was there to say
    /// yes.
    approval: Mutex<Option<bool>>,

    /// Held mid-session: the connection stays up and the viewer keeps the
    /// last frame on screen, rather than being disconnected and having to
    /// rejoin with a new code.
    paused: AtomicBool,

    mic_on: AtomicBool,
    mic_available: AtomicBool,
    /// Peak since the last read, as a fraction of full scale times 1000.
    mic_peak: AtomicU32,

    /// What the encoder is actually running at, in kbit/s, and the frame rate
    /// its rate control is budgeting for. These are the *applied* figures, not
    /// the controller's wish: if a driver refuses a rate change, the read-out
    /// should show that rather than the number we asked for.
    video_kbps: AtomicU32,
    video_fps: AtomicU32,
}

impl Default for Session {
    fn default() -> Self {
        Self {
            phase: Mutex::new(Phase::Idle),
            source: Mutex::new(String::new()),
            resolution: Mutex::new(String::new()),
            selected: AtomicU32::new(0),
            notice: Mutex::new(None),
            stop: AtomicBool::new(false),
            video_frames: AtomicU64::new(0),
            audio_packets: AtomicU64::new(0),
            video_bytes: AtomicU64::new(0),
            approval: Mutex::new(None),
            paused: AtomicBool::new(false),
            mic_on: AtomicBool::new(false),
            mic_available: AtomicBool::new(false),
            mic_peak: AtomicU32::new(0),
            video_kbps: AtomicU32::new(0),
            video_fps: AtomicU32::new(0),
        }
    }
}

impl Session {
    pub fn phase(&self) -> Phase {
        self.phase.lock().map(|p| p.clone()).unwrap_or(Phase::Idle)
    }

    pub fn set_phase(&self, phase: Phase) {
        if let Ok(mut p) = self.phase.lock() {
            *p = phase;
        }
    }

    pub fn preparing(&self, what: &str) {
        self.set_phase(Phase::Preparing(what.to_owned()));
    }

    pub fn fail(&self, why: impl Into<String>) {
        self.set_phase(Phase::Failed(why.into()));
    }

    pub fn source(&self) -> String {
        self.source.lock().map(|s| s.clone()).unwrap_or_default()
    }

    pub fn set_source(&self, exe: &str, title: &str) {
        if let Ok(mut v) = self.source.lock() {
            *v = if title.is_empty() {
                exe.to_owned()
            } else {
                format!("{exe} — {title}")
            };
        }
    }

    /// Asks for a different application. Takes effect within a frame or two;
    /// the capture threads own the actual switch and may refuse it.
    pub fn select_source(&self, pid: u32) {
        self.selected.store(pid, Ordering::Relaxed);
    }

    /// The application the stream is meant to be showing. Zero before one has
    /// been chosen.
    pub fn selected_source(&self) -> u32 {
        self.selected.load(Ordering::Relaxed)
    }

    pub fn note(&self, what: impl Into<String>) {
        if let Ok(mut n) = self.notice.lock() {
            *n = Some((what.into(), Instant::now()));
        }
    }

    /// The current notice, if one was left recently enough to still matter.
    pub fn notice(&self) -> Option<String> {
        let mut guard = self.notice.lock().ok()?;
        let (text, at) = guard.as_ref()?;
        if at.elapsed() > NOTICE_LIFETIME {
            *guard = None;
            return None;
        }
        Some(text.clone())
    }

    pub fn resolution(&self) -> String {
        self.resolution.lock().map(|s| s.clone()).unwrap_or_default()
    }

    pub fn set_resolution(&self, w: u32, h: u32) {
        if let Ok(mut v) = self.resolution.lock() {
            *v = format!("{w}x{h}");
        }
    }

    pub fn note_video(&self, bytes: usize) {
        self.video_frames.fetch_add(1, Ordering::Relaxed);
        self.video_bytes.fetch_add(bytes as u64, Ordering::Relaxed);
    }

    pub fn note_audio(&self) {
        self.audio_packets.fetch_add(1, Ordering::Relaxed);
    }

    pub fn counters(&self) -> (u64, u64, u64) {
        (
            self.video_frames.load(Ordering::Relaxed),
            self.audio_packets.load(Ordering::Relaxed),
            self.video_bytes.load(Ordering::Relaxed),
        )
    }

    /// Asks the host to let a viewer in, describing who is asking.
    pub fn request_approval(&self, viewer: &str) {
        if let Ok(mut a) = self.approval.lock() {
            *a = None;
        }
        self.set_phase(Phase::Approving { viewer: viewer.to_owned() });
    }

    pub fn approve(&self) {
        if let Ok(mut a) = self.approval.lock() {
            *a = Some(true);
        }
    }

    pub fn deny(&self) {
        if let Ok(mut a) = self.approval.lock() {
            *a = Some(false);
        }
    }

    /// `None` until the host answers.
    pub fn approval_decision(&self) -> Option<bool> {
        self.approval.lock().ok().and_then(|a| *a)
    }

    pub fn paused(&self) -> bool {
        self.paused.load(Ordering::Relaxed)
    }

    /// Returns the new state.
    pub fn toggle_pause(&self) -> bool {
        let now = !self.paused.load(Ordering::Relaxed);
        self.paused.store(now, Ordering::Relaxed);
        now
    }

    pub fn mic_available(&self) -> bool {
        self.mic_available.load(Ordering::Relaxed)
    }

    pub fn set_mic_available(&self, yes: bool) {
        self.mic_available.store(yes, Ordering::Relaxed);
    }

    pub fn mic_on(&self) -> bool {
        self.mic_on.load(Ordering::Relaxed)
    }

    pub fn set_mic_on(&self, on: bool) {
        self.mic_on.store(on, Ordering::Relaxed);
    }

    /// Peak level 0.0–1.0. Reading does not clear it; the capture side owns
    /// the reset so the display can be redrawn as often as it likes without
    /// stealing readings from itself.
    pub fn mic_peak(&self) -> f32 {
        self.mic_peak.load(Ordering::Relaxed) as f32 / 1000.0
    }

    pub fn set_mic_peak(&self, level: f32) {
        self.mic_peak
            .store((level.clamp(0.0, 1.0) * 1000.0) as u32, Ordering::Relaxed);
    }

    /// Records what the encoder settled on for this second.
    pub fn set_quality(&self, bits_per_second: u32, fps: u32) {
        self.video_kbps.store(bits_per_second / 1000, Ordering::Relaxed);
        self.video_fps.store(fps, Ordering::Relaxed);
    }

    /// `None` until a stream is running. Kbit/s and frames per second.
    pub fn quality(&self) -> Option<(u32, u32)> {
        let kbps = self.video_kbps.load(Ordering::Relaxed);
        let fps = self.video_fps.load(Ordering::Relaxed);
        (kbps > 0).then_some((kbps, fps))
    }

    pub fn should_stop(&self) -> bool {
        self.stop.load(Ordering::Relaxed)
    }

    pub fn request_stop(&self) {
        self.stop.store(true, Ordering::Relaxed);
    }
}

/// Rolling rate calculator, so the display can show frames per second and
/// megabits per second from counters that only ever increase.
pub struct Rates {
    last: std::time::Instant,
    frames: u64,
    packets: u64,
    bytes: u64,
    pub fps: f32,
    pub audio_pps: f32,
    pub mbps: f32,
}

impl Default for Rates {
    fn default() -> Self {
        Self {
            last: std::time::Instant::now(),
            frames: 0,
            packets: 0,
            bytes: 0,
            fps: 0.0,
            audio_pps: 0.0,
            mbps: 0.0,
        }
    }
}

impl Rates {
    /// Recomputes at most once a second; calling it more often is harmless and
    /// simply returns the previous figures.
    pub fn update(&mut self, session: &Session) {
        let elapsed = self.last.elapsed().as_secs_f32();
        if elapsed < 1.0 {
            return;
        }
        let (frames, packets, bytes) = session.counters();
        self.fps = (frames.saturating_sub(self.frames)) as f32 / elapsed;
        self.audio_pps = (packets.saturating_sub(self.packets)) as f32 / elapsed;
        self.mbps = (bytes.saturating_sub(self.bytes)) as f32 * 8.0 / elapsed / 1_000_000.0;
        self.frames = frames;
        self.packets = packets;
        self.bytes = bytes;
        self.last = std::time::Instant::now();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn counters_accumulate() {
        let s = Session::default();
        s.note_video(1000);
        s.note_video(500);
        s.note_audio();
        assert_eq!(s.counters(), (2, 1, 1500));
    }

    #[test]
    fn mic_peak_round_trips_and_clamps() {
        let s = Session::default();
        s.set_mic_peak(0.5);
        assert!((s.mic_peak() - 0.5).abs() < 0.002);

        s.set_mic_peak(5.0);
        assert_eq!(s.mic_peak(), 1.0, "over-range input is clamped, not wrapped");
    }

    #[test]
    fn phase_transitions_are_visible() {
        let s = Session::default();
        assert_eq!(s.phase(), Phase::Idle);
        s.preparing("gathering");
        assert_eq!(s.phase(), Phase::Preparing("gathering".into()));
        s.fail("nope");
        assert_eq!(s.phase(), Phase::Failed("nope".into()));
    }

    #[test]
    fn approval_starts_undecided_and_records_either_answer() {
        let s = Session::default();
        s.request_approval("198.51.100.7");
        assert_eq!(s.phase(), Phase::Approving { viewer: "198.51.100.7".into() });
        assert_eq!(s.approval_decision(), None, "nothing is assumed before an answer");

        s.approve();
        assert_eq!(s.approval_decision(), Some(true));

        // A fresh prompt must not inherit the previous answer, or a second
        // viewer would be let in on the strength of the first one's approval.
        s.request_approval("203.0.113.9");
        assert_eq!(s.approval_decision(), None);
        s.deny();
        assert_eq!(s.approval_decision(), Some(false));
    }

    #[test]
    fn quality_is_absent_until_a_stream_is_running() {
        let s = Session::default();
        assert_eq!(s.quality(), None, "nothing to report before anything is encoded");

        s.set_quality(2_500_000, 60);
        assert_eq!(s.quality(), Some((2500, 60)));

        s.set_quality(900_000, 30);
        assert_eq!(s.quality(), Some((900, 30)));
    }

    #[test]
    fn a_source_can_be_swapped_while_a_stream_runs() {
        let s = Session::default();
        assert_eq!(s.selected_source(), 0, "nothing chosen yet");

        s.select_source(1234);
        assert_eq!(s.selected_source(), 1234);

        // The whole point: choosing again is not an error, and does not need
        // the stream to be stopped first.
        s.select_source(5678);
        assert_eq!(s.selected_source(), 5678);
    }

    #[test]
    fn notices_are_readable_and_do_not_linger_forever() {
        let s = Session::default();
        assert_eq!(s.notice(), None);

        s.note("could not capture that window");
        assert_eq!(s.notice().as_deref(), Some("could not capture that window"));
        // Reading does not consume it — the window redraws many times a second
        // and a notice that vanished on first paint would never be seen.
        assert!(s.notice().is_some());
    }

    #[test]
    fn pause_toggles_and_reports_the_new_state() {
        let s = Session::default();
        assert!(!s.paused());
        assert!(s.toggle_pause(), "toggling from off returns on");
        assert!(s.paused());
        assert!(!s.toggle_pause());
        assert!(!s.paused());
    }

    #[test]
    fn rates_hold_steady_until_a_second_has_passed() {
        let s = Session::default();
        let mut r = Rates::default();
        s.note_video(100);
        r.update(&s);
        // Too soon to have measured anything; must not divide by a tiny
        // elapsed time and report an absurd rate.
        assert_eq!(r.fps, 0.0);
    }
}
