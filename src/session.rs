//! Shared state between the streaming threads and whatever is showing it.
//!
//! The capture, encode and send paths do not know whether a window or a
//! terminal is watching, they push facts in here and something else decides
//! how to draw them. That is what lets the same engine back both the GUI and
//! the command line without either one owning the other.

use std::collections::HashSet;
use std::sync::atomic::{AtomicBool, AtomicU32, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
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

/// Somebody watching, as the window lists them.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ViewerInfo {
    /// Ours, for this session only. What removing them refers to.
    pub id: u64,
    /// How their page described itself, "Android, Chrome" and the like. Empty
    /// when it said nothing.
    pub device: String,
    /// Their address as the world sees it, when they shared one.
    pub address: String,
    /// The random name their browser keeps for itself. Not shown: it exists
    /// so that somebody removed is recognised if they come straight back.
    pub browser: String,
    pub since: Instant,
}

impl ViewerInfo {
    /// One line for the window and the read-out.
    pub fn describe(&self) -> String {
        match (self.device.is_empty(), self.address.is_empty()) {
            (false, false) => format!("{} ({})", self.device, self.address),
            (false, true) => self.device.clone(),
            (true, false) => self.address.clone(),
            (true, true) => "someone".to_owned(),
        }
    }
}

pub struct Session {
    phase: Mutex<Phase>,

    /// The code and link most recently offered, kept after the phase moves on.
    ///
    /// They used to exist only inside `Phase::Waiting`, so the moment the
    /// first viewer connected they were gone from the window. That was
    /// harmless while a code admitted one person; it stopped being harmless
    /// when a code admits everybody, because inviting a second viewer then
    /// meant stopping and starting again to see the code.
    share: Mutex<Option<(Option<String>, String)>>,

    /// Who is watching right now, in the order they arrived.
    viewers: Mutex<Vec<ViewerInfo>>,

    /// Viewers the host has asked to remove, by `ViewerInfo::id`. Each one's
    /// send loop looks for itself here and ends.
    kicked: Mutex<HashSet<u64>>,

    /// Browsers turned away for the rest of this session, by the name their
    /// page keeps. Without this, removing somebody from a link that admits
    /// everybody would only last until their page reconnected, a few seconds.
    banned: Mutex<HashSet<String>>,

    /// Nobody new gets in while this is set. Everyone already watching stays.
    locked: AtomicBool,

    /// Asked for a fresh code and link, the old ones stopped. Set by the
    /// window and taken by the relay loop, which is the only thing that can
    /// act on it.
    new_link: AtomicBool,

    /// Play a sound when somebody arrives or leaves, and when a hotkey flips
    /// something. The window is usually behind a fullscreen game when either
    /// happens, so a sound is the only notice that reaches anyone.
    sounds: AtomicBool,
    source: Mutex<(String, String)>,
    resolution: Mutex<String>,

    /// The process the stream should be showing right now.
    ///
    /// Not fixed at start-up: the window and the capture threads read it every
    /// pass, so picking a different application mid-stream swaps both the
    /// picture and the audio without disturbing the connection. Zero until a
    /// session has one.
    selected: AtomicU32,

    /// Something worth telling the person at the keyboard, and when it was
    /// said. Short-lived by design, a switch that could not be made needs an
    /// explanation at the moment it fails, not a permanent banner.
    notice: Mutex<Option<(String, Instant)>>,

    /// Whether the current notice is news rather than a problem, somebody
    /// arriving rather than something failing, so it can be drawn as such.
    notice_is_news: AtomicBool,

    /// Whether viewers who cannot reach this machine on their own can still
    /// get in, and why: see `portmap`. `None` until a session has looked.
    reach: Mutex<Option<(bool, String)>>,

    pub stop: AtomicBool,

    video_frames: AtomicU64,
    audio_packets: AtomicU64,
    video_bytes: AtomicU64,

    /// Set once the host answers the approval prompt. `None` means still
    /// waiting; a timeout is treated as a refusal by the caller, because the
    /// safe reading of an unanswered prompt is that nobody was there to say
    /// yes.
    approval: Mutex<Option<bool>>,

