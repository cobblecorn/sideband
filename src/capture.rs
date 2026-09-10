//! Stage 2, capture a single window's pixels via Windows.Graphics.Capture.
//!
//! WGC hands back GPU textures, which is the whole point: in the real pipeline
//! the texture goes straight into NVENC and never touches the CPU. This module
//! deliberately breaks that rule at the very end, it copies one frame down to
//! a staging texture so it can be written to disk as proof the capture works.
//! Nothing downstream of stage 4 should ever do that.

use std::cell::Cell;
use std::path::Path;

use windows::core::{Interface, Result};
use windows::Foundation::TypedEventHandler;
use windows::Graphics::Capture::{
    Direct3D11CaptureFramePool, GraphicsCaptureItem, GraphicsCaptureSession,
};
use windows::Graphics::DirectX::DirectXPixelFormat;
use windows::Win32::Foundation::{HMODULE, HWND};
use windows::Win32::Graphics::Direct3D::D3D_DRIVER_TYPE_HARDWARE;
use windows::Win32::Graphics::Direct3D11::{
    D3D11CreateDevice, ID3D11Device, ID3D11DeviceContext, ID3D11Texture2D,
    D3D11_CPU_ACCESS_READ, D3D11_CREATE_DEVICE_BGRA_SUPPORT, D3D11_MAPPED_SUBRESOURCE,
    D3D11_MAP_READ, D3D11_SDK_VERSION, D3D11_TEXTURE2D_DESC, D3D11_USAGE_STAGING,
};
use windows::Win32::Graphics::Dxgi::IDXGIDevice;
use windows::Win32::System::WinRT::Direct3D11::{
    CreateDirect3D11DeviceFromDXGIDevice, IDirect3DDxgiInterfaceAccess,
};
use windows::Win32::System::WinRT::Graphics::Capture::IGraphicsCaptureItemInterop;

pub struct WindowCapture {
    device: ID3D11Device,
    context: ID3D11DeviceContext,
    pool: Direct3D11CaptureFramePool,
    session: GraphicsCaptureSession,
    item: GraphicsCaptureItem,
    /// Kept because the frame pool must be handed it again on every resize.
    winrt_device: windows::Graphics::DirectX::Direct3D11::IDirect3DDevice,
    /// The size the pool is currently built for. A window the user drags
    /// bigger keeps producing frames at the old size until the pool is
    /// recreated, which looks exactly like the stream being stuck.
    size: Cell<(i32, i32)>,
}

impl WindowCapture {
    pub fn start(hwnd: HWND) -> Result<Self> {
        if !GraphicsCaptureSession::IsSupported()? {
            return Err(windows::core::Error::new(
                windows::Win32::Foundation::E_FAIL,
                "Windows.Graphics.Capture is not available on this machine",
            ));
        }

        unsafe {
            let mut device: Option<ID3D11Device> = None;
            let mut context: Option<ID3D11DeviceContext> = None;
            D3D11CreateDevice(
                None,
                D3D_DRIVER_TYPE_HARDWARE,
                HMODULE::default(),
                D3D11_CREATE_DEVICE_BGRA_SUPPORT,
                None,
                D3D11_SDK_VERSION,
                Some(&mut device),
                None,
                Some(&mut context),
            )?;

            let device = device.unwrap();
            let context = context.unwrap();

            // WGC speaks WinRT, D3D11CreateDevice speaks Win32. This is the bridge.
            let dxgi: IDXGIDevice = device.cast()?;
            let winrt_device = CreateDirect3D11DeviceFromDXGIDevice(&dxgi)?;
            let winrt_device: windows::Graphics::DirectX::Direct3D11::IDirect3DDevice =
                winrt_device.cast()?;

            let interop: IGraphicsCaptureItemInterop =
                windows::core::factory::<GraphicsCaptureItem, IGraphicsCaptureItemInterop>()?;
            let item: GraphicsCaptureItem = interop.CreateForWindow(hwnd)?;

            let pool = Direct3D11CaptureFramePool::CreateFreeThreaded(
                &winrt_device,
                DirectXPixelFormat::B8G8R8A8UIntNormalized,
                2,
                item.Size()?,
            )?;

            let session = pool.CreateCaptureSession(&item)?;

            // Win11 draws a yellow "you are being captured" border by default,
            // and it would end up in the stream. Both of these are best-effort:
            // older builds simply do not have the setters.
            let _ = session.SetIsBorderRequired(false);
            let _ = session.SetIsCursorCaptureEnabled(false);

            session.StartCapture()?;

            let start = item.Size()?;

            Ok(Self {
                device,
                context,
                pool,
                session,
                item,
                winrt_device,
                size: Cell::new((start.Width, start.Height)),
            })
        }
    }

    pub fn title(&self) -> String {
        self.item
            .DisplayName()
            .map(|s| s.to_string_lossy())
            .unwrap_or_default()
    }

    /// Pulls whatever frame is ready, if any. Returns its dimensions.
    /// `save_to` writes that frame out as a BMP, for the test only.
    pub fn try_frame(&self, save_to: Option<&Path>) -> Result<Option<(u32, u32)>> {
        let Ok(frame) = self.pool.TryGetNextFrame() else {
            return Ok(None);
        };

        unsafe {
            let surface = frame.Surface()?;
            let access: IDirect3DDxgiInterfaceAccess = surface.cast()?;
            let texture: ID3D11Texture2D = access.GetInterface()?;

            let mut desc = D3D11_TEXTURE2D_DESC::default();
            texture.GetDesc(&mut desc);

            if let Some(path) = save_to {
                self.save_bmp(&texture, &desc, path)?;
            }

            Ok(Some((desc.Width, desc.Height)))
        }
    }

