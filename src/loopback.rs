//! Stage 3, capture audio from one process tree and nothing else.
//!
//! This is the whole premise of the project. `ActivateAudioInterfaceAsync`
//! against the process-loopback virtual device gives us a normal `IAudioClient`
//! that is scoped to a single process tree, so Discord and the microphone are
//! simply not present in the stream rather than being mixed in and subtracted.
//!
//! Two things about this API are not obvious from the docs:
//!
//!   * `GetMixFormat` does not work on the virtual device. You declare the
//!     format you want and the audio engine converts into it.
//!   * When the target process is silent it delivers *no packets at all*,
//!     rather than buffers of zeros. Anything downstream that assumes a
//!     continuous stream, an encoder, a muxer, a WebRTC track, will drift
//!     against the video within about a minute. The fix is to notice the gap
//!     against a wall clock and synthesise the missing silence, which is what
//!     `Capture::pump` does below.

use std::mem::ManuallyDrop;
use std::path::Path;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::Instant;

use windows::core::{implement, w, Interface, Ref, Result};
use windows::Win32::Foundation::HANDLE;
use windows::Win32::Media::Audio::{
    ActivateAudioInterfaceAsync, IActivateAudioInterfaceAsyncOperation,
    IActivateAudioInterfaceCompletionHandler, IActivateAudioInterfaceCompletionHandler_Impl,
    IAudioCaptureClient, IAudioClient, AUDCLNT_BUFFERFLAGS_SILENT, AUDCLNT_SHAREMODE_SHARED,
    AUDCLNT_STREAMFLAGS_EVENTCALLBACK, AUDCLNT_STREAMFLAGS_LOOPBACK,
    AUDIOCLIENT_ACTIVATION_PARAMS, AUDIOCLIENT_ACTIVATION_PARAMS_0,
    AUDIOCLIENT_ACTIVATION_TYPE_PROCESS_LOOPBACK, AUDIOCLIENT_PROCESS_LOOPBACK_PARAMS,
    PROCESS_LOOPBACK_MODE_INCLUDE_TARGET_PROCESS_TREE, WAVEFORMATEX, WAVE_FORMAT_PCM,
};
use windows::Win32::System::Com::StructuredStorage::{
    PROPVARIANT, PROPVARIANT_0, PROPVARIANT_0_0, PROPVARIANT_0_0_0,
};
use windows::Win32::System::Com::{CoInitializeEx, CoUninitialize, BLOB, COINIT_MULTITHREADED};
use windows::Win32::System::Threading::{CreateEventW, SetEvent, WaitForSingleObject};
use windows::Win32::System::Variant::VT_BLOB;

use crate::audio::{OpusPacket, OpusStream};
use crate::gain::Gain;
use crate::mic::{mix_into, Mic};
use crate::session::Session;
use crate::wav::WavWriter;

pub const SAMPLE_RATE: u32 = 48_000;
pub const CHANNELS: u16 = 2;
pub const BITS: u16 = 16;
pub const BLOCK_ALIGN: usize = (CHANNELS as usize) * (BITS as usize) / 8;

/// 20 ms, in 100-nanosecond units.
const BUFFER_DURATION_HNS: i64 = 200_000;

// ---------------------------------------------------------------------------
// Completion handler
// ---------------------------------------------------------------------------

#[implement(IActivateAudioInterfaceCompletionHandler)]
struct ActivateHandler {
    done: HANDLE,
}

impl IActivateAudioInterfaceCompletionHandler_Impl for ActivateHandler_Impl {
    fn ActivateCompleted(
        &self,
        _operation: Ref<'_, IActivateAudioInterfaceAsyncOperation>,
    ) -> Result<()> {
        unsafe {
            let _ = SetEvent(self.done);
        }
        Ok(())
    }
}

// ---------------------------------------------------------------------------
// Capture
// ---------------------------------------------------------------------------

pub struct Capture {
    client: IAudioClient,
    capture: IAudioCaptureClient,
    packet_event: HANDLE,
    started: Instant,
    frames_written: u64,
    gap_frames: u64,
    /// Loudest absolute sample seen since the last `take_peak`, measured after
    /// the lift and before the microphone is mixed in.
    peak: u32,
}

