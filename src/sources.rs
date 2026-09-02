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
use windows::Win32::Foundation::{HWND, LPARAM, RECT, TRUE};
use windows::Win32::System::Threading::{
    OpenProcess, QueryFullProcessImageNameW, PROCESS_NAME_WIN32,
    PROCESS_QUERY_LIMITED_INFORMATION,
};
use windows::Win32::UI::WindowsAndMessaging::{
    EnumWindows, GetClassNameW, GetWindowRect, GetWindowTextLengthW, GetWindowTextW,
    GetWindowThreadProcessId, IsWindowVisible,
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
        out.push(Source { hwnd, pid, title, exe, path, area });
    }

    TRUE
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

    let mut buf = [0u16; 64];
    let len = unsafe { GetClassNameW(hwnd, &mut buf) };
    if len <= 0 {
        return false;
    }
    let class = String::from_utf16_lossy(&buf[..len as usize]);
    SHELL_CLASSES.contains(&class.as_str())
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
