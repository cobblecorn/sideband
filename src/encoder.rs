//! Stage 4, part two, NVENC, fed D3D11 textures directly.
//!
//! The crate we lean on here (`moq-nvenc`) is a fork of the mainline SDK
//! bindings whose one relevant change is that it loads `nvEncodeAPI64.dll` at
//! runtime instead of linking against it. That matters: the DLL ships with the
//! NVIDIA driver, so this builds on a machine with neither the CUDA toolkit
//! nor the (login-gated) Video Codec SDK installed. Its *safe* wrapper hardwires
//! CUDA as the device type, so the session is opened against `sys` by hand.
//!
//! The zero-copy claim is real and worth protecting: WGC hands us a BGRA
//! texture, and NVENC's `ARGB` buffer format is byte-order B,G,R,A, the same
//! layout. No conversion pass, no shader, no readback. If anyone ever adds a
//! CPU copy between capture and here, the latency budget is gone.

use std::collections::HashMap;
use std::ffi::c_void;
use std::ptr;

use moq_nvenc::sys::nvEncodeAPI::*;
use moq_nvenc::ENCODE_API;
use windows::core::Interface;
use windows::Win32::Graphics::Direct3D11::{ID3D11Device, ID3D11Texture2D};

use crate::pipeline::{Paced, VideoEncoder};

fn check(status: NVENCSTATUS, what: &str) -> Result<(), String> {
    if status == NVENCSTATUS::NV_ENC_SUCCESS {
        Ok(())
    } else {
        Err(format!("NVENC {what} failed: {status:?}"))
    }
}

pub struct NvencEncoder {
    encoder: *mut c_void,
    bitstream: *mut c_void,
    width: u32,
    height: u32,
    /// The encode configuration, kept alive and at a stable address because
    /// `init.encodeConfig` points into it and `reconfigure` resubmits both.
    /// A stack copy would leave that pointer dangling the moment `new`
    /// returned; nothing notices until the first rate change.
    config: Box<NV_ENC_CONFIG>,
    init: Box<NV_ENC_INITIALIZE_PARAMS>,
    /// What the encoder is actually running at, and what was last asked for.
    /// They differ when a driver refuses a rate change, which is worth being
    /// able to see rather than assuming it took.
    applied: (u32, u32),
    attempted: (u32, u32),
    /// Registration is expensive and the WGC frame pool recycles a small set
    /// of textures, so registrations are cached by texture pointer rather than
    /// redone every frame.
    registered: HashMap<usize, *mut c_void>,
    pending_idr: bool,
    /// A refresh cycle to begin on the next frame, because the viewer asked
    /// for a picture it could decode.
    pending_refresh: bool,
    frames: u64,
}

/// How often a full intra refresh cycle begins, in frames.
///
/// About two and a half seconds at 60 fps. This is the worst case for how long
/// a broken decoder stays broken, so it is the number that decides whether a
/// viewer who loses a burst of packets waits a moment or waits for ever.
const REFRESH_PERIOD: u32 = 150;

/// How many frames one cycle is spread across.
///
/// The whole point. A keyframe puts an entire picture's worth of intra coded
/// data in one frame, which on a constrained link is a burst the link cannot
/// absorb, so the keyframe itself is what gets lost, and the viewer asks for
/// another one, and the stream spends its bandwidth on keyframes nobody
/// receives. Spreading the same refresh over a quarter of a second turns that
/// burst into a barely visible rise in the ordinary frame size.
const REFRESH_FRAMES: u32 = 15;

/// Splits an Annex-B bitstream into NAL units, without their start codes.
fn split_nals(au: &[u8]) -> Vec<(usize, usize)> {
    let mut starts = Vec::new();
    let mut i = 0;
    while i + 3 <= au.len() {
        if au[i] == 0 && au[i + 1] == 0 {
            if au[i + 2] == 1 {
                starts.push((i, 3));
                i += 3;
                continue;
            }
            if i + 4 <= au.len() && au[i + 2] == 0 && au[i + 3] == 1 {
                starts.push((i, 4));
                i += 4;
                continue;
            }
        }
        i += 1;
    }

    let mut out = Vec::with_capacity(starts.len());
    for (n, &(pos, len)) in starts.iter().enumerate() {
        let body = pos + len;
        let end = starts.get(n + 1).map(|&(p, _)| p).unwrap_or(au.len());
        if body < end {
            out.push((pos, end));
        }
    }
    out
}

