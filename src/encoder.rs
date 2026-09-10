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
    /// Whether SPS/PPS have been sent. See `strip_parameter_sets`.
    sent_parameter_sets: bool,
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

/// Removes SPS (7) and PPS (8) NAL units, keeping everything else.
///
/// NVENC repeats the parameter sets on every IDR, which is normally good
/// practice. It is not good practice here: `rtc-rtp`'s H.264 payloader bundles
/// SPS+PPS into a STAP-A and then falls through and emits the PPS a *second*
/// time as a standalone NAL. The first keyframe survives that because the
/// payloader's parameter-set state starts empty; every later one corrupts the
/// stream, and the viewer's decoder stops decoding anything and asks for
/// keyframes several times a second, which makes it worse, not better.
///
/// The receiver keeps the parameter sets it got from the first keyframe for
/// the life of the session, so later IDRs decode without them. Each viewer
/// gets its own session and therefore its own first keyframe.
fn strip_parameter_sets(au: &[u8]) -> Vec<u8> {
    let mut out = Vec::with_capacity(au.len());
    for (start, end) in split_nals(au) {
        // The NAL type is the low 5 bits of the byte after the start code.
        let header = au[start..end].iter().position(|&b| b == 1).map(|p| start + p + 1);
        let Some(h) = header else { continue };
        if h >= end {
            continue;
        }
        let nal_type = au[h] & 0x1f;
        if nal_type == 7 || nal_type == 8 {
            continue;
        }
        out.extend_from_slice(&au[start..end]);
    }
    out
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
                    NV_ENC_PRESET_P1_GUID,
                    NV_ENC_TUNING_INFO::NV_ENC_TUNING_INFO_ULTRA_LOW_LATENCY,
                    &mut preset,
                ),
                "get_encode_preset_config_ex",
            )?;

            let mut config = Box::new(preset.presetCfg);
            config.version = NV_ENC_CONFIG_VER;

            // No B-frames: they reorder output and add a frame of latency for
            // a compression win nobody watching a game will notice.
            config.frameIntervalP = 1;

            // No periodic IDR, and none on demand either. See
            // `stream::KEYFRAME_INTERVAL`: a forced IDR mid-stream does not
            // survive this pipeline, so recovery is done with intra refresh
            // instead, which is what the block below sets up.
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
                presetGUID: NV_ENC_PRESET_P1_GUID,
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
                sent_parameter_sets: false,
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

            let bytes = if self.sent_parameter_sets {
                strip_parameter_sets(raw)
            } else {
                self.sent_parameter_sets = true;
                raw.to_vec()
            };

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
    /// keyframe, a burst of exactly the size a congested link cannot absorb,
    /// sent at the moment congestion was detected, and would need new
    /// parameter sets that `strip_parameter_sets` deliberately withholds.
    ///
    /// Resolution is not among the levers here, and cannot be until the
    /// payloader bug that `strip_parameter_sets` works around is fixed: a new
    /// resolution needs a new SPS, and this stream sends parameter sets
    /// exactly once.
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

    /// What to do when the viewer says it cannot decode.
    ///
    /// Not an IDR. See `stream::KEYFRAME_INTERVAL` for the measurement: a
    /// forced IDR mid-stream took a 64 fps stream down to 9.7 and produced
    /// nearly four picture-loss requests a second, because the answer to
    /// "I cannot decode" was a burst too big for the link that had just
    /// dropped something. A refresh cycle repairs the same damage using
    /// ordinary frames.
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
    use super::{split_nals, strip_parameter_sets};

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
    fn strips_sps_and_pps_but_keeps_the_slice() {
        let mut au = nal(0x67, b"sps");
        au.extend(nal(0x68, b"pps"));
        au.extend(nal(0x65, b"idr-slice"));

        let out = strip_parameter_sets(&au);
        assert_eq!(out, nal(0x65, b"idr-slice"));
    }

    #[test]
    fn leaves_a_plain_p_frame_untouched() {
        let au = nal(0x41, b"p-frame");
        assert_eq!(strip_parameter_sets(&au), au);
    }

    #[test]
    fn keeps_sei_and_other_nal_types() {
        let mut au = nal(0x67, b"sps");
        au.extend(nal(0x06, b"sei"));
        au.extend(nal(0x65, b"idr"));

        let mut expected = nal(0x06, b"sei");
        expected.extend(nal(0x65, b"idr"));
        assert_eq!(strip_parameter_sets(&au), expected);
    }

    #[test]
    fn empty_input_is_handled() {
        assert!(strip_parameter_sets(&[]).is_empty());
        assert!(split_nals(&[]).is_empty());
    }
}