impl Capture {
    /// Opens a loopback stream over `pid` and everything it spawned.
    pub fn open(pid: u32) -> Result<Self> {
        unsafe {
            let mut params = AUDIOCLIENT_ACTIVATION_PARAMS {
                ActivationType: AUDIOCLIENT_ACTIVATION_TYPE_PROCESS_LOOPBACK,
                Anonymous: AUDIOCLIENT_ACTIVATION_PARAMS_0 {
                    ProcessLoopbackParams: AUDIOCLIENT_PROCESS_LOOPBACK_PARAMS {
                        TargetProcessId: pid,
                        // Tree, not bare PID: Discord and Chrome emit audio
                        // from child processes, and a bare match gets nothing.
                        ProcessLoopbackMode:
                            PROCESS_LOOPBACK_MODE_INCLUDE_TARGET_PROCESS_TREE,
                    },
                },
            };

            // The activation params travel as a VT_BLOB PROPVARIANT. There is
            // no safe constructor for that variant, so it is assembled from
            // the crate's own structs, the field offsets are then not our
            // assumption to get wrong.
            //
            // The outer ManuallyDrop is load-bearing. `windows` implements
            // Drop for PROPVARIANT as a call to PropVariantClear, which would
            // try to free pBlobData, a pointer to `params`, which lives on
            // our stack. Letting this value drop corrupts the heap and takes
            // the process down a moment after activation succeeds.
            let activation = ManuallyDrop::new(PROPVARIANT {
                Anonymous: PROPVARIANT_0 {
                    Anonymous: ManuallyDrop::new(PROPVARIANT_0_0 {
                        vt: VT_BLOB,
                        wReserved1: 0,
                        wReserved2: 0,
                        wReserved3: 0,
                        Anonymous: PROPVARIANT_0_0_0 {
                            blob: BLOB {
                                cbSize: std::mem::size_of::<AUDIOCLIENT_ACTIVATION_PARAMS>()
                                    as u32,
                                pBlobData: &mut params as *mut _ as *mut u8,
                            },
                        },
                    }),
                },
            });

            let activate_done = CreateEventW(None, false, false, None)?;
            let handler: IActivateAudioInterfaceCompletionHandler =
                ActivateHandler { done: activate_done }.into();

            let operation: IActivateAudioInterfaceAsyncOperation = ActivateAudioInterfaceAsync(
                w!("VAD\\Process_Loopback"),
                &IAudioClient::IID,
                Some(&*activation as *const PROPVARIANT),
                &handler,
            )?;

            WaitForSingleObject(activate_done, 5_000);

            let mut activate_hr = windows::core::HRESULT(0);
            let mut unknown: Option<windows::core::IUnknown> = None;
            operation.GetActivateResult(&mut activate_hr, &mut unknown)?;
            activate_hr.ok()?;

            let client: IAudioClient = unknown
                .ok_or_else(|| windows::core::Error::from_hresult(windows::Win32::Foundation::E_FAIL))?
                .cast()?;

            let format = WAVEFORMATEX {
                wFormatTag: WAVE_FORMAT_PCM as u16,
                nChannels: CHANNELS,
                nSamplesPerSec: SAMPLE_RATE,
                nAvgBytesPerSec: SAMPLE_RATE * BLOCK_ALIGN as u32,
                nBlockAlign: BLOCK_ALIGN as u16,
                wBitsPerSample: BITS,
                cbSize: 0,
            };

            client.Initialize(
                AUDCLNT_SHAREMODE_SHARED,
                AUDCLNT_STREAMFLAGS_LOOPBACK | AUDCLNT_STREAMFLAGS_EVENTCALLBACK,
                BUFFER_DURATION_HNS,
                0,
                &format,
                None,
            )?;

            let packet_event = CreateEventW(None, false, false, None)?;
            client.SetEventHandle(packet_event)?;

            let capture: IAudioCaptureClient = client.GetService()?;
            client.Start()?;

            Ok(Self {
                client,
                capture,
                packet_event,
                started: Instant::now(),
                frames_written: 0,
                gap_frames: 0,
                peak: 0,
            })
        }
    }