/// Whether an access unit carries a keyframe (an IDR picture, NAL type 5).
///
/// Worth counting rather than assuming. Keyframes are now sent in answer to a
/// viewer that cannot decode, at most one a second, and "how many went out,
/// and when" is the first question when somebody says their picture froze.
pub fn is_keyframe(au: &[u8]) -> bool {
    split_nals(au).into_iter().any(|(start, end)| {
        // The NAL type is the low five bits of the byte after the start code.
        let header = au[start..end].iter().position(|&b| b == 1).map(|p| start + p + 1);
        header.is_some_and(|h| h < end && au[h] & 0x1f == 5)
    })
}

impl NvencEncoder {
    pub fn new(
        device: &ID3D11Device,
        width: u32,
        height: u32,
        fps: u32,
        bitrate_bps: u32,
    ) -> Result<Self, String> {
        unsafe {
            let mut encoder: *mut c_void = ptr::null_mut();
            let mut session = NV_ENC_OPEN_ENCODE_SESSION_EX_PARAMS {
                version: NV_ENC_OPEN_ENCODE_SESSION_EX_PARAMS_VER,
                deviceType: NV_ENC_DEVICE_TYPE::NV_ENC_DEVICE_TYPE_DIRECTX,
                apiVersion: NVENCAPI_VERSION,
                device: device.as_raw(),
                ..Default::default()
            };
            check(
                (ENCODE_API.open_encode_session_ex)(&mut session, &mut encoder),
                "open_encode_session_ex",
            )?;

            // Start from the low-latency preset and adjust, rather than
            // filling NV_ENC_CONFIG from scratch, the struct is large and
            // most of it is not ours to have an opinion about.
            let mut preset = NV_ENC_PRESET_CONFIG {
                version: NV_ENC_PRESET_CONFIG_VER,
                presetCfg: NV_ENC_CONFIG { version: NV_ENC_CONFIG_VER, ..Default::default() },
                ..Default::default()
            };
            check(
                (ENCODE_API.get_encode_preset_config_ex)(
                    encoder,
                    NV_ENC_CODEC_H264_GUID,
                    NV_ENC_PRESET_P4_GUID,
                    NV_ENC_TUNING_INFO::NV_ENC_TUNING_INFO_ULTRA_LOW_LATENCY,
                    &mut preset,
                ),
                "get_encode_preset_config_ex",
            )?;

            let mut config = Box::new(preset.presetCfg);
            config.version = NV_ENC_CONFIG_VER;

            // Say which profile, rather than leaving the driver to pick one.
            //
            // The offer this stream is advertised with names a profile, and
            // what is actually encoded has to be that profile: a viewer whose
            // decoder trusts the offer and gets something else is entitled to
            // refuse it, and phones do. High is what the offer says, see
            // `net::H264_FMTP`, and it is also the one worth having: CABAC
            // and 8x8 transforms are most of the picture quality per bit that
            // separates a soft stream from a sharp one at the same rate.
            config.profileGUID = NV_ENC_H264_PROFILE_HIGH_GUID;

            // No B-frames: they reorder output and add a frame of latency for
            // a compression win nobody watching a game will notice.
            config.frameIntervalP = 1;

            // No keyframes on a schedule. They are sent when a viewer asks
            // for one, at most one a second, see `stream::KEYFRAME_GAP`, and
            // a keyframe nobody needed costs twenty to fifty ordinary frames.
            config.gopLength = NVENC_INFINITE_GOPLENGTH;

            // Rolling intra refresh, the reason a viewer can recover at all.
            //
            // Without it a decoder broken by loss stays broken for the rest of
            // the session: NACK repairs what it can retransmit in time and
            // nothing repairs the rest. With it, a band of intra coded
            // macroblocks sweeps the picture every couple of seconds, so any
            // decoder in any state converges on a correct picture within one
            // cycle without a keyframe ever being sent.
            //
            // It costs a few percent of bitrate and, on a badly broken
            // picture, shows as a band sweeping across once. Both are trades
            // worth making against a stream that never comes back.
            let h264 = &mut config.encodeCodecConfig.h264Config;
            h264.set_enableIntraRefresh(1);
            h264.intraRefreshPeriod = REFRESH_PERIOD;
            h264.intraRefreshCnt = REFRESH_FRAMES;

            config.rcParams.rateControlMode = NV_ENC_PARAMS_RC_MODE::NV_ENC_PARAMS_RC_CBR;
            config.rcParams.averageBitRate = bitrate_bps;
            config.rcParams.maxBitRate = bitrate_bps;

            // VBV sizing is a direct latency-vs-recovery trade. A one-frame
            // buffer is the lowest-latency choice, but it leaves no budget for
            // a keyframe: a forced 1080p IDR needs many times a normal frame's
            // bits, and squeezing it into one frame's allowance starves the
            // frames that follow until the next IDR, which looks exactly like
            // a decoder that cannot recover.
            //
            // A quarter-second buffer still keeps the encoder from banking
            // bits in any way a viewer would notice, and leaves room for the
            // periodic IDR to be a real keyframe.
            let vbv = bitrate_bps / 4;
            config.rcParams.vbvBufferSize = vbv;
            config.rcParams.vbvInitialDelay = vbv;

            let mut init = Box::new(NV_ENC_INITIALIZE_PARAMS {
                version: NV_ENC_INITIALIZE_PARAMS_VER,
                encodeGUID: NV_ENC_CODEC_H264_GUID,
                presetGUID: NV_ENC_PRESET_P4_GUID,
                encodeWidth: width,
                encodeHeight: height,
                darWidth: width,
                darHeight: height,
                frameRateNum: fps,
                frameRateDen: 1,
                enablePTD: 1,
                tuningInfo: NV_ENC_TUNING_INFO::NV_ENC_TUNING_INFO_ULTRA_LOW_LATENCY,
                encodeConfig: &mut *config,
                ..Default::default()
            });
            check(
                (ENCODE_API.initialize_encoder)(encoder, &mut *init),
                "initialize_encoder",
            )?;

            let mut create_bs = NV_ENC_CREATE_BITSTREAM_BUFFER {
                version: NV_ENC_CREATE_BITSTREAM_BUFFER_VER,
                ..Default::default()
            };
            check(
                (ENCODE_API.create_bitstream_buffer)(encoder, &mut create_bs),
                "create_bitstream_buffer",
            )?;

            Ok(Self {
                encoder,
                bitstream: create_bs.bitstreamBuffer,
                width,
                height,
                config,
                init,
                applied: (bitrate_bps, fps),
                attempted: (bitrate_bps, fps),
                registered: HashMap::new(),
                // The first frame must be an IDR or the viewer has nothing to
                // decode against.
                pending_idr: true,
                pending_refresh: false,
                frames: 0,
            })
        }
    }