    /// Whether holding the code is enough on its own.
    ///
    /// The prompt exists because a code can be forwarded, shoulder-read or
    /// guessed at, and the host is the only one who knows whether the person
    /// asking is the person they sent it to. Turning it off trades that check
    /// for not having to be at the keyboard when someone arrives, which is a
    /// reasonable trade to want and not one to make on somebody's behalf, so
    /// it is off until asked for.
    auto_approve: AtomicBool,

    /// The picture is covered by a pause card, see `stream::HideCard`.
    ///
    /// Covered rather than frozen. Freezing held the last frame on the
    /// viewer's screen, which is exactly the frame with the login box or the
    /// private message in it that prompted somebody to reach for this. And
    /// the stream keeps flowing underneath, so there is nothing to recover
    /// from when it comes back: no dropped references, no keyframe needed.
    hidden: AtomicBool,

    /// Shared with every microphone opened for this session, one per viewer,
    /// so that the button, the hotkey and all of them agree. It used to live
    /// inside the microphone, where the button could not reach it at all.
    mic_on: Arc<AtomicBool>,
    mic_available: AtomicBool,
    /// Peak since the last read, as a fraction of full scale times 1000.
    mic_peak: AtomicU32,
    /// The capture device actually in use, for the window to show. Knowing
    /// which one is live is most of the answer when a microphone reads zero.
    mic_name: Mutex<String>,

