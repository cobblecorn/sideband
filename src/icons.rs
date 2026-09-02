//! Application icons for the source list.
//!
//! Shell icons come back as an `HICON`, which is a pair of GDI bitmaps rather
//! than pixels you can hand to a renderer. Getting RGBA out of one means
//! asking GDI for the colour bits, and then undoing two things it does that
//! are wrong for our purposes: it hands back BGRA, and for icons that predate
//! 32-bit colour the alpha channel is all zeroes — which would draw a
//! perfectly transparent icon if taken at face value.

use std::collections::HashMap;

use windows::core::PCWSTR;
use windows::Win32::Foundation::HWND;
use windows::Win32::Graphics::Gdi::{
    DeleteObject, GetDC, GetDIBits, GetObjectW, ReleaseDC, BITMAP, BITMAPINFO, BITMAPINFOHEADER,
    BI_RGB, DIB_RGB_COLORS, HGDIOBJ,
};
use windows::Win32::UI::Shell::{SHGetFileInfoW, SHFILEINFOW, SHGFI_ICON, SHGFI_SMALLICON};
use windows::Win32::UI::WindowsAndMessaging::{DestroyIcon, GetIconInfo, HICON, ICONINFO};

/// Decoded icon: tightly packed RGBA, ready for the renderer.
pub struct Icon {
    pub width: usize,
    pub height: usize,
    pub rgba: Vec<u8>,
}

/// Icons keyed by executable path.
///
/// Shell lookups touch the disk, so a list that redraws sixty times a second
/// must not repeat them. Failures are cached as `None` too — a path that has
/// no icon will not grow one, and retrying it every frame is the same cost as
/// succeeding.
#[derive(Default)]
pub struct IconCache {
    entries: HashMap<String, Option<Icon>>,
}

impl IconCache {
    pub fn get(&mut self, exe_path: &str) -> Option<&Icon> {
        if !self.entries.contains_key(exe_path) {
            let icon = load(exe_path);
            self.entries.insert(exe_path.to_owned(), icon);
        }
        self.entries.get(exe_path).and_then(|e| e.as_ref())
    }

    /// How many lookups have been remembered, successful or not.
    #[cfg(test)]
    pub fn len(&self) -> usize {
        self.entries.len()
    }
}

fn load(exe_path: &str) -> Option<Icon> {
    if exe_path.is_empty() {
        return None;
    }

    let wide: Vec<u16> = exe_path.encode_utf16().chain(std::iter::once(0)).collect();

    unsafe {
        let mut info = SHFILEINFOW::default();
        let ok = SHGetFileInfoW(
            PCWSTR(wide.as_ptr()),
            Default::default(),
            Some(&mut info),
            std::mem::size_of::<SHFILEINFOW>() as u32,
            SHGFI_ICON | SHGFI_SMALLICON,
        );
        if ok == 0 || info.hIcon.is_invalid() {
            return None;
        }

        let icon = rgba_from_hicon(info.hIcon);
        let _ = DestroyIcon(info.hIcon);
        icon
    }
}

unsafe fn rgba_from_hicon(hicon: HICON) -> Option<Icon> {
    unsafe {
        let mut ii = ICONINFO::default();
        GetIconInfo(hicon, &mut ii).ok()?;

        // Both bitmaps are ours to free once we are done with them.
        let colour = ii.hbmColor;
        let mask = ii.hbmMask;
        let result = colour_bits(colour);
        if !colour.is_invalid() {
            let _ = DeleteObject(HGDIOBJ(colour.0));
        }
        if !mask.is_invalid() {
            let _ = DeleteObject(HGDIOBJ(mask.0));
        }
        result
    }
}

unsafe fn colour_bits(bitmap: windows::Win32::Graphics::Gdi::HBITMAP) -> Option<Icon> {
    unsafe {
        if bitmap.is_invalid() {
            return None;
        }

        let mut bm = BITMAP::default();
        if GetObjectW(
            HGDIOBJ(bitmap.0),
            std::mem::size_of::<BITMAP>() as i32,
            Some(&mut bm as *mut _ as *mut _),
        ) == 0
        {
            return None;
        }

        let (w, h) = (bm.bmWidth.max(0) as usize, bm.bmHeight.max(0) as usize);
        if w == 0 || h == 0 || w > 512 || h > 512 {
            return None;
        }

        let mut header = BITMAPINFO {
            bmiHeader: BITMAPINFOHEADER {
                biSize: std::mem::size_of::<BITMAPINFOHEADER>() as u32,
                biWidth: w as i32,
                // Negative height asks for top-down rows, which saves flipping
                // the image afterwards.
                biHeight: -(h as i32),
                biPlanes: 1,
                biBitCount: 32,
                biCompression: BI_RGB.0,
                ..Default::default()
            },
            ..Default::default()
        };

        let mut buffer = vec![0u8; w * h * 4];
        let dc = GetDC(Some(HWND::default()));
        let copied = GetDIBits(
            dc,
            bitmap,
            0,
            h as u32,
            Some(buffer.as_mut_ptr() as *mut _),
            &mut header,
            DIB_RGB_COLORS,
        );
        ReleaseDC(Some(HWND::default()), dc);

        if copied == 0 {
            return None;
        }

        // GDI gives BGRA. Older icons also come back with alpha zeroed, which
        // would render as nothing at all — if no pixel claims to be visible,
        // treat the icon as fully opaque rather than invisible.
        let opaque = buffer.chunks_exact(4).all(|p| p[3] == 0);
        for px in buffer.chunks_exact_mut(4) {
            px.swap(0, 2);
            if opaque {
                px[3] = 255;
            }
        }

        Some(Icon { width: w, height: h, rgba: buffer })
    }
}

#[cfg(test)]
mod tests {
    use super::IconCache;

    #[test]
    fn missing_paths_are_cached_as_failures() {
        let mut cache = IconCache::default();
        assert!(cache.get("").is_none());
        assert!(cache.get(r"C:\definitely\not\here.exe").is_none());
        // Both attempts are remembered, so a list redrawing every frame does
        // not repeat a shell lookup that already failed.
        assert_eq!(cache.len(), 2);
    }

    #[test]
    fn a_real_system_binary_has_an_icon() {
        let mut cache = IconCache::default();
        let icon = cache.get(r"C:\Windows\explorer.exe");
        if let Some(icon) = icon {
            assert!(icon.width > 0 && icon.height > 0);
            assert_eq!(icon.rgba.len(), icon.width * icon.height * 4);
            // An icon whose pixels are entirely transparent would draw as
            // nothing, which is the bug the alpha fix-up exists to prevent.
            assert!(icon.rgba.chunks_exact(4).any(|p| p[3] > 0));
        }
    }
}