    unsafe fn register(&mut self, texture: &ID3D11Texture2D) -> Result<*mut c_void, String> {
        let key = texture.as_raw() as usize;
        if let Some(existing) = self.registered.get(&key) {
            return Ok(*existing);
        }

        let mut reg = NV_ENC_REGISTER_RESOURCE {
            version: NV_ENC_REGISTER_RESOURCE_VER,
            resourceType: NV_ENC_INPUT_RESOURCE_TYPE::NV_ENC_INPUT_RESOURCE_TYPE_DIRECTX,
            width: self.width,
            height: self.height,
            pitch: 0,
            resourceToRegister: texture.as_raw(),
            bufferFormat: NV_ENC_BUFFER_FORMAT::NV_ENC_BUFFER_FORMAT_ARGB,
            bufferUsage: NV_ENC_BUFFER_USAGE::NV_ENC_INPUT_IMAGE,
            ..Default::default()
        };
        check(
            unsafe { (ENCODE_API.register_resource)(self.encoder, &mut reg) },
            "register_resource",
        )?;

        self.registered.insert(key, reg.registeredResource);
        Ok(reg.registeredResource)
    }

    /// Encodes one frame. Returns the H.264 access unit, or `None` when the
    /// encoder wants more input before it will emit anything.
    pub fn encode(
        &mut self,
        texture: &ID3D11Texture2D,
        timestamp_us: u64,
    ) -> Result<Option<Vec<u8>>, String> {
        unsafe {
            let registered = self.register(texture)?;

            let mut map = NV_ENC_MAP_INPUT_RESOURCE {
                version: NV_ENC_MAP_INPUT_RESOURCE_VER,
                registeredResource: registered,
                ..Default::default()
            };
            check(
                (ENCODE_API.map_input_resource)(self.encoder, &mut map),
                "map_input_resource",
            )?;

            let force_idr = std::mem::take(&mut self.pending_idr);
            let refresh_now = std::mem::take(&mut self.pending_refresh);

            let mut pic = NV_ENC_PIC_PARAMS {
                version: NV_ENC_PIC_PARAMS_VER,
                inputWidth: self.width,
                inputHeight: self.height,
                inputPitch: self.width,
                inputBuffer: map.mappedResource,
                outputBitstream: self.bitstream,
                bufferFmt: map.mappedBufferFmt,
                pictureStruct: NV_ENC_PIC_STRUCT::NV_ENC_PIC_STRUCT_FRAME,
                inputTimeStamp: timestamp_us,
                encodePicFlags: if force_idr {
                    NV_ENC_PIC_FLAGS::NV_ENC_PIC_FLAG_FORCEIDR as u32
                } else {
                    0
                },
                ..Default::default()
            };

            // A viewer asking for a keyframe gets a refresh cycle started now
            // rather than a keyframe. It repairs the same damage, and unlike a
            // forced IDR it is a request this pipeline can actually carry: the
            // measured failure was a viewer asking several times a second and
            // each answer being too large to arrive.
            if refresh_now && !force_idr {
                pic.codecPicParams.h264PicParams.forceIntraRefreshWithFrameCnt = REFRESH_FRAMES;
            }

            let status = (ENCODE_API.encode_picture)(self.encoder, &mut pic);

            // Not an error: the encoder is buffering and will emit later.
            if status == NVENCSTATUS::NV_ENC_ERR_NEED_MORE_INPUT {
                let _ = (ENCODE_API.unmap_input_resource)(self.encoder, map.mappedResource);
                return Ok(None);
            }
            if let Err(e) = check(status, "encode_picture") {
                let _ = (ENCODE_API.unmap_input_resource)(self.encoder, map.mappedResource);
                return Err(e);
            }

            let mut lock = NV_ENC_LOCK_BITSTREAM {
                version: NV_ENC_LOCK_BITSTREAM_VER,
                outputBitstream: self.bitstream,
                ..Default::default()
            };
            check(
                (ENCODE_API.lock_bitstream)(self.encoder, &mut lock),
                "lock_bitstream",
            )?;

            let raw = std::slice::from_raw_parts(
                lock.bitstreamBufferPtr as *const u8,
                lock.bitstreamSizeInBytes as usize,
            );

            // Everything the encoder produced, parameter sets included.
            //
            // They used to be stripped from every access unit after the
            // first, to work around a payloader that was said to emit the PPS
            // twice. It does not: it holds SPS and PPS back and emits them
            // once, together, in front of the next picture, and it does that
            // for every keyframe. Withholding them meant a keyframe could
            // never carry a new resolution, and a viewer that lost the
            // originals could never be given them again.
            let bytes = raw.to_vec();

            check(
                (ENCODE_API.unlock_bitstream)(self.encoder, self.bitstream),
                "unlock_bitstream",
            )?;
            check(
                (ENCODE_API.unmap_input_resource)(self.encoder, map.mappedResource),
                "unmap_input_resource",
            )?;

            self.frames += 1;
            Ok(Some(bytes))
        }
    }