    /// The same measure for the shared application's own audio.
    ///
    /// Worth showing for the reason the mic meter is worth showing, only more
    /// so: process loopback delivers perfectly paced packets whether or not
    /// the application is making any sound, so a stream carrying nothing but
    /// silence looks identical from here to one carrying a game. Without a
    /// level there is no way to tell "this application is quiet" from "we are
    /// listening to the wrong thing", and both arrive as the viewer saying
    /// they cannot hear anything.
    app_peak: AtomicU32,
    /// Whether the loopback capture is actually open. False while a target
    /// refuses to open, which is the one case where silence is our fault.
    app_audio_ok: AtomicBool,

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
            share: Mutex::new(None),
            viewers: Mutex::new(Vec::new()),
            kicked: Mutex::new(HashSet::new()),
            banned: Mutex::new(HashSet::new()),
            locked: AtomicBool::new(false),
            new_link: AtomicBool::new(false),
            sounds: AtomicBool::new(true),
            source: Mutex::new((String::new(), String::new())),
            resolution: Mutex::new(String::new()),
            selected: AtomicU32::new(0),
            notice: Mutex::new(None),
            notice_is_news: AtomicBool::new(false),
            reach: Mutex::new(None),
            stop: AtomicBool::new(false),
            video_frames: AtomicU64::new(0),
            audio_packets: AtomicU64::new(0),
            video_bytes: AtomicU64::new(0),
            approval: Mutex::new(None),
            auto_approve: AtomicBool::new(false),
            hidden: AtomicBool::new(false),
            mic_on: Arc::new(AtomicBool::new(false)),
            mic_available: AtomicBool::new(false),
            mic_peak: AtomicU32::new(0),
            mic_name: Mutex::new(String::new()),
            app_peak: AtomicU32::new(0),
            app_audio_ok: AtomicBool::new(true),
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
        // Remembered on the way through, so it outlives the phase it came in.
        if let Phase::Waiting { code, link } = &phase
            && let Ok(mut s) = self.share.lock()
        {
            *s = Some((code.clone(), link.clone()));
        }
        if let Ok(mut p) = self.phase.lock() {
            *p = phase;
        }
    }

    /// The code (when there is a relay) and link last offered, if any.
    pub fn share(&self) -> Option<(Option<String>, String)> {
        self.share.lock().ok().and_then(|s| s.clone())
    }

    /// Replaces the code and link on offer without changing the phase, for
    /// when they change while somebody is watching.
    pub fn set_share(&self, code: Option<String>, link: String) {
        if let Ok(mut s) = self.share.lock() {
            *s = Some((code, link));
        }
    }

    pub fn watching(&self) -> u32 {
        self.viewers.lock().map_or(0, |v| v.len() as u32)
    }

    pub fn viewers(&self) -> Vec<ViewerInfo> {
        self.viewers.lock().map(|v| v.clone()).unwrap_or_default()
    }

    pub fn add_viewer(&self, viewer: ViewerInfo) {
        if let Ok(mut v) = self.viewers.lock() {
            v.push(viewer);
        }
    }

    /// Takes them off the list, and returns who they were.
    pub fn remove_viewer(&self, id: u64) -> Option<ViewerInfo> {
        if let Ok(mut k) = self.kicked.lock() {
            k.remove(&id);
        }
        let mut v = self.viewers.lock().ok()?;
        let at = v.iter().position(|x| x.id == id)?;
        Some(v.remove(at))
    }

    /// Removes somebody, and keeps them out for the rest of the session.
    ///
    /// Kept out by the name their browser keeps, and by their address when it
    /// kept none. Not by address otherwise: everybody in one house shares one,
    /// and removing a stranger should not also remove the person on the sofa.
    pub fn kick(&self, id: u64) {
        let Some(who) = self.viewers().into_iter().find(|v| v.id == id) else { return };
        if let Ok(mut k) = self.kicked.lock() {
            k.insert(id);
        }
        self.ban(&who.browser, &who.address);
    }

    /// Keeps somebody out for the rest of the session without their having
    /// got in, for a viewer the host turned away at the prompt.
    pub fn ban(&self, browser: &str, address: &str) {
        let key = if !browser.is_empty() {
            browser
        } else if !address.is_empty() {
            address
        } else {
            return;
        };
        if let Ok(mut b) = self.banned.lock() {
            b.insert(key.to_owned());
        }
    }

    pub fn is_kicked(&self, id: u64) -> bool {
        self.kicked.lock().is_ok_and(|k| k.contains(&id))
    }

    /// Whether somebody asking to watch was removed earlier this session.
    pub fn is_banned(&self, browser: &str, address: &str) -> bool {
        let Ok(b) = self.banned.lock() else { return false };
        (!browser.is_empty() && b.contains(browser))
            || (browser.is_empty() && !address.is_empty() && b.contains(address))
    }

    pub fn locked(&self) -> bool {
        self.locked.load(Ordering::Relaxed)
    }

    pub fn set_locked(&self, on: bool) {
        self.locked.store(on, Ordering::Relaxed);
    }

    pub fn request_new_link(&self) {
        self.new_link.store(true, Ordering::Relaxed);
    }

    /// Looked at without being taken, by the parts that only need to get out
    /// of the way of it.
    pub fn new_link_pending(&self) -> bool {
        self.new_link.load(Ordering::Relaxed)
    }

    /// Read and cleared together, so one press is acted on once.
    pub fn take_new_link(&self) -> bool {
        self.new_link.swap(false, Ordering::Relaxed)
    }

    pub fn sounds(&self) -> bool {
        self.sounds.load(Ordering::Relaxed)
    }

    pub fn set_sounds(&self, on: bool) {
        self.sounds.store(on, Ordering::Relaxed);
    }

    pub fn preparing(&self, what: &str) {
        self.set_phase(Phase::Preparing(what.to_owned()));
    }

    pub fn fail(&self, why: impl Into<String>) {
        self.set_phase(Phase::Failed(why.into()));
    }

    /// The application being shared, as its executable and its window title.
    ///
    /// Kept as two strings rather than one joined one. Window titles routinely
    /// contain the separator any joined form would need to be split on again
    /// ("Meme Harvester - Google Chrome"), so joining them means choosing a
    /// character that titles never use, and then hoping.
    pub fn source(&self) -> (String, String) {
        self.source.lock().map(|s| s.clone()).unwrap_or_default()
    }

    pub fn set_source(&self, exe: &str, title: &str) {
        if let Ok(mut v) = self.source.lock() {
            *v = (exe.to_owned(), title.to_owned());
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

    /// Something that went wrong, or that needs doing.
    pub fn note(&self, what: impl Into<String>) {
        if let Ok(mut n) = self.notice.lock() {
            *n = Some((what.into(), Instant::now()));
            self.notice_is_news.store(false, Ordering::Relaxed);
        }
    }

    /// Something that happened and is fine, somebody joining or leaving.
    pub fn tell(&self, what: impl Into<String>) {
        if let Ok(mut n) = self.notice.lock() {
            *n = Some((what.into(), Instant::now()));
            self.notice_is_news.store(true, Ordering::Relaxed);
        }
    }

    /// Whether the notice showing now came from `tell` rather than `note`.
    pub fn notice_is_news(&self) -> bool {
        self.notice_is_news.load(Ordering::Relaxed)
    }

    pub fn set_reach(&self, ok: bool, why: impl Into<String>) {
        if let Ok(mut r) = self.reach.lock() {
            *r = Some((ok, why.into()));
        }
    }

    pub fn reach(&self) -> Option<(bool, String)> {
        self.reach.lock().ok().and_then(|r| r.clone())
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
    pub fn auto_approve(&self) -> bool {
        self.auto_approve.load(Ordering::Relaxed)
    }

    pub fn set_auto_approve(&self, on: bool) {
        self.auto_approve.store(on, Ordering::Relaxed);
    }

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

    pub fn hidden(&self) -> bool {
        self.hidden.load(Ordering::Relaxed)
    }

    /// Returns the new state.
    pub fn toggle_hidden(&self) -> bool {
        !self.hidden.fetch_xor(true, Ordering::Relaxed)
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

    /// The switch itself, for a microphone to read directly.
    pub fn mic_flag(&self) -> Arc<AtomicBool> {
        Arc::clone(&self.mic_on)
    }

    /// Peak level 0.0-1.0. Reading does not clear it; the capture side owns
    /// the reset so the display can be redrawn as often as it likes without
    /// stealing readings from itself.
    pub fn mic_peak(&self) -> f32 {
        self.mic_peak.load(Ordering::Relaxed) as f32 / 1000.0
    }

    pub fn mic_name(&self) -> String {
        self.mic_name.lock().map(|n| n.clone()).unwrap_or_default()
    }

    pub fn set_mic_name(&self, name: String) {
        if let Ok(mut n) = self.mic_name.lock() {
            *n = name;
        }
    }

    pub fn set_mic_peak(&self, level: f32) {
        self.mic_peak
            .store((level.clamp(0.0, 1.0) * 1000.0) as u32, Ordering::Relaxed);
    }

    /// Peak level of the shared application's own audio, 0.0-1.0, excluding
    /// the microphone. The mic is deliberately left out: mixing it in would
    /// mean talking over a silent game made the game look like it was
    /// working.
    pub fn app_peak(&self) -> f32 {
        self.app_peak.load(Ordering::Relaxed) as f32 / 1000.0
    }

    pub fn set_app_peak(&self, level: f32) {
        self.app_peak
            .store((level.clamp(0.0, 1.0) * 1000.0) as u32, Ordering::Relaxed);
    }

    /// False while the application's audio could not be captured at all, as
    /// opposed to captured and silent.
    pub fn app_audio_ok(&self) -> bool {
        self.app_audio_ok.load(Ordering::Relaxed)
    }

    pub fn set_app_audio_ok(&self, ok: bool) {
        self.app_audio_ok.store(ok, Ordering::Relaxed);
        if !ok {
            self.app_peak.store(0, Ordering::Relaxed);
        }
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
    fn the_code_and_link_outlive_the_waiting_phase() {
        // The window reads them while live, so they must still be there once
        // somebody has connected, which is exactly when inviting a second
        // person becomes possible.
        let s = Session::default();
        assert_eq!(s.share(), None, "nothing offered yet");

        s.set_phase(Phase::Waiting {
            code: Some("ABC234".into()),
            link: "https://relay.example/ABC234".into(),
        });
        s.set_phase(Phase::Live);

        assert_eq!(
            s.share(),
            Some((Some("ABC234".into()), "https://relay.example/ABC234".into()))
        );
    }

    #[test]
    fn app_peak_round_trips_and_clamps() {
        let s = Session::default();
        s.set_app_peak(0.25);
        assert!((s.app_peak() - 0.25).abs() < 0.002);

        s.set_app_peak(3.0);
        assert_eq!(s.app_peak(), 1.0, "over-range input is clamped, not wrapped");
    }

    #[test]
    fn audio_starts_assumed_working_and_a_failure_zeroes_the_level() {
        // The meter must not keep showing the last level a dead capture
        // produced: a frozen bar reads as "still working", which is the exact
        // wrong answer to the only question it is there to answer.
        let s = Session::default();
        assert!(s.app_audio_ok(), "nothing has failed yet");

        s.set_app_peak(0.8);
        s.set_app_audio_ok(false);
        assert!(!s.app_audio_ok());
        assert_eq!(s.app_peak(), 0.0, "a dead capture reads as silent, not as loud");

        s.set_app_audio_ok(true);
        assert!(s.app_audio_ok());
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
    fn a_session_asks_before_letting_anyone_in_unless_told_not_to() {
        // The default matters more than the feature. Anything that reads a
        // missing or unreadable setting as "let them in" would hand out the
        // screen on a typo.
        let s = Session::default();
        assert!(!s.auto_approve(), "asking is the default");

        s.set_auto_approve(true);
        assert!(s.auto_approve());
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
    fn a_window_title_containing_a_dash_survives_intact() {
        // The reason the two halves are stored separately. Almost every
        // browser and editor window is titled "document - application", so any
        // single string that had to be split apart again would lose half the
        // title to the first separator it found.
        let s = Session::default();
        s.set_source("chrome.exe", "Meme Harvester - Google Chrome");
        assert_eq!(
            s.source(),
            ("chrome.exe".to_owned(), "Meme Harvester - Google Chrome".to_owned())
        );
    }

    #[test]
    fn a_source_with_no_title_reports_an_empty_one() {
        let s = Session::default();
        s.set_source("solo.exe", "");
        assert_eq!(s.source(), ("solo.exe".to_owned(), String::new()));
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
        // Reading does not consume it, the window redraws many times a second
        // and a notice that vanished on first paint would never be seen.
        assert!(s.notice().is_some());
    }

    #[test]
    fn hiding_toggles_and_reports_the_new_state() {
        let s = Session::default();
        assert!(!s.hidden());
        assert!(s.toggle_hidden(), "toggling from off returns on");
        assert!(s.hidden());
        assert!(!s.toggle_hidden());
        assert!(!s.hidden());
    }

    #[test]
    fn the_mic_button_and_every_microphone_share_one_switch() {
        // The bug this guards: the window's button flipped a flag nothing
        // read, and only the hotkey reached the microphone itself.
        let s = Session::default();
        let a = s.mic_flag();
        let b = s.mic_flag();
        s.set_mic_on(true);
        assert!(a.load(Ordering::Relaxed) && b.load(Ordering::Relaxed));
        b.store(false, Ordering::Relaxed);
        assert!(!s.mic_on(), "a change from either side is seen by the other");
    }

    fn viewer(id: u64, browser: &str, address: &str) -> ViewerInfo {
        ViewerInfo {
            id,
            device: "Android, Chrome".into(),
            address: address.into(),
            browser: browser.into(),
            since: Instant::now(),
        }
    }

    #[test]
    fn viewers_are_counted_as_they_come_and_go() {
        let s = Session::default();
        assert_eq!(s.watching(), 0);
        s.add_viewer(viewer(1, "a", "203.0.113.9"));
        s.add_viewer(viewer(2, "b", "203.0.113.9"));
        assert_eq!(s.watching(), 2);
        assert_eq!(s.remove_viewer(1).map(|v| v.id), Some(1));
        assert_eq!(s.watching(), 1);
        assert_eq!(s.remove_viewer(1), None, "nobody is removed twice");
    }

    #[test]
    fn removing_somebody_keeps_their_browser_out_but_not_their_house() {
        let s = Session::default();
        s.add_viewer(viewer(7, "stranger", "203.0.113.9"));
        s.kick(7);
        assert!(s.is_kicked(7));
        assert!(s.is_banned("stranger", "198.51.100.1"), "the same browser, anywhere");
        assert!(
            !s.is_banned("sofa", "203.0.113.9"),
            "somebody else behind the same address is not caught by it"
        );
    }

    #[test]
    fn a_browser_that_keeps_no_name_is_kept_out_by_address() {
        let s = Session::default();
        s.add_viewer(viewer(3, "", "203.0.113.9"));
        s.kick(3);
        assert!(s.is_banned("", "203.0.113.9"));
    }

    #[test]
    fn a_new_link_is_acted_on_once() {
        let s = Session::default();
        assert!(!s.take_new_link());
        s.request_new_link();
        assert!(s.new_link_pending());
        assert!(s.take_new_link());
        assert!(!s.take_new_link());
        assert!(!s.new_link_pending());
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
