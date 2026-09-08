//! Stage 1, enumerate candidate capture sources.
//!
//! Video capture will want the HWND; audio capture wants the PID. We collect
//! both, then collapse to one row per process: a game typically owns several
//! windows (splash, launcher, overlay) and for audio purposes they are all the
//! same target. Keeping the largest window per PID is what makes the list
//! readable rather than a wall of duplicates.

use std::collections::HashMap;
use std::ffi::c_void;

use windows::core::BOOL;
use windows::Win32::Foundation::{FALSE, HWND, LPARAM, RECT, TRUE};
use windows::Win32::System::Threading::{
    OpenProcess, QueryFullProcessImageNameW, PROCESS_NAME_WIN32,
    PROCESS_QUERY_LIMITED_INFORMATION,
};
use windows::Win32::UI::WindowsAndMessaging::{
    EnumChildWindows, EnumWindows, GetClassNameW, GetWindowRect, GetWindowTextLengthW,
    GetWindowTextW, GetWindowThreadProcessId, IsWindowVisible,
};

#[derive(Clone)]
pub struct Source {
    /// Unused until stage 2 (Windows.Graphics.Capture takes the HWND;
    /// process loopback takes the PID).
    #[allow(dead_code)]
    pub hwnd: HWND,
    pub pid: u32,
    pub title: String,
    pub exe: String,
    /// Full path, kept because the shell needs it to find the icon. `exe` is
    /// just the file name and is what gets shown.
    pub path: String,
    pub area: i64,
    /// The window is drawn by the frame host on behalf of a packaged
    /// application we could not identify, so its audio cannot be captured.
    /// See `hosted_app`. Video is unaffected.
    pub frame_hosted: bool,
}

unsafe extern "system" fn enum_proc(hwnd: HWND, lparam: LPARAM) -> BOOL {
    let out = unsafe { &mut *(lparam.0 as *mut Vec<Source>) };

    unsafe {
        if !IsWindowVisible(hwnd).as_bool() {
            return TRUE;
        }

        let len = GetWindowTextLengthW(hwnd);
        if len <= 0 {
            return TRUE;
        }

        let mut buf = vec![0u16; len as usize + 1];
        let n = GetWindowTextW(hwnd, &mut buf);
        if n <= 0 {
            return TRUE;
        }
        let title = String::from_utf16_lossy(&buf[..n as usize]);

        // The desktop and taskbar are visible, titled and enumerable, but
        // Windows.Graphics.Capture produces no frames for them, offering one
        // gives a stream that connects and then shows nothing at all.
        if is_shell_window(hwnd) {
            return TRUE;
        }

        let mut pid = 0u32;
        GetWindowThreadProcessId(hwnd, Some(&mut pid));
        if pid == 0 {
            return TRUE;
        }
        // Not necessarily the process that makes the sound. See `hosted_app`.
        let pid = hosted_app(hwnd).unwrap_or(pid);

        let mut rect = RECT::default();
        let area = if GetWindowRect(hwnd, &mut rect).is_ok() {
            ((rect.right - rect.left) as i64) * ((rect.bottom - rect.top) as i64)
        } else {
            0
        };

        let path = exe_path(pid);
        let exe = path
            .rsplit('\\')
            .next()
            .unwrap_or("?")
            .to_string();
        // Still the frame host means the resolution above found nothing, and
        // the process actually making the sound is one we cannot name.
        let frame_hosted = exe.eq_ignore_ascii_case(FRAME_HOST);
        out.push(Source { hwnd, pid, title, exe, path, area, frame_hosted });
    }

    TRUE
}

/// The class of the window a packaged application draws into.
const CORE_WINDOW: &str = "Windows.UI.Core.CoreWindow";

/// The process that owns a packaged application's frame.
pub const FRAME_HOST: &str = "ApplicationFrameHost.exe";