    /// Retunes the running session to a new bitrate and frame rate.
    ///
    /// The encoder keeps going: no IDR is forced and no state is reset, which
    /// is the whole point. Rebuilding the session instead would emit a fresh
    /// keyframe at the moment congestion was detected, which is the worst
    /// moment for one.
    ///
    /// Resolution is not among the levers here. A new resolution needs a new
    /// SPS, which means a keyframe, and changing the size of the picture
    /// under a viewer is a thing this deliberately does not do: see the note
    /// on picture size in the README.
    ///
    /// A driver that refuses the change leaves the encoder running at its
    /// previous settings, which `bitrate` and `fps` continue to report
    /// honestly.
    pub fn reconfigure(&mut self, bitrate_bps: u32, fps: u32) -> Result<(), String> {
        if (bitrate_bps, fps) == self.attempted {
            return Ok(());
        }
        self.attempted = (bitrate_bps, fps);

        self.config.rcParams.averageBitRate = bitrate_bps;
        self.config.rcParams.maxBitRate = bitrate_bps;

        // The VBV window is a fraction of the bitrate, so it has to move with
        // it. Leaving a 10 Mbit/s buffer in place under a 1 Mbit/s target
        // would let the encoder bank two and a half seconds of bits and spend
        // them in one burst, which is precisely the behaviour a link in
        // trouble cannot take.
        let vbv = bitrate_bps / 4;
        self.config.rcParams.vbvBufferSize = vbv;
        self.config.rcParams.vbvInitialDelay = vbv;

        self.init.frameRateNum = fps;
        self.init.frameRateDen = 1;

        debug_assert_eq!(
            self.init.encodeConfig,
            std::ptr::from_mut::<NV_ENC_CONFIG>(&mut *self.config),
            "init.encodeConfig must still point at our own config"
        );

        let mut params = NV_ENC_RECONFIGURE_PARAMS {
            version: NV_ENC_RECONFIGURE_PARAMS_VER,
            reInitEncodeParams: *self.init,
            ..Default::default()
        };
        // Retune in place: no reset, no keyframe.
        params.set_resetEncoder(0);
        params.set_forceIDR(0);

        check(
            unsafe { (ENCODE_API.reconfigure_encoder)(self.encoder, &mut params) },
            "reconfigure_encoder",
        )?;

        self.applied = (bitrate_bps, fps);
        Ok(())
    }

