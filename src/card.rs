//! What viewers see while the host has hidden the picture.
//!
//! A still frame, drawn once per picture size and handed to the encoder in
//! place of the capture. It goes through exactly the path every captured frame
//! does, so hiding and showing are nothing but a change of picture to the
//! encoder: no frames dropped, no references broken, no keyframe to ask for,
//! and nothing that has to recover afterwards. Freezing the stream instead,
//! which is what pausing used to do, left the viewer looking at whatever was
//! on screen at the moment of pressing, and that is exactly the frame with the
//! password box or the private message in it.
//!
//! The mark, and the word PAUSED under it, in the window's own colours, so a
//! viewer can tell at a glance this is on purpose and not a broken stream.

use windows::Win32::Graphics::Direct3D11::{
    ID3D11Device, ID3D11Texture2D, D3D11_BIND_SHADER_RESOURCE, D3D11_SUBRESOURCE_DATA,
    D3D11_TEXTURE2D_DESC, D3D11_USAGE_DEFAULT,
};
use windows::Win32::Graphics::Dxgi::Common::{DXGI_FORMAT_B8G8R8A8_UNORM, DXGI_SAMPLE_DESC};

use crate::mark;

/// The lettering, from the window's secondary text colour.
const INK: [u8; 3] = [0x98, 0xa4, 0xb1];

/// Five by seven, a row per byte, the low five bits used, most significant
/// on the left. Only the letters the card needs.
const WORD: [[u8; 7]; 6] = [
    // P
    [0b11110, 0b10001, 0b10001, 0b11110, 0b10000, 0b10000, 0b10000],
    // A
    [0b01110, 0b10001, 0b10001, 0b11111, 0b10001, 0b10001, 0b10001],
    // U
    [0b10001, 0b10001, 0b10001, 0b10001, 0b10001, 0b10001, 0b01110],
    // S
    [0b01111, 0b10000, 0b10000, 0b01110, 0b00001, 0b00001, 0b11110],
    // E
    [0b11111, 0b10000, 0b10000, 0b11110, 0b10000, 0b10000, 0b11111],
    // D
    [0b11110, 0b10001, 0b10001, 0b10001, 0b10001, 0b10001, 0b11110],
];

/// Letter spacing, in the same blocks the letters are drawn with. Wide, like
/// the spaced capitals everywhere else in the window.
const GAP: u32 = 3;

/// The card as a texture on `device`, the same format and size as the frames
/// it stands in for.
pub fn texture(device: &ID3D11Device, width: u32, height: u32) -> Result<ID3D11Texture2D, String> {
    let pixels = pixels(width, height);
    let desc = D3D11_TEXTURE2D_DESC {
        Width: width,
        Height: height,
        MipLevels: 1,
        ArraySize: 1,
        Format: DXGI_FORMAT_B8G8R8A8_UNORM,
        SampleDesc: DXGI_SAMPLE_DESC { Count: 1, Quality: 0 },
        Usage: D3D11_USAGE_DEFAULT,
        BindFlags: D3D11_BIND_SHADER_RESOURCE.0 as u32,
        CPUAccessFlags: 0,
        MiscFlags: 0,
    };
    let initial = D3D11_SUBRESOURCE_DATA {
        pSysMem: pixels.as_ptr() as *const _,
        SysMemPitch: width * 4,
        SysMemSlicePitch: 0,
    };

    let mut texture: Option<ID3D11Texture2D> = None;
    unsafe { device.CreateTexture2D(&desc, Some(&initial), Some(&mut texture)) }
        .map_err(|e| format!("could not make the pause card: {e}"))?;
    texture.ok_or_else(|| "could not make the pause card".to_owned())
}

