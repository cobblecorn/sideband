//! Global hotkeys: the microphone, and covering the picture.
//!
//! Global rather than in-window on purpose: the moment you want to talk, or
//! to hide something, you are in a fullscreen game, and alt-tabbing to click a
//! button is exactly the friction this is meant to avoid.
//!
//! `RegisterHotKey` delivers `WM_HOTKEY` to a thread's message queue rather
//! than to a window, so this owns a thread and pumps messages on it.
//!
//! One thread for the whole process, registered once, pointed at whichever
//! session is current. It used to be started per viewer, which went wrong in
//! two ways at once: a combination can only be registered once, so every
//! viewer after the first had a hotkey that did nothing, and when the first
//! viewer left, the thread holding the registration carried on flipping the
//! switch of a microphone that had been closed.

use std::sync::{Arc, Mutex, OnceLock, Weak};

use windows::Win32::UI::Input::KeyboardAndMouse::{
    RegisterHotKey, HOT_KEY_MODIFIERS, MOD_ALT, MOD_CONTROL, MOD_NOREPEAT,
};
use windows::Win32::UI::WindowsAndMessaging::{GetMessageW, MSG, WM_HOTKEY};

use crate::chime::{self, Chime};
use crate::session::Session;

pub const MIC_KEY: &str = "Ctrl+Alt+M";
pub const HIDE_KEY: &str = "Ctrl+Alt+H";

const MIC_ID: i32 = 1;
const HIDE_ID: i32 = 2;

/// The session the keys act on. Weak, so a finished session is let go of
/// rather than kept alive by a key nobody has pressed.
static TARGET: Mutex<Option<Weak<Session>>> = Mutex::new(None);

/// Whether each key could be registered. Decided once, when the thread starts.
static REGISTERED: OnceLock<Registered> = OnceLock::new();

#[derive(Clone, Copy, Debug, Default)]
pub struct Registered {
    pub mic: bool,
    pub hide: bool,
}

/// Points the keys at this session, starting the listening thread the first
/// time. Something else on the machine may already own a combination, in
/// which case that one simply does nothing, and the answer says which.
pub fn attach(session: &Arc<Session>) -> Registered {
    if let Ok(mut t) = TARGET.lock() {
        *t = Some(Arc::downgrade(session));
    }
    *REGISTERED.get_or_init(start)
}

fn start() -> Registered {
    let (tx, rx) = std::sync::mpsc::channel::<Registered>();

    std::thread::spawn(move || unsafe {
        // NOREPEAT stops a held key from toggling dozens of times.
        let modifiers: HOT_KEY_MODIFIERS = MOD_CONTROL | MOD_ALT | MOD_NOREPEAT;
        // Hotkeys are per-thread when the window handle is null, which is why
        // registration and the message pump must live on the same thread.
        let registered = Registered {
            mic: RegisterHotKey(None, MIC_ID, modifiers, b'M' as u32).is_ok(),
            hide: RegisterHotKey(None, HIDE_ID, modifiers, b'H' as u32).is_ok(),
        };
        let _ = tx.send(registered);
        if !registered.mic && !registered.hide {
            return;
        }

        // For the life of the process. The registrations go when it does.
        let mut msg = MSG::default();
        while GetMessageW(&mut msg, None, 0, 0).as_bool() {
            if msg.message != WM_HOTKEY {
                continue;
            }
            let Some(session) = TARGET.lock().ok().and_then(|t| t.as_ref()?.upgrade()) else {
                continue;
            };
            pressed(msg.wParam.0 as i32, &session);
        }
    });

    rx.recv_timeout(std::time::Duration::from_secs(2)).unwrap_or_default()
}

fn pressed(id: i32, session: &Session) {
    // Heard rather than seen: whoever pressed this is looking at a game, not
    // at the window, and needs to know which way it went.
    match id {
        MIC_ID if session.mic_available() => {
            let now = !session.mic_on();
            session.set_mic_on(now);
            if session.sounds() {
                chime::play(if now { Chime::MicOn } else { Chime::MicOff });
            }
        }
        HIDE_ID => {
            let now = session.toggle_hidden();
            if session.sounds() {
                chime::play(if now { Chime::Hidden } else { Chime::Shown });
            }
        }
        _ => {}
    }
}