/// The process actually behind a window, where that is not the process that
/// owns it.
///
/// A packaged ("Store") application does not own its own frame. The visible,
/// titled, enumerable window belongs to `ApplicationFrameHost.exe`, and the
/// application runs in a separate process which is a *sibling* of the frame
/// host, not a child of it.
///
/// Video capture does not care, the frame host's window is showing the
/// application. Audio does, and badly: process loopback scoped to the frame
/// host's tree captures a process that never makes a sound, so the viewer gets
/// a perfect picture in total silence. Nothing reports this. The loopback API
/// accepts any process id at all, including one that does not exist, and
/// answers with an unbroken stream of silent buffers, so there is no error to
/// notice and no way to tell this apart from an application that happens to be
/// quiet.
///
/// Where the frame exposes the application's `Windows.UI.Core.CoreWindow` as
/// a child, that window's process is the answer. It does not always: on
/// Windows 11 the frame is routinely childless when enumerated from outside
/// the process, and then there is nothing here to find. Callers are told as
/// much through `Source::frame_hosted`, because a source whose audio silently
/// cannot work is worth saying out loud rather than leaving to be discovered
/// by a viewer who cannot hear anything.
///
/// Returns `None` when there is no such window, which is also the ordinary
/// case for an ordinary application.
fn hosted_app(frame: HWND) -> Option<u32> {
    let mut found = 0u32;
    unsafe {
        let _ = EnumChildWindows(
            Some(frame),
            Some(core_window_owner),
            LPARAM(&mut found as *mut u32 as isize),
        );
    }
    (found != 0).then_some(found)
}

unsafe extern "system" fn core_window_owner(hwnd: HWND, lparam: LPARAM) -> BOOL {
    let out = unsafe { &mut *(lparam.0 as *mut u32) };

    unsafe {
        if class_of(hwnd).as_deref() != Some(CORE_WINDOW) {
            return TRUE;
        }
        let mut pid = 0u32;
        GetWindowThreadProcessId(hwnd, Some(&mut pid));
        if pid == 0 {
            return TRUE;
        }
        *out = pid;
    }

    // Found it; there is no second answer worth waiting for.
    FALSE
}

fn class_of(hwnd: HWND) -> Option<String> {
    let mut buf = [0u16; 64];
    let len = unsafe { GetClassNameW(hwnd, &mut buf) };
    if len <= 0 {
        return None;
    }
    Some(String::from_utf16_lossy(&buf[..len as usize]))
}

/// Whether this is part of the desktop shell rather than an application.
///
/// `Progman` and `WorkerW` are the desktop itself; the tray and start button
/// are the taskbar. All of them enumerate like ordinary windows and none of
/// them can be captured.
fn is_shell_window(hwnd: HWND) -> bool {
    const SHELL_CLASSES: [&str; 5] = [
        "Progman",
        "WorkerW",
        "Shell_TrayWnd",
        "Shell_SecondaryTrayWnd",
        "Button",
    ];

    match class_of(hwnd) {
        Some(class) => SHELL_CLASSES.contains(&class.as_str()),
        None => false,
    }
}

fn exe_path(pid: u32) -> String {
    unsafe {
        let Ok(h) = OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, false, pid) else {
            return String::from("?");
        };

        let mut buf = [0u16; 260];
        let mut len = buf.len() as u32;
        let ok = QueryFullProcessImageNameW(
            h,
            PROCESS_NAME_WIN32,
            windows::core::PWSTR(buf.as_mut_ptr()),
            &mut len,
        )
        .is_ok();

        let _ = windows::Win32::Foundation::CloseHandle(h);

        if !ok {
            return String::from("?");
        }

        String::from_utf16_lossy(&buf[..len as usize])
    }
}

/// Visible, titled windows collapsed to one entry per process, largest first.
pub fn list() -> windows::core::Result<Vec<Source>> {
    let mut found: Vec<Source> = Vec::new();

    unsafe {
        EnumWindows(
            Some(enum_proc),
            LPARAM(&mut found as *mut Vec<Source> as *mut c_void as isize),
        )?;
    }

    let mut best: HashMap<u32, Source> = HashMap::new();
    for s in found {
        best.entry(s.pid)
            .and_modify(|cur| {
                if s.area > cur.area {
                    *cur = s.clone();
                }
            })
            .or_insert(s);
    }

    let mut out: Vec<Source> = best.into_values().collect();
    out.sort_by(|a, b| b.area.cmp(&a.area).then_with(|| a.exe.cmp(&b.exe)));
    Ok(out)
}
