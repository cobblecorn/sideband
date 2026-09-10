//! Shrinking the picture before it is encoded.
//!
//! Bitrate and frame rate were the only two things fitted to the viewer's
//! link, which left the third and largest one fixed. A viewer whose connection
//! settles at half a megabit was being sent a full sized picture regardless:
//! 1080p at 500 kbit/s is roughly twelve thousandths of a bit per pixel, which
//! does not freeze and does not stutter, it simply arrives as mush.
//!
//! Halving each dimension quarters the pixel count, so the same bits buy four
//! times as many per pixel. The viewer's browser scales it back up to fill
//! their window, and a slightly soft picture at the right size beats a sharp
//! one nobody can read.
//!
//! The scaling is done by generating mipmaps.
//!
//! That choice is worth explaining, because a shader would be the obvious
//! answer. Mip generation needs no HLSL to compile, no bytecode to embed, no
//! vertex buffer, no viewport and no render pass, and it is done by the same
//! fixed-function hardware a shader would end up asking for. What it costs is
//! that only powers of two are available, which is exactly what
//! `bwe::SCALE_LADDER` offers anyway.
//!
//! The invariant from `capture` still holds: everything here happens on the
//! GPU. Nothing is mapped, nothing is read back, and the texture handed to
//! NVENC is one it can register directly. At a divisor of one this module is
//! bypassed entirely and the capture texture goes straight to the encoder, so
//! a fast link pays nothing at all for the existence of a slow one.

use windows::core::Result;
use windows::Win32::Graphics::Direct3D::D3D11_SRV_DIMENSION_TEXTURE2D;
use windows::Win32::Graphics::Direct3D11::{
    ID3D11Device, ID3D11DeviceContext, ID3D11ShaderResourceView, ID3D11Texture2D,
    D3D11_BIND_RENDER_TARGET, D3D11_BIND_SHADER_RESOURCE, D3D11_RESOURCE_MISC_GENERATE_MIPS,
    D3D11_SHADER_RESOURCE_VIEW_DESC, D3D11_SHADER_RESOURCE_VIEW_DESC_0, D3D11_TEX2D_SRV,
    D3D11_TEXTURE2D_DESC, D3D11_USAGE_DEFAULT,
};
use windows::Win32::Graphics::Dxgi::Common::{DXGI_FORMAT_B8G8R8A8_UNORM, DXGI_SAMPLE_DESC};

/// Downsamples capture textures by a power of two.
///
/// Holds onto its intermediates rather than making them per frame: at sixty
/// frames a second, allocating two textures and a view every frame would cost
/// more than the scaling.
pub struct Scaler {
    device: ID3D11Device,
    context: ID3D11DeviceContext,
    /// What the cached resources were built for. A window resize or a change
    /// of divisor rebuilds them, nothing else does.
    built_for: Option<(u32, u32, u32)>,
    /// Full size, with a mip chain. The source is copied into its top level
    /// and the hardware fills the rest.
    chain: Option<ID3D11Texture2D>,
    view: Option<ID3D11ShaderResourceView>,
    /// Single level, at the reduced size. This is what the encoder registers,
    /// and it is reused every frame so the registration cache keeps hitting.
    out: Option<ID3D11Texture2D>,
}

impl Scaler {
    pub fn new(device: &ID3D11Device, context: &ID3D11DeviceContext) -> Self {
        Self {
            device: device.clone(),
            context: context.clone(),
            built_for: None,
            chain: None,
            view: None,
            out: None,
        }
    }

    /// The size a source of `width` by `height` becomes at this divisor.
    ///
    /// Rounded up to even numbers, because H.264 encodes in macroblocks and
    /// an odd dimension is not something to hand a hardware encoder.
    pub fn target(width: u32, height: u32, divisor: u32) -> (u32, u32) {
        let even = |v: u32| (v / divisor).max(2) & !1;
        (even(width), even(height))
    }

    /// Returns `source` shrunk by `divisor`, or `source` itself at a divisor
    /// of one.
    ///
    /// The returned texture is owned by this scaler and is overwritten on the
    /// next call, which is exactly how the encoder wants it: the same texture
    /// arriving every frame is one registration rather than one per frame.
    pub fn shrink(
        &mut self,
        source: &ID3D11Texture2D,
        width: u32,
        height: u32,
        divisor: u32,
    ) -> Result<ID3D11Texture2D> {
        if divisor <= 1 {
            return Ok(source.clone());
        }

        self.ensure(width, height, divisor)?;

        let (Some(chain), Some(view), Some(out)) =
            (self.chain.as_ref(), self.view.as_ref(), self.out.as_ref())
        else {
            // Nothing was built, so there is nothing to shrink with. Sending
            // the picture at full size is worse than intended and better than
            // not sending one.
            return Ok(source.clone());
        };

        unsafe {
            // Into the top of the chain, then let the hardware fill the rest.
            // `CopySubresourceRegion` rather than `CopyResource`, because the
            // two textures differ in mip count and `CopyResource` insists the
            // descriptions match.
            self.context.CopySubresourceRegion(chain, 0, 0, 0, 0, source, 0, None);
            self.context.GenerateMips(view);

            // A divisor of two is mip one, four is mip two, and so on, which
            // is the whole reason the ladder is powers of two.
            let level = divisor.trailing_zeros();
            self.context.CopySubresourceRegion(out, 0, 0, 0, 0, chain, level, None);
        }

        Ok(out.clone())
    }