    /// Drains whatever is pending, first topping up any silence the process
    /// failed to produce. Returns bytes appended this call.
    fn pump(
        &mut self,
        mut wav: Option<&mut WavWriter>,
        opus: &mut OpusStream,
        mic: Option<&Mic>,
        mut gain: Option<&mut Gain>,
    ) -> std::result::Result<Vec<OpusPacket>, String> {
        unsafe {
            WaitForSingleObject(self.packet_event, 200);

            let mut packets = Vec::new();

            // Gap fill. Half a buffer of slack keeps normal jitter from being
            // mistaken for silence.
            let elapsed = self.started.elapsed().as_secs_f64();
            let expected = (elapsed * SAMPLE_RATE as f64) as u64;
            let slack = (SAMPLE_RATE as u64) / 100; // 10 ms
            if expected > self.frames_written + slack {
                // Capped so a long stall cannot ask for a single enormous
                // allocation; the next call simply fills the rest.
                let missing =
                    (expected - self.frames_written - slack).min(SAMPLE_RATE as u64);

                if let Some(w) = wav.as_deref_mut() {
                    let _ = w.write_silence(missing as usize * BLOCK_ALIGN);
                }

                // The synthesised silence goes to the encoder too, that is
                // the entire point of generating it, and the mic is mixed
                // into it, because a silent game with the mic live is exactly
                // when the viewer most needs to hear you.
                let quiet_samples = missing as usize * CHANNELS as usize;
                let mut quiet = vec![0i16; quiet_samples];
                if let Some(m) = mic {
                    if m.is_on() {
                        mix_into(&mut quiet, &m.take(quiet_samples));
                    }
                }
                packets.extend(opus.push(&quiet)?);

                self.frames_written += missing;
                self.gap_frames += missing;
            }

            loop {
                let packet = self.capture.GetNextPacketSize().map_err(|e| e.to_string())?;
                if packet == 0 {
                    break;
                }

                let mut data: *mut u8 = std::ptr::null_mut();
                let mut frames: u32 = 0;
                let mut flags: u32 = 0;

                self.capture
                    .GetBuffer(&mut data, &mut frames, &mut flags, None, None)
                    .map_err(|e| e.to_string())?;

                let bytes = frames as usize * BLOCK_ALIGN;

                let samples = frames as usize * CHANNELS as usize;
                let silent = flags & AUDCLNT_BUFFERFLAGS_SILENT.0 as u32 != 0 || data.is_null();

                let mut pcm: Vec<i16> = if silent {
                    vec![0i16; samples]
                } else {
                    std::slice::from_raw_parts(data as *const i16, samples).to_vec()
                };

                // Lifted here, before anything else touches the buffer.
                //
                // What the application rendered is not what its own listener
                // hears: the master volume, the mixer's per-application slider
                // and the headset's amplifier are all downstream of this and
                // none of them are in the capture. Sent as-is, a game that
                // sounds fine in the room arrives twenty decibels down.
                //
                // Only the application audio goes through it. The microphone
                // is mixed in below at its own level, because levelling the
                // two together would duck the voice every time the game got
                // loud.
                let lifted = match gain.as_deref_mut() {
                    Some(g) => g.apply(&mut pcm),
                    None => {
                        pcm.iter().map(|s| s.unsigned_abs() as f32).fold(0.0, f32::max) / 32767.0
                    }
                };

                // Measured after the lift, because the meter answers "will
                // they hear this", which is a question about what is being
                // sent. Talking into the mic must not make a silent game look
                // live, so this is still before the mix.
                self.peak = self.peak.max((lifted * i16::MAX as f32) as u32);

                if let Some(m) = mic {
                    if m.is_on() {
                        mix_into(&mut pcm, &m.take(samples));
                    }
                }

                if let Some(w) = wav.as_deref_mut() {
                    let _ = w.write(std::slice::from_raw_parts(
                        pcm.as_ptr() as *const u8,
                        pcm.len() * 2,
                    ));
                }
                packets.extend(opus.push(&pcm)?);
                let _ = bytes;

                self.capture.ReleaseBuffer(frames).map_err(|e| e.to_string())?;
                self.frames_written += frames as u64;
            }

            Ok(packets)
        }
    }

    /// Loudest sample captured since the last call, 0.0 to 1.0, and resets.
    ///
    /// Reset-on-read so the reading is "loudest in the last interval" rather
    /// than "loudest ever", which would latch on one notification chime and
    /// then claim everything was fine for the rest of the session.
    fn take_peak(&mut self) -> f32 {
        let peak = std::mem::take(&mut self.peak);
        peak as f32 / i16::MAX as f32
    }

    /// Fraction of the recording that had to be synthesised because the
    /// process was silent. Useful sanity check: a game that was making noise
    /// the whole time should be near zero.
    pub fn silence_ratio(&self) -> f64 {
        if self.frames_written == 0 {
            0.0
        } else {
            self.gap_frames as f64 / self.frames_written as f64
        }
    }
}

