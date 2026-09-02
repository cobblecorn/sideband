//! The Sideband mark.
//!
//! A sideband is the band of frequencies beside a carrier wave, and
//! single-sideband radio transmits one of them and suppresses the rest, a
//! narrow, point-to-point signal with everything else left out. That is what
//! this software does with a desktop, so that is what the mark draws: a tall
//! carrier, two bands falling away to the right, and the pair on the left
//! faded down to almost nothing.
//!
//! It is described as geometry rather than kept as a file. The window icon,
//! the executable's icon and every size in between are rasterised from these
//! numbers, so they cannot drift apart, `build.rs` includes this same source
//! to generate the `.ico`, which is why nothing here may depend on anything
//! outside `core`.

/// The mark is designed on a 512-unit square, which is also the largest size
/// Windows asks for.
pub const GRID: f32 = 512.0;

/// The signal. Straight out of the application's own palette, so the icon and
/// the window it launches are visibly the same product.
pub const AMBER: [u8; 3] = [0xf0, 0xa9, 0x3b];

/// The ground it sits on.
pub const TILE: [u8; 3] = [0x14, 0x18, 0x1d];

/// Corner radius of the tile, in grid units, the proportion Windows and
/// macOS both round application icons to.
const TILE_RADIUS: f32 = 114.0;

/// One bar of the mark.
pub struct Bar {
    pub x: f32,
    pub y: f32,
    pub w: f32,
    pub h: f32,
    /// How much of the signal reaches this band. The suppressed pair is drawn
    /// in the same amber, simply attenuated, one hue, not two.
    pub alpha: f32,
}

impl Bar {
    const fn new(x: f32, y: f32, w: f32, h: f32, alpha: f32) -> Self {
        Self { x, y, w, h, alpha }
    }

    /// Fully round caps. A bar is a capsule, never a rectangle with softened
    /// corners.
    fn radius(&self) -> f32 {
        self.w / 2.0
    }
}

/// Carrier, two sidebands right, two suppressed left.
pub const FULL: [Bar; 5] = [
    Bar::new(226.0, 96.0, 60.0, 320.0, 1.00),
    Bar::new(314.0, 156.0, 60.0, 200.0, 1.00),
    Bar::new(402.0, 198.0, 60.0, 116.0, 1.00),
    Bar::new(138.0, 156.0, 60.0, 200.0, 0.28),
    Bar::new(50.0, 198.0, 60.0, 116.0, 0.28),
];

/// The small cut: three heavier bars.
///
/// Not the five-bar mark scaled down. At 32 pixels the outer pair is under two
/// pixels wide and turns into grey fringing rather than a band, so the mark is
/// redrawn with fewer, thicker strokes and the same silhouette. Redrawing
/// instead of scaling is most of the difference between an icon and a logo.
pub const COMPACT: [Bar; 3] = [
    Bar::new(214.0, 86.0, 84.0, 340.0, 1.00),
    Bar::new(332.0, 146.0, 84.0, 220.0, 1.00),
    Bar::new(96.0, 146.0, 84.0, 220.0, 0.30),
];

/// Below this many pixels, the compact cut is used.
pub const COMPACT_BELOW: u32 = 48;

pub fn bars(size: u32) -> &'static [Bar] {
    if size < COMPACT_BELOW { &COMPACT } else { &FULL }
}

/// Samples per axis when rasterising. Sixteen samples a pixel is plenty for
/// shapes made only of straight edges and circular arcs, and this runs a
/// handful of times at start-up rather than per frame.
const SAMPLES: u32 = 4;

/// The mark on its tile, as straight (non-premultiplied) RGBA, row-major from
/// the top left, the layout both `egui` and the ICO format want.
pub fn rgba(size: u32) -> Vec<u8> {
    let bars = bars(size);
    let scale = GRID / size as f32;
    let step = scale / SAMPLES as f32;
    let mut out = vec![0u8; (size as usize) * (size as usize) * 4];

    for py in 0..size {
        for px in 0..size {
            // Accumulated premultiplied, so that partly covered samples at the
            // tile's rounded corners average into the right colour instead of
            // dragging black in from the transparent side.
            let (mut r, mut g, mut b, mut a) = (0.0f32, 0.0f32, 0.0f32, 0.0f32);

            for sy in 0..SAMPLES {
                for sx in 0..SAMPLES {
                    let x = px as f32 * scale + (sx as f32 + 0.5) * step;
                    let y = py as f32 * scale + (sy as f32 + 0.5) * step;

                    if !inside(x, y, 0.0, 0.0, GRID, GRID, TILE_RADIUS) {
                        continue;
                    }

                    let (mut sr, mut sg, mut sb) =
                        (TILE[0] as f32, TILE[1] as f32, TILE[2] as f32);

                    // Every bar sits well inside the tile, so the ground under
                    // one is always opaque and a straight over-composite is
                    // exact here.
                    for bar in bars {
                        if inside(x, y, bar.x, bar.y, bar.w, bar.h, bar.radius()) {
                            let k = bar.alpha;
                            sr = AMBER[0] as f32 * k + sr * (1.0 - k);
                            sg = AMBER[1] as f32 * k + sg * (1.0 - k);
                            sb = AMBER[2] as f32 * k + sb * (1.0 - k);
                        }
                    }

                    r += sr;
                    g += sg;
                    b += sb;
                    a += 1.0;
                }
            }

            let i = ((py as usize) * (size as usize) + px as usize) * 4;
            if a > 0.0 {
                out[i] = (r / a).round() as u8;
                out[i + 1] = (g / a).round() as u8;
                out[i + 2] = (b / a).round() as u8;
                out[i + 3] = ((a / (SAMPLES * SAMPLES) as f32) * 255.0).round() as u8;
            }
        }
    }

    out
}