    /// The bitrate the encoder is running at, in bits per second.
    /// Sends a real keyframe on the next frame.
    ///
    /// What a viewer asking for a picture is answered with, subject to the
    /// once-a-second limit in `stream`. Nothing else repairs a receiver that
    /// has given up: every browser holds decoding until a keyframe arrives,
    /// whatever else is sent in the meantime.
    pub fn force_idr(&mut self) {
        self.pending_idr = true;
    }

    pub fn bitrate(&self) -> u32 {
        self.applied.0
    }

    /// The frame rate the encoder's rate control is budgeting for.
    pub fn fps(&self) -> u32 {
        self.applied.1
    }

    pub fn frames_encoded(&self) -> u64 {
        self.frames
    }
}

impl VideoEncoder for NvencEncoder {
    type Frame = ID3D11Texture2D;

    fn submit(&mut self, frame: &Paced<Self::Frame>) -> Result<(), String> {
        self.encode(&frame.frame, frame.timestamp_us).map(|_| ())
    }

    /// Starts a refresh cycle: the cheap answer, for when a real keyframe
    /// has just been sent and another would only add to the load.
    ///
    /// A band of intra coded macroblocks sweeps the picture over the next
    /// few frames, which repairs a damaged picture without a keyframe's cost.
    /// It does not, on its own, restart a receiver that has stopped decoding,
    /// which is what `force_idr` is for.
    fn request_keyframe(&mut self) {
        self.pending_refresh = true;
    }


}

impl Drop for NvencEncoder {
    fn drop(&mut self) {
        unsafe {
            for (_, resource) in self.registered.drain() {
                let _ = (ENCODE_API.unregister_resource)(self.encoder, resource);
            }
            if !self.bitstream.is_null() {
                let _ = (ENCODE_API.destroy_bitstream_buffer)(self.encoder, self.bitstream);
            }
            if !self.encoder.is_null() {
                let _ = (ENCODE_API.destroy_encoder)(self.encoder);
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::{is_keyframe, split_nals};

    fn nal(kind: u8, body: &[u8]) -> Vec<u8> {
        let mut v = vec![0, 0, 0, 1, kind];
        v.extend_from_slice(body);
        v
    }

    #[test]
    fn splits_four_byte_start_codes() {
        let mut au = nal(7, b"sps");
        au.extend(nal(8, b"pps"));
        au.extend(nal(5, b"idr"));
        assert_eq!(split_nals(&au).len(), 3);
    }

    #[test]
    fn splits_three_byte_start_codes() {
        let au = [0, 0, 1, 0x67, 9, 9, 0, 0, 1, 0x68, 8, 0, 0, 1, 0x65, 1, 2, 3];
        assert_eq!(split_nals(&au).len(), 3);
    }

    #[test]
    fn a_keyframe_is_recognised_behind_its_parameter_sets() {
        // What NVENC actually emits for a keyframe: SPS, PPS, then the
        // picture itself. Both of the first two must not be mistaken for one.
        let mut au = nal(0x67, b"sps");
        au.extend(nal(0x68, b"pps"));
        au.extend(nal(0x65, b"idr-slice"));
        assert!(is_keyframe(&au));
    }

    #[test]
    fn an_ordinary_frame_is_not_a_keyframe() {
        assert!(!is_keyframe(&nal(0x41, b"p-frame")));
        let mut parameters_only = nal(0x67, b"sps");
        parameters_only.extend(nal(0x68, b"pps"));
        assert!(!is_keyframe(&parameters_only));
    }

    #[test]
    fn empty_input_is_handled() {
        assert!(!is_keyframe(&[]));
        assert!(split_nals(&[]).is_empty());
    }
}