    /// The D3D11 device these frames live on. NVENC must open its session
    /// against the same device or the textures are not shareable.
    pub fn device(&self) -> &ID3D11Device {
        &self.device
    }

    /// The immediate context, for the scaler. Shared rather than made afresh
    /// because D3D11 immediate contexts are per device and not thread safe:
    /// there is exactly one, and the encode thread is the only one using it.
    pub fn context(&self) -> &ID3D11DeviceContext {
        &self.context
    }

    /// The next frame as a raw GPU texture plus its dimensions. This is the
    /// path the real pipeline uses, nothing is copied to the CPU.
    pub fn next_texture(&self) -> Result<Option<(ID3D11Texture2D, u32, u32)>> {
        let Ok(frame) = self.pool.TryGetNextFrame() else {
            return Ok(None);
        };

        // A resized window keeps delivering old-size frames until the pool is
        // rebuilt for the new size. This frame is still the old size; the next
        // one will not be, and the caller rebuilds its encoder when the
        // dimensions it is handed change.
        let content = frame.ContentSize()?;
        if (content.Width, content.Height) != self.size.get() && content.Width > 0 {
            self.pool.Recreate(
                &self.winrt_device,
                DirectXPixelFormat::B8G8R8A8UIntNormalized,
                2,
                content,
            )?;
            self.size.set((content.Width, content.Height));
        }

        let surface = frame.Surface()?;
        let access: IDirect3DDxgiInterfaceAccess = surface.cast()?;
        let texture: ID3D11Texture2D = unsafe { access.GetInterface()? };

        let mut desc = D3D11_TEXTURE2D_DESC::default();
        unsafe { texture.GetDesc(&mut desc) };

        Ok(Some((texture, desc.Width, desc.Height)))
    }

    /// Copies a GPU texture down to the CPU and writes a 32-bit BMP.
    /// This readback is exactly what stage 4 must never do.
    unsafe fn save_bmp(
        &self,
        texture: &ID3D11Texture2D,
        desc: &D3D11_TEXTURE2D_DESC,
        path: &Path,
    ) -> Result<()> {
        let staging_desc = D3D11_TEXTURE2D_DESC {
            Usage: D3D11_USAGE_STAGING,
            BindFlags: 0,
            CPUAccessFlags: D3D11_CPU_ACCESS_READ.0 as u32,
            MiscFlags: 0,
            ..*desc
        };

        let mut staging: Option<ID3D11Texture2D> = None;
        unsafe { self.device.CreateTexture2D(&staging_desc, None, Some(&mut staging))? };
        let staging = staging.unwrap();

        unsafe {
            self.context.CopyResource(&staging, texture);

            let mut mapped = D3D11_MAPPED_SUBRESOURCE::default();
            self.context.Map(&staging, 0, D3D11_MAP_READ, 0, Some(&mut mapped))?;

            let w = desc.Width as usize;
            let h = desc.Height as usize;
            let mut pixels = vec![0u8; w * h * 4];

            // BMP rows run bottom-up; WGC gives us top-down. Flip while
            // copying, and honour RowPitch, it is usually wider than w*4.
            for y in 0..h {
                let src = (mapped.pData as *const u8).add(y * mapped.RowPitch as usize);
                let dst_row = h - 1 - y;
                std::ptr::copy_nonoverlapping(
                    src,
                    pixels.as_mut_ptr().add(dst_row * w * 4),
                    w * 4,
                );
            }

            self.context.Unmap(&staging, 0);

            write_bmp(path, w as i32, h as i32, &pixels)
                .map_err(|e| windows::core::Error::new(
                    windows::Win32::Foundation::E_FAIL,
                    format!("could not write {}: {e}", path.display()),
                ))?;
        }

        Ok(())
    }
}

impl Drop for WindowCapture {
    fn drop(&mut self) {
        let _ = self.session.Close();
        let _ = self.pool.Close();
    }
}

fn write_bmp(path: &Path, width: i32, height: i32, bgra: &[u8]) -> std::io::Result<()> {
    use std::io::Write;

    let pixel_bytes = (width as usize) * (height as usize) * 4;
    let file_size = 54 + pixel_bytes;
    let mut f = std::io::BufWriter::new(std::fs::File::create(path)?);

    f.write_all(b"BM")?;
    f.write_all(&(file_size as u32).to_le_bytes())?;
    f.write_all(&0u16.to_le_bytes())?;
    f.write_all(&0u16.to_le_bytes())?;
    f.write_all(&54u32.to_le_bytes())?;

    f.write_all(&40u32.to_le_bytes())?; // BITMAPINFOHEADER
    f.write_all(&width.to_le_bytes())?;
    f.write_all(&height.to_le_bytes())?;
    f.write_all(&1u16.to_le_bytes())?; // planes
    f.write_all(&32u16.to_le_bytes())?; // bpp
    f.write_all(&0u32.to_le_bytes())?; // BI_RGB
    f.write_all(&(pixel_bytes as u32).to_le_bytes())?;
    f.write_all(&0i32.to_le_bytes())?;
    f.write_all(&0i32.to_le_bytes())?;
    f.write_all(&0u32.to_le_bytes())?;
    f.write_all(&0u32.to_le_bytes())?;

    f.write_all(bgra)?;
    f.flush()
}

/// Keeps the type in use until the FrameArrived path replaces polling.
#[allow(dead_code)]
type FrameHandler = TypedEventHandler<Direct3D11CaptureFramePool, windows::core::IInspectable>;
