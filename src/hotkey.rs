//! Global hotkey for the mic toggle.
//!
//! Global rather than in-window on purpose: the moment you want to talk you
//! are in a fullscreen game, and alt-tabbing to click a button is exactly the
//! friction this is meant to avoid.
//!
//! `RegisterHotKey` delivers `WM_HOTKEY` to a thread's message queue rather
//! than to a window, so this owns a thread and pumps messages on it.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;

use windows::Win32::UI::Input::KeyboardAndMouse::{
    RegisterHotKey, UnregisterHotKey, HOT_KEY_MODIFIERS, MOD_ALT, MOD_CONTROL, MOD_NOREPEAT,
};
use windows::Win32::UI::WindowsAndMessaging::{GetMessageW, MSG, WM_HOTKEY};

const HOTKEY_ID: i32 = 1;

/// Ctrl+Alt+M. NOREPEAT stops a held key from toggling dozens of times.
pub const DESCRIPTION: &str = "Ctrl+Alt+M";

/// Spawns a thread that flips `flag` whenever the hotkey is pressed, calling
/// `on_toggle` with the new state. Returns false if the combination could not
/// be registered — usually because something else already owns it.
pub fn spawn_toggle<F>(flag: Arc<AtomicBool>, on_toggle: F) -> bool
where
    F: Fn(bool) + Send + 'static,
{
    let (tx, rx) = std::sync::mpsc::channel::<bool>();

    std::thread::spawn(move || unsafe {
        let modifiers: HOT_KEY_MODIFIERS = MOD_CONTROL | MOD_ALT | MOD_NOREPEAT;
        // Hotkeys are per-thread when the window handle is null, which is why
        // registration and the message pump must live on the same thread.
        if RegisterHotKey(None, HOTKEY_ID, modifiers, b'M' as u32).is_err() {
            let _ = tx.send(false);
            return;
        }
        let _ = tx.send(true);

        let mut msg = MSG::default();
        while GetMessageW(&mut msg, None, 0, 0).as_bool() {
            if msg.message == WM_HOTKEY && msg.wParam.0 as i32 == HOTKEY_ID {
                let now = !flag.load(Ordering::Relaxed);
                flag.store(now, Ordering::Relaxed);
                on_toggle(now);
            }
        }

        let _ = UnregisterHotKey(None, HOTKEY_ID);
    });

    rx.recv_timeout(std::time::Duration::from_secs(2)).unwrap_or(false)
}