/// Whether a point falls inside a rounded rectangle.
fn inside(px: f32, py: f32, x: f32, y: f32, w: f32, h: f32, r: f32) -> bool {
    if px < x || px > x + w || py < y || py > y + h {
        return false;
    }
    // The nearest point on the rectangle the corner arcs are centred on. Away
    // from the corners this lands on the sample itself and the test is free.
    let cx = px.clamp(x + r, x + w - r);
    let cy = py.clamp(y + r, y + h - r);
    let (dx, dy) = (px - cx, py - cy);
    dx * dx + dy * dy <= r * r
}

#[cfg(test)]
mod tests {
    use super::*;

    fn pixel(rgba: &[u8], size: u32, x: u32, y: u32) -> [u8; 4] {
        let i = ((y as usize) * (size as usize) + x as usize) * 4;
        [rgba[i], rgba[i + 1], rgba[i + 2], rgba[i + 3]]
    }

    #[test]
    fn the_mark_renders_at_every_size_windows_asks_for() {
        for size in [16u32, 24, 32, 48, 64, 128, 256] {
            let px = rgba(size);
            assert_eq!(px.len(), (size * size * 4) as usize, "at {size}");
        }
    }

    #[test]
    fn the_carrier_is_amber_and_the_ground_is_not() {
        let size = 256;
        let px = rgba(size);

        // Dead centre falls on the carrier bar.
        assert_eq!(pixel(&px, size, size / 2, size / 2), [AMBER[0], AMBER[1], AMBER[2], 255]);

        // A little above the bars is bare tile.
        let ground = pixel(&px, size, size / 2, 20);
        assert_eq!(&ground[..3], &TILE[..]);
        assert_eq!(ground[3], 255);
    }

    #[test]
    fn the_left_pair_is_attenuated_not_absent() {
        // The whole idea of the mark is one band transmitted and one
        // suppressed. If the suppressed pair rendered identically to the
        // carrier, or vanished entirely, the mark would be saying something
        // else.
        let size = 256;
        let px = rgba(size);
        let y = size / 2;

        let left = pixel(&px, size, (168.0 / GRID * size as f32) as u32, y);
        let right = pixel(&px, size, (344.0 / GRID * size as f32) as u32, y);

        assert_eq!(&right[..3], &AMBER[..], "the transmitted side is full strength");
        assert_ne!(&left[..3], &AMBER[..], "and the suppressed side is not");
        assert_ne!(&left[..3], &TILE[..], "but it is still there");
    }

    #[test]
    fn the_corners_are_rounded_away() {
        let size = 128;
        let px = rgba(size);
        assert_eq!(pixel(&px, size, 0, 0)[3], 0, "the tile does not fill its own corner");
        assert_eq!(pixel(&px, size, size - 1, size - 1)[3], 0);
        assert_eq!(pixel(&px, size, size / 2, 0)[3], 255, "but its edges are square");
    }

    #[test]
    fn small_sizes_get_the_redrawn_cut() {
        assert_eq!(bars(16).len(), 3);
        assert_eq!(bars(32).len(), 3);
        assert_eq!(bars(48).len(), 5);
        assert_eq!(bars(256).len(), 5);
    }

    #[test]
    fn every_bar_sits_inside_the_tile() {
        // The rasteriser composites bars onto opaque ground and would be
        // subtly wrong if one crossed a rounded corner.
        for bar in FULL.iter().chain(COMPACT.iter()) {
            for (x, y) in [
                (bar.x, bar.y),
                (bar.x + bar.w, bar.y),
                (bar.x, bar.y + bar.h),
                (bar.x + bar.w, bar.y + bar.h),
            ] {
                assert!(
                    inside(x, y, 0.0, 0.0, GRID, GRID, TILE_RADIUS),
                    "corner {x},{y} escapes the tile"
                );
            }
        }
    }
}