    fn ensure(&mut self, width: u32, height: u32, divisor: u32) -> Result<()> {
        if self.built_for == Some((width, height, divisor)) {
            return Ok(());
        }

        // Dropped before the new ones are made, so a long session that resizes
        // repeatedly does not hold several full sized textures at once.
        self.view = None;
        self.chain = None;
        self.out = None;
        self.built_for = None;

        let levels = divisor.trailing_zeros() + 1;
        let chain_desc = D3D11_TEXTURE2D_DESC {
            Width: width,
            Height: height,
            MipLevels: levels,
            ArraySize: 1,
            Format: DXGI_FORMAT_B8G8R8A8_UNORM,
            SampleDesc: DXGI_SAMPLE_DESC { Count: 1, Quality: 0 },
            Usage: D3D11_USAGE_DEFAULT,
            // Both binds and the misc flag are required together: mip
            // generation writes through a render target and reads through a
            // shader resource, and refuses without all three.
            BindFlags: (D3D11_BIND_SHADER_RESOURCE.0 | D3D11_BIND_RENDER_TARGET.0) as u32,
            CPUAccessFlags: 0,
            MiscFlags: D3D11_RESOURCE_MISC_GENERATE_MIPS.0 as u32,
        };

        let mut chain: Option<ID3D11Texture2D> = None;
        unsafe { self.device.CreateTexture2D(&chain_desc, None, Some(&mut chain))? };
        let chain = chain.expect("a successful CreateTexture2D returns a texture");

        let srv_desc = D3D11_SHADER_RESOURCE_VIEW_DESC {
            Format: DXGI_FORMAT_B8G8R8A8_UNORM,
            ViewDimension: D3D11_SRV_DIMENSION_TEXTURE2D,
            Anonymous: D3D11_SHADER_RESOURCE_VIEW_DESC_0 {
                Texture2D: D3D11_TEX2D_SRV { MostDetailedMip: 0, MipLevels: levels },
            },
        };
        let mut view: Option<ID3D11ShaderResourceView> = None;
        unsafe {
            self.device.CreateShaderResourceView(&chain, Some(&srv_desc), Some(&mut view))?;
        }

        let (out_w, out_h) = Self::target(width, height, divisor);
        let out_desc = D3D11_TEXTURE2D_DESC {
            Width: out_w,
            Height: out_h,
            MipLevels: 1,
            ArraySize: 1,
            Format: DXGI_FORMAT_B8G8R8A8_UNORM,
            SampleDesc: DXGI_SAMPLE_DESC { Count: 1, Quality: 0 },
            Usage: D3D11_USAGE_DEFAULT,
            BindFlags: D3D11_BIND_SHADER_RESOURCE.0 as u32,
            CPUAccessFlags: 0,
            MiscFlags: 0,
        };
        let mut out: Option<ID3D11Texture2D> = None;
        unsafe { self.device.CreateTexture2D(&out_desc, None, Some(&mut out))? };

        self.chain = Some(chain);
        self.view = view;
        self.out = out;
        self.built_for = Some((width, height, divisor));
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_divisor_of_one_changes_nothing() {
        assert_eq!(Scaler::target(1920, 1080, 1), (1920, 1080));
    }

    #[test]
    fn dimensions_stay_even() {
        // An odd dimension is not something to hand a hardware encoder, and
        // odd sources are ordinary: a window is whatever size it was dragged
        // to.
        for (w, h) in [(1921, 1081), (1607, 970), (999, 555), (1366, 769)] {
            for divisor in [1, 2, 4] {
                let (tw, th) = Scaler::target(w, h, divisor);
                assert_eq!(tw % 2, 0, "{w}x{h} / {divisor} gave an odd width {tw}");
                assert_eq!(th % 2, 0, "{w}x{h} / {divisor} gave an odd height {th}");
            }
        }
    }

    #[test]
    fn a_tiny_window_never_reaches_zero() {
        // A window smaller than the divisor would otherwise scale to nothing,
        // and a zero sized encoder is a failure rather than a small picture.
        for divisor in [2, 4] {
            let (w, h) = Scaler::target(3, 1, divisor);
            assert!(w >= 2 && h >= 2, "{w}x{h} at divisor {divisor}");
        }
    }

    #[test]
    fn halving_quarters_the_pixels() {
        // The point of the exercise, stated as a number.
        let (w, h) = Scaler::target(1920, 1080, 2);
        assert_eq!((w, h), (960, 540));
        assert_eq!((1920 * 1080) / (w * h), 4);
    }
}