impl Drop for Capture {
    fn drop(&mut self) {
        unsafe {
            let _ = self.client.Stop();
        }
    }
}

/// Records `pid`'s audio to `path` until `stop` flips. Runs on its own thread,
/// so it owns its own COM apartment.
pub fn record_to_wav(
    pid: u32,
    path: &Path,
    stop: Arc<AtomicBool>,
) -> std::result::Result<CaptureReport, String> {
    unsafe {
        CoInitializeEx(None, COINIT_MULTITHREADED)
            .ok()
            .map_err(|e| format!("CoInitializeEx failed: {e}"))?;
    }

    let result = (|| {
        let mut cap = Capture::open(pid).map_err(|e| {
            format!(
                "could not open process loopback for pid {pid}: {e}\n  \
                 (if this is 0x80070005 the target is likely elevated - \
                 run this from an admin shell too)"
            )
        })?;

        let mut wav = WavWriter::create(path, SAMPLE_RATE, CHANNELS, BITS)
            .map_err(|e| format!("could not create {}: {e}", path.display()))?;

        // 128 kbit/s stereo, plenty for game audio, and small next to video.
        let mut opus = OpusStream::new(128_000)?;

        while !stop.load(Ordering::Relaxed) {
            cap.pump(Some(&mut wav), &mut opus, None, None)?;
        }

        let ratio = cap.silence_ratio();
        let bytes = wav.finish().map_err(|e| format!("could not finalise wav: {e}"))?;
        Ok(CaptureReport {
            wav_bytes: bytes,
            silence_ratio: ratio,
            opus_frames: opus.frames_emitted(),
            opus_bytes: opus.bytes_emitted(),
            opus_bitrate_bps: opus.average_bitrate_bps(),
        })
    })();

    unsafe { CoUninitialize() };
    result
}

/// What one capture run produced, on both the raw and encoded sides.
pub struct CaptureReport {
    pub wav_bytes: u32,
    pub silence_ratio: f64,
    pub opus_frames: u64,
    pub opus_bytes: u64,
    pub opus_bitrate_bps: f64,
}

/// Streaming variant: no file, Opus packets straight to a channel. Owns its
/// own COM apartment because it runs on a dedicated thread.
/// How long to wait before trying a process again after its loopback failed
/// to open. Long enough not to hammer the audio engine, short enough that an
/// application still starting up is picked up almost immediately.
const REOPEN_AFTER: std::time::Duration = std::time::Duration::from_millis(400);

/// How often the application's level is published to the session.
const PEAK_INTERVAL: std::time::Duration = std::time::Duration::from_millis(100);

/// Why the loopback would not open, in terms of what to do about it.
///
/// The overwhelmingly common cause is elevation: a process running as
/// administrator cannot be captured by a process that is not, and the raw
/// `E_ACCESSDENIED` says nothing a person can act on.
fn open_failure(session: &Session, e: &windows::core::Error) -> String {
    let (exe, _) = session.source();
    let what = if exe.is_empty() { "that application".to_owned() } else { exe };

    if e.code() == windows::Win32::Foundation::E_ACCESSDENIED {
        format!(
            "no audio from {what}: it is running as administrator.              Start Sideband as administrator too, or the viewer hears silence."
        )
    } else {
        format!("no audio from {what}: {e}")
    }
}

/// Silence covering a stretch of wall-clock time in which there was no capture
/// at all.
///
/// The Opus timeline advances by however many samples it is fed, so a gap left
/// unfilled is one the audio never makes up, it would sit permanently that far
/// behind the video for the rest of the session. Switching applications takes
/// long enough for that to matter.
fn silence_for(elapsed: std::time::Duration, mic: Option<&Mic>) -> Vec<i16> {
    let frames = (elapsed.as_secs_f64() * SAMPLE_RATE as f64) as usize;
    let samples = frames * CHANNELS as usize;
    let mut quiet = vec![0i16; samples];
    // The mic still belongs in it. A source being swapped is exactly when the
    // person watching most wants to hear what is going on.
    if let Some(m) = mic {
        if m.is_on() {
            mix_into(&mut quiet, &m.take(samples));
        }
    }
    quiet
}