/// The card as BGRA, top row first.
pub fn pixels(width: u32, height: u32) -> Vec<u8> {
    let (w, h) = (width as usize, height as usize);
    let mut out = Vec::with_capacity(w * h * 4);
    for _ in 0..w * h {
        out.extend_from_slice(&[mark::TILE[2], mark::TILE[1], mark::TILE[0], 255]);
    }
    if w == 0 || h == 0 {
        return out;
    }

    // The mark: a fifth of the height, on its own tile, which is the same
    // colour as the ground and so disappears into it.
    let size = ((height as f32 * 0.20).round() as u32).clamp(8, 400).min(width).min(height);
    let block = (height / 150).max(1);
    let text_h = 7 * block;
    let text_w = (WORD.len() as u32 * (5 + GAP) - GAP) * block;
    let spacing = (size as f32 * 0.22) as u32;
    let with_text = text_w <= width && size + spacing + text_h <= height;

    let total = size + if with_text { spacing + text_h } else { 0 };
    let top = (height - total.min(height)) / 2;

    let icon = mark::rgba(size);
    let left = (width - size) / 2;
    for y in 0..size as usize {
        for x in 0..size as usize {
            let s = (y * size as usize + x) * 4;
            let a = icon[s + 3] as u32;
            if a == 0 {
                continue;
            }
            let d = ((top as usize + y) * w + left as usize + x) * 4;
            // Straight alpha over the ground, channel order swapped from RGBA.
            for (c, from) in [(0, 2), (1, 1), (2, 0)] {
                let under = out[d + c] as u32;
                out[d + c] = ((icon[s + from] as u32 * a + under * (255 - a)) / 255) as u8;
            }
        }
    }

    if with_text {
        let text_top = top + size + spacing;
        let text_left = (width - text_w) / 2;
        for (n, glyph) in WORD.iter().enumerate() {
            let gx = text_left + n as u32 * (5 + GAP) * block;
            for (row, bits) in glyph.iter().enumerate() {
                for col in 0..5u32 {
                    if bits & (0b10000 >> col) == 0 {
                        continue;
                    }
                    for by in 0..block {
                        for bx in 0..block {
                            let x = (gx + col * block + bx) as usize;
                            let y = (text_top + row as u32 * block + by) as usize;
                            let d = (y * w + x) * 4;
                            out[d..d + 3].copy_from_slice(&[INK[2], INK[1], INK[0]]);
                        }
                    }
                }
            }
        }
    }

    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn at(p: &[u8], w: u32, x: u32, y: u32) -> [u8; 4] {
        let i = ((y * w + x) * 4) as usize;
        [p[i], p[i + 1], p[i + 2], p[i + 3]]
    }

    #[test]
    fn the_card_is_the_size_it_stands_in_for_and_opaque() {
        let p = pixels(1920, 1080);
        assert_eq!(p.len(), 1920 * 1080 * 4);
        assert!(p.chunks(4).all(|px| px[3] == 255), "nothing see-through");
    }

    #[test]
    fn the_ground_is_the_window_colour_and_the_middle_is_not_empty() {
        let (w, h) = (1280, 720);
        let p = pixels(w, h);
        let ground = [mark::TILE[2], mark::TILE[1], mark::TILE[0], 255];
        assert_eq!(at(&p, w, 5, 5), ground);
        assert_eq!(at(&p, w, w - 5, h - 5), ground);

        // Some amber in the middle band, where the mark is.
        let amber = (h / 3..h / 2)
            .flat_map(|y| (w / 3..2 * w / 3).map(move |x| (x, y)))
            .any(|(x, y)| at(&p, w, x, y)[2] > 0xc0);
        assert!(amber, "the mark is drawn");

        // And lettering somewhere below it.
        let ink = [INK[2], INK[1], INK[0], 255];
        let lettered = (h / 2..h).any(|y| (0..w).any(|x| at(&p, w, x, y) == ink));
        assert!(lettered, "the word is drawn");
    }

    #[test]
    fn odd_and_tiny_sizes_do_not_panic() {
        for (w, h) in [(1, 1), (7, 3), (33, 17), (640, 361), (3440, 1440)] {
            assert_eq!(pixels(w, h).len(), (w * h * 4) as usize);
        }
    }
}
