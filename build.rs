//! Puts the mark on the executable.
//!
//! The icon is rasterised here from `src/mark.rs` rather than kept as a file
//! next to it — the same geometry the window icon uses at runtime, so the two
//! cannot drift. That is also why `mark.rs` depends on nothing: this build
//! script includes its source directly, outside the crate it normally belongs
//! to.

#[cfg(windows)]
#[path = "src/mark.rs"]
mod mark;

fn main() {
    println!("cargo:rerun-if-changed=src/mark.rs");
    println!("cargo:rerun-if-changed=build.rs");

    #[cfg(windows)]
    windows_icon();
}

/// Every size Windows will ask for: the tray and the title bar at the bottom,
/// the extra-large view in Explorer at the top. Anything missing is scaled
/// from a neighbour by the shell, which is exactly the mush the compact cut
/// exists to avoid — so each one is drawn rather than left to be derived.
#[cfg(windows)]
const SIZES: &[u32] = &[16, 20, 24, 32, 40, 48, 64, 128, 256];

#[cfg(windows)]
fn windows_icon() {
    let out = std::path::PathBuf::from(std::env::var("OUT_DIR").expect("OUT_DIR"));
    let ico = out.join("sideband.ico");

    let images: Vec<(u32, Vec<u8>)> = SIZES.iter().map(|&s| (s, mark::rgba(s))).collect();
    std::fs::write(&ico, encode_ico(&images)).expect("could not write the icon");

    let mut res = winresource::WindowsResource::new();
    res.set_icon(ico.to_str().expect("icon path is not valid UTF-8"));
    res.set("FileDescription", "Sideband");
    res.set("ProductName", "Sideband");
    res.set("OriginalFilename", "sideband.exe");

    // A missing resource compiler is not a reason to fail a build. The
    // program is identical without this; it simply wears the default icon.
    if let Err(e) = res.compile() {
        println!("cargo:warning=could not attach the icon ({e}); building without it");
    }
}

/// Packs rasterised images into a Windows `.ico`.
///
/// Each entry is a device-independent bitmap rather than a PNG, which costs
/// some size and saves pulling in a deflate implementation to compress images
/// that are a few kilobytes of flat colour anyway.
#[cfg(windows)]
fn encode_ico(images: &[(u32, Vec<u8>)]) -> Vec<u8> {
    const DIR_ENTRY: usize = 16;
    const HEADER: usize = 40;

    let mut out = Vec::new();
    out.extend_from_slice(&0u16.to_le_bytes()); // reserved
    out.extend_from_slice(&1u16.to_le_bytes()); // 1 = icon, 2 = cursor
    out.extend_from_slice(&(images.len() as u16).to_le_bytes());

    let mut offset = 6 + images.len() * DIR_ENTRY;
    let mut bodies = Vec::new();

    for (size, rgba) in images {
        let body = dib(*size, rgba);

        // Zero means 256 in a byte-wide field; every other size fits.
        let dimension = if *size >= 256 { 0u8 } else { *size as u8 };
        out.push(dimension); // width
        out.push(dimension); // height
        out.push(0); // palette size, unused at 32 bits per pixel
        out.push(0); // reserved
        out.extend_from_slice(&1u16.to_le_bytes()); // colour planes
        out.extend_from_slice(&32u16.to_le_bytes()); // bits per pixel
        out.extend_from_slice(&(body.len() as u32).to_le_bytes());
        out.extend_from_slice(&(offset as u32).to_le_bytes());

        offset += body.len();
        bodies.push(body);
    }

    for body in bodies {
        out.extend_from_slice(&body);
    }

    debug_assert!(out.len() > 6 + images.len() * (DIR_ENTRY + HEADER));
    out
}

/// One image: a bitmap header, bottom-up BGRA pixels, and the 1-bit mask the
/// format still requires even when the pixels carry their own alpha.
#[cfg(windows)]
fn dib(size: u32, rgba: &[u8]) -> Vec<u8> {
    let width = size as i32;
    // Doubled, because the header describes the colour bitmap and the mask
    // stacked together.
    let height = size as i32 * 2;

    // Mask rows are one bit per pixel, padded out to four bytes.
    let mask_row = (size as usize).div_ceil(32) * 4;
    let mask_bytes = mask_row * size as usize;
    let pixel_bytes = (size as usize) * (size as usize) * 4;

    let mut out = Vec::with_capacity(40 + pixel_bytes + mask_bytes);
    out.extend_from_slice(&40u32.to_le_bytes()); // header size
    out.extend_from_slice(&width.to_le_bytes());
    out.extend_from_slice(&height.to_le_bytes());
    out.extend_from_slice(&1u16.to_le_bytes()); // planes
    out.extend_from_slice(&32u16.to_le_bytes()); // bits per pixel
    out.extend_from_slice(&0u32.to_le_bytes()); // BI_RGB, uncompressed
    out.extend_from_slice(&((pixel_bytes + mask_bytes) as u32).to_le_bytes());
    out.extend_from_slice(&0i32.to_le_bytes()); // pixels per metre, unused
    out.extend_from_slice(&0i32.to_le_bytes());
    out.extend_from_slice(&0u32.to_le_bytes()); // palette entries used
    out.extend_from_slice(&0u32.to_le_bytes()); // palette entries required

    // Bottom-up, and BGRA rather than RGBA.
    for y in (0..size).rev() {
        for x in 0..size {
            let i = ((y as usize) * (size as usize) + x as usize) * 4;
            out.push(rgba[i + 2]);
            out.push(rgba[i + 1]);
            out.push(rgba[i]);
            out.push(rgba[i + 3]);
        }
    }

    // All zeroes: "take the alpha channel at its word". Windows still expects
    // the mask to be present and correctly sized.
    out.resize(out.len() + mask_bytes, 0);
    out
}