/// Streams one process tree's audio, following whichever application the
/// session currently has selected.
///
/// The selection can change at any moment, and when it does the old capture is
/// closed *before* the new one is opened. That order is the point: playing one
/// application's sound over another's picture is the single failure this whole
/// program exists to avoid, so a switch that cannot be completed yields silence
/// rather than the wrong thing.
pub fn stream_opus(
    session: Arc<Session>,
    stop: Arc<AtomicBool>,
    tx: tokio::sync::mpsc::Sender<OpusPacket>,
    mic: Option<Arc<Mic>>,
) -> std::result::Result<(), String> {
    unsafe {
        CoInitializeEx(None, COINIT_MULTITHREADED)
            .ok()
            .map_err(|e| format!("CoInitializeEx failed: {e}"))?;
    }

    let result = (|| {
        let mut opus = OpusStream::new(128_000)?;
        // Owned out here rather than by the capture, so switching application
        // does not drop the amplifier back to unity and make the first second
        // of the new source inaudible while it works the level out again.
        let mut gain = Gain::default();
        let mut cap: Option<Capture> = None;
        let mut listening: u32 = 0;
        let mut retry_at = Instant::now();
        // Reported once per target rather than once per retry, which at
        // `REOPEN_AFTER` would be a fresh notice twice a second forever.
        let mut announced = false;
        let mut reported_at = Instant::now();

        while !stop.load(Ordering::Relaxed) {
            let wanted = session.selected_source();
            let mut packets = Vec::new();

            if wanted != listening || (cap.is_none() && Instant::now() >= retry_at) {
                let began = Instant::now();

                // Dropped first, unconditionally.
                cap = None;
                match Capture::open(wanted) {
                    Ok(opened) => {
                        cap = Some(opened);
                        announced = false;
                        session.set_app_audio_ok(true);
                    }
                    Err(e) => {
                        retry_at = began + REOPEN_AFTER;
                        session.set_app_audio_ok(false);
                        // A process that has a window but no audio endpoint
                        // yet is normal for the first moment after it
                        // launches, so the retry is silent. What is not
                        // acceptable is staying silent forever: an
                        // application whose audio cannot be opened at all
                        // streams perfectly paced silence, and without this
                        // the first anyone hears of it is the viewer saying
                        // they cannot hear anything.
                        if !announced && wanted != 0 {
                            session.note(open_failure(&session, &e));
                            announced = true;
                        }
                    }
                }
                listening = wanted;

                // Opening is not instant, and the media clock does not wait.
                packets.extend(opus.push(&silence_for(began.elapsed(), mic.as_deref()))?);
            }

            match cap.as_mut() {
                Some(c) => match c.pump(None, &mut opus, mic.as_deref(), Some(&mut gain)) {
                    Ok(got) => {
                        packets.extend(got);
                        // On a timer rather than every pass: `pump` returns
                        // about every 20 ms, and resetting the peak that often
                        // would report the level of a single buffer instead of
                        // the level of the moment.
                        if reported_at.elapsed() >= PEAK_INTERVAL {
                            session.set_app_peak(c.take_peak());
                            reported_at = Instant::now();
                        }
                    }
                    Err(_) => {
                        // A capture that was working can be invalidated out
                        // from under us: the default output device changing,
                        // headphones being plugged in, or the target process
                        // exiting all end the stream mid-session. Treating
                        // that as fatal costs the viewer audio for the rest
                        // of the call, and silently, because nothing reads
                        // this thread's result. So it is dropped and reopened
                        // on the same path a switch uses.
                        cap = None;
                        session.set_app_audio_ok(false);
                        retry_at = Instant::now() + REOPEN_AFTER;
                        // Not announced. Reopening normally succeeds within
                        // the second, and a notice for every headphone swap
                        // would be noise; `announced` is left as it is, so a
                        // target that then refuses to reopen still gets its
                        // one explanation.
                    }
                },
                None => {
                    // Nothing to listen to. Keep the timeline moving anyway,
                    // so that when there is, it still lines up with the video.
                    let began = Instant::now();
                    std::thread::sleep(std::time::Duration::from_millis(20));
                    packets.extend(opus.push(&silence_for(began.elapsed(), mic.as_deref()))?);
                }
            }

            for packet in packets {
                // Back-pressure rather than drop: a gap in the Opus stream is
                // exactly what the silence gap-filling exists to prevent.
                if tx.blocking_send(packet).is_err() {
                    return Ok(());
                }
            }
        }
        Ok(())
    })();

    unsafe { CoUninitialize() };
    result
}
