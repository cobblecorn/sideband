//! Microphone passthrough.
//!
//! The whole point of this project is that the stream carries the game and
//! nothing else. The mic is the one exception you actually want on purpose,
//! so it is off by default and toggled with a global hotkey, you are in a
//! game when you decide to use it, and alt-tabbing to click something defeats
//! the purpose.
//!
//! Mixing happens in `loopback::Capture::pump`, alongside the synthesised
//! silence: if the game is quiet and the mic is live, the viewer must still
//! hear you, so the mic is mixed into gap-filled silence too.

use std::collections::VecDeque;
use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};
use std::sync::{Arc, Mutex};

use windows::Win32::Foundation::HANDLE;
use windows::Win32::Devices::FunctionDiscovery::PKEY_Device_FriendlyName;
use windows::Win32::Media::Audio::{
    eCapture, eConsole, IAudioCaptureClient, IAudioClient, IMMDevice, IMMDeviceEnumerator,
    MMDeviceEnumerator, AUDCLNT_BUFFERFLAGS_SILENT, AUDCLNT_SHAREMODE_SHARED,
    AUDCLNT_STREAMFLAGS_AUTOCONVERTPCM, AUDCLNT_STREAMFLAGS_EVENTCALLBACK,
    AUDCLNT_STREAMFLAGS_SRC_DEFAULT_QUALITY, DEVICE_STATE_ACTIVE, WAVEFORMATEX, WAVE_FORMAT_PCM,
};
use windows::Win32::System::Com::{
    CoCreateInstance, CoInitializeEx, CoTaskMemFree, CoUninitialize, CLSCTX_ALL,
    COINIT_MULTITHREADED, STGM_READ,
};
use windows::Win32::System::Threading::{CreateEventW, WaitForSingleObject};

use crate::loopback::{BLOCK_ALIGN, CHANNELS, SAMPLE_RATE};

/// Cap on buffered mic audio. Anything older than this is stale by the time it
/// would be mixed, and letting the queue grow unbounded would turn a slow
/// consumer into ever-increasing mic latency.
const MAX_BUFFERED_MS: usize = 200;

/// One capture device the machine will let us open.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Device {
    /// The endpoint id. Stable across reboots and across the device being
    /// unplugged and put back, which is why this rather than the name is what
    /// gets remembered.
    pub id: String,
    /// What the person sees in Sound settings.
    pub name: String,
    /// Whether Windows would pick this one on its own.
    pub default: bool,
}

/// Every active capture endpoint, with the default marked.
///
/// Worth offering rather than always taking the default, because on a machine
/// with a headset, a webcam and any of the vendor mixer suites, the default is
/// routinely a virtual device that captures nothing at all. There is no way to
/// tell that apart from a muted microphone by listening, and no error is
/// raised: the device opens perfectly and delivers silence.
pub fn devices() -> Vec<Device> {
    // Its own apartment, because this is called from the window thread rather
    // than from the capture thread that has one already.
    unsafe {
        if CoInitializeEx(None, COINIT_MULTITHREADED).is_err() {
            // Already initialised on this thread with another model, which is
            // fine: the calls below work either way.
        }
    }

    let found = unsafe { enumerate() }.unwrap_or_default();
    unsafe { CoUninitialize() };
    found
}

unsafe fn enumerate() -> Result<Vec<Device>, String> {
    unsafe {
        let enumerator: IMMDeviceEnumerator =
            CoCreateInstance(&MMDeviceEnumerator, None, CLSCTX_ALL)
                .map_err(|e| format!("no audio device enumerator: {e}"))?;

        // Absent rather than fatal: a machine with no microphone at all still
        // gets a usable list, it is just empty.
        let default_id = enumerator
            .GetDefaultAudioEndpoint(eCapture, eConsole)
            .ok()
            .and_then(|d| device_id(&d));

        let collection = enumerator
            .EnumAudioEndpoints(eCapture, DEVICE_STATE_ACTIVE)
            .map_err(|e| format!("could not list capture devices: {e}"))?;

        let count = collection.GetCount().unwrap_or(0);
        let mut out = Vec::with_capacity(count as usize);
        for i in 0..count {
            let Ok(device) = collection.Item(i) else { continue };
            let Some(id) = device_id(&device) else { continue };
            let name = friendly_name(&device).unwrap_or_else(|| id.clone());
            let default = Some(&id) == default_id.as_ref();
            out.push(Device { id, name, default });
        }

        // The default first, then alphabetically. The list is read by someone
        // looking for a name they recognise, not by position.
        out.sort_by(|a, b| b.default.cmp(&a.default).then_with(|| a.name.cmp(&b.name)));
        Ok(out)
    }
}

unsafe fn device_id(device: &IMMDevice) -> Option<String> {
    unsafe {
        let raw = device.GetId().ok()?;
        let id = raw.to_string().ok();
        // GetId allocates with the COM task allocator and hands over
        // ownership, so this is ours to free.
        CoTaskMemFree(Some(raw.0 as *const _));
        id
    }
}

unsafe fn friendly_name(device: &IMMDevice) -> Option<String> {
    unsafe {
        let store = device.OpenPropertyStore(STGM_READ).ok()?;
        let value = store.GetValue(&PKEY_Device_FriendlyName).ok()?;
        let text = value.to_string();
        if text.is_empty() { None } else { Some(text) }
    }
}

pub struct Mic {
    enabled: Arc<AtomicBool>,
    buffer: Arc<Mutex<VecDeque<i16>>>,
    /// None when no capture device could be opened. The stream still runs;
    /// the toggle just reports that there is nothing to turn on.
    available: bool,
    /// Peak sample seen since the last read, so the console can show whether
    /// the mic is actually picking anything up. A mic that is on but reading
    /// zero is the single most likely thing to be silently wrong.
    peak: Arc<AtomicU32>,
    /// What was actually opened, which is not always what was asked for: a
    /// remembered device that has since been unplugged falls back to the
    /// default rather than leaving the person with no microphone and no
    /// explanation.
    opened: Option<Device>,
}

impl Mic {
    /// Opens the default capture device and starts buffering. Returns a Mic
    /// with `available: false` rather than an error if there is no device,
    /// a missing microphone should not stop someone sharing their screen.
    /// Opens `wanted` if it is still there, and the default otherwise.
    pub fn start_on(wanted: Option<String>, stop: Arc<AtomicBool>) -> Self {
        let enabled = Arc::new(AtomicBool::new(false));
        let buffer = Arc::new(Mutex::new(VecDeque::new()));
        let peak = Arc::new(AtomicU32::new(0));

        // Resolved here rather than inside the capture thread so the window
        // can say which device is live without reaching across a thread for
        // it. A remembered id that no longer matches anything becomes None,
        // which is the same as never having chosen.
        let available_devices = devices();
        let chosen = wanted
            .as_deref()
            .and_then(|id| available_devices.iter().find(|d| d.id == id))
            .or_else(|| available_devices.iter().find(|d| d.default))
            .cloned();

        let (probe_tx, probe_rx) = std::sync::mpsc::channel::<bool>();
        {
            let enabled = Arc::clone(&enabled);
            let buffer = Arc::clone(&buffer);
            let peak = Arc::clone(&peak);
            let id = chosen.as_ref().map(|d| d.id.clone());
            std::thread::spawn(move || {
                let ok = capture_loop(id, enabled, buffer, peak, stop, probe_tx);
                if let Err(e) = ok {
                    eprintln!("  microphone: {e}");
                }
            });
        }

        // Wait briefly for the thread to report whether it opened a device.
        let available = probe_rx
            .recv_timeout(std::time::Duration::from_secs(3))
            .unwrap_or(false);

        Self { enabled, buffer, available, peak, opened: chosen.filter(|_| available) }
    }

    /// The device actually in use, once one has been opened.
    pub fn opened(&self) -> Option<&Device> {
        self.opened.as_ref()
    }

    pub fn available(&self) -> bool {
        self.available
    }

    pub fn is_on(&self) -> bool {
        self.enabled.load(Ordering::Relaxed)
    }

    pub fn handle(&self) -> Arc<AtomicBool> {
        Arc::clone(&self.enabled)
    }

    /// Loudest sample captured since the last call, 0.0 to 1.0, and resets.
    pub fn take_peak(&self) -> f32 {
        self.peak.swap(0, Ordering::Relaxed) as f32 / i16::MAX as f32
    }

    /// Takes exactly `samples` interleaved samples, padding with silence when
    /// the mic is off or has not produced enough yet. Returning short would
    /// force every caller to handle a partial buffer for no benefit.
    pub fn take(&self, samples: usize) -> Vec<i16> {
        if !self.enabled.load(Ordering::Relaxed) {
            return vec![0i16; samples];
        }

        let mut out = Vec::with_capacity(samples);
        if let Ok(mut buf) = self.buffer.lock() {
            for _ in 0..samples {
                out.push(buf.pop_front().unwrap_or(0));
            }
        } else {
            out.resize(samples, 0);
        }
        out
    }
}

/// Sums two interleaved PCM streams, saturating rather than wrapping.
///
/// Wrapping here would be audible and awful: two loud signals would fold from
/// full positive to full negative and produce a click on every overflow.
pub fn mix_into(dst: &mut [i16], src: &[i16]) {
    for (d, s) in dst.iter_mut().zip(src.iter()) {
        *d = (*d as i32 + *s as i32).clamp(i16::MIN as i32, i16::MAX as i32) as i16;
    }
}

fn capture_loop(
    device_id: Option<String>,
    enabled: Arc<AtomicBool>,
    buffer: Arc<Mutex<VecDeque<i16>>>,
    peak: Arc<AtomicU32>,
    stop: Arc<AtomicBool>,
    probe: std::sync::mpsc::Sender<bool>,
) -> Result<(), String> {
    unsafe {
        CoInitializeEx(None, COINIT_MULTITHREADED)
            .ok()
            .map_err(|e| format!("CoInitializeEx failed: {e}"))?;
    }

    let result = (|| unsafe {
        let enumerator: IMMDeviceEnumerator =
            CoCreateInstance(&MMDeviceEnumerator, None, CLSCTX_ALL)
                .map_err(|e| format!("no audio device enumerator: {e}"))?;

        // The remembered device first, the default if that fails. Falling
        // back matters: an id is remembered across reboots and the device it
        // names can be unplugged in between, and losing the microphone
        // entirely because of a stale setting would be worse than quietly
        // using the one that is there.
        let device = device_id
            .as_deref()
            .and_then(|id| enumerator.GetDevice(&windows::core::HSTRING::from(id)).ok())
            .map_or_else(
                || {
                    enumerator
                        .GetDefaultAudioEndpoint(eCapture, eConsole)
                        .map_err(|e| format!("no default microphone: {e}"))
                },
                Ok,
            )?;

        let client: IAudioClient = device
            .Activate(CLSCTX_ALL, None)
            .map_err(|e| format!("could not open the microphone: {e}"))?;

        // Ask for the same format the rest of the pipeline uses and let the
        // audio engine resample and upmix. Without AUTOCONVERTPCM this would
        // have to match the device's own mix format, and mono 44.1 kHz mics
        // are common enough that the conversion is worth asking for.
        let format = WAVEFORMATEX {
            wFormatTag: WAVE_FORMAT_PCM as u16,
            nChannels: CHANNELS,
            nSamplesPerSec: SAMPLE_RATE,
            nAvgBytesPerSec: SAMPLE_RATE * BLOCK_ALIGN as u32,
            nBlockAlign: BLOCK_ALIGN as u16,
            wBitsPerSample: 16,
            cbSize: 0,
        };

        client
            .Initialize(
                AUDCLNT_SHAREMODE_SHARED,
                AUDCLNT_STREAMFLAGS_EVENTCALLBACK
                    | AUDCLNT_STREAMFLAGS_AUTOCONVERTPCM
                    | AUDCLNT_STREAMFLAGS_SRC_DEFAULT_QUALITY,
                200_000, // 20 ms
                0,
                &format,
                None,
            )
            .map_err(|e| format!("could not initialise the microphone: {e}"))?;

        let event: HANDLE = CreateEventW(None, false, false, None)
            .map_err(|e| format!("could not create mic event: {e}"))?;
        client
            .SetEventHandle(event)
            .map_err(|e| format!("could not set mic event: {e}"))?;

        let capture: IAudioCaptureClient = client
            .GetService()
            .map_err(|e| format!("could not get mic capture client: {e}"))?;
        client.Start().map_err(|e| format!("could not start mic: {e}"))?;

        let _ = probe.send(true);

        let max_samples = SAMPLE_RATE as usize / 1000 * MAX_BUFFERED_MS * CHANNELS as usize;

        while !stop.load(Ordering::Relaxed) {
            WaitForSingleObject(event, 200);

            loop {
                let packet = capture
                    .GetNextPacketSize()
                    .map_err(|e| format!("mic packet size failed: {e}"))?;
                if packet == 0 {
                    break;
                }

                let mut data: *mut u8 = std::ptr::null_mut();
                let mut frames: u32 = 0;
                let mut flags: u32 = 0;
                capture
                    .GetBuffer(&mut data, &mut frames, &mut flags, None, None)
                    .map_err(|e| format!("mic GetBuffer failed: {e}"))?;

                let samples = frames as usize * CHANNELS as usize;
                let silent = flags & AUDCLNT_BUFFERFLAGS_SILENT.0 as u32 != 0;
                let live = enabled.load(Ordering::Relaxed);

                // The level is measured whether or not the microphone is live.
                //
                // It used to be measured only while live, which made the meter
                // useless for the question people actually have, which is
                // "have I picked the right device". Muted is exactly when you
                // want to check that, and a meter that reads zero because
                // nothing is being captured looks identical to one reading
                // zero because the device is wrong.
                // Empty stands for "this buffer carried nothing", whether
                // that is the silence flag or a null pointer. Both mean the
                // same thing downstream and neither should reach the meter.
                let pcm: &[i16] = if silent || data.is_null() {
                    &[]
                } else {
                    std::slice::from_raw_parts(data as *const i16, samples)
                };

                if let Some(loudest) = pcm.iter().map(|s| s.unsigned_abs() as u32).max() {
                    peak.fetch_max(loudest, Ordering::Relaxed);
                }

                if let Ok(mut buf) = buffer.lock() {
                    if live {
                        if pcm.is_empty() {
                            buf.extend(std::iter::repeat_n(0, samples));
                        } else {
                            buf.extend(pcm);
                        }
                        while buf.len() > max_samples {
                            buf.pop_front();
                        }
                    } else {
                        // Drained but not kept: unmuting should start from
                        // now, not replay whatever was said while muted.
                        buf.clear();
                    }
                }

                capture
                    .ReleaseBuffer(frames)
                    .map_err(|e| format!("mic ReleaseBuffer failed: {e}"))?;
            }
        }

        let _ = client.Stop();
        Ok(())
    })();

    if result.is_err() {
        let _ = probe.send(false);
    }

    unsafe { CoUninitialize() };
    result
}

#[cfg(test)]
mod tests {
    use super::mix_into;

    #[test]
    fn mixing_sums_both_signals() {
        let mut game = [100i16, -100, 0, 50];
        mix_into(&mut game, &[10, 10, 10, 10]);
        assert_eq!(game, [110, -90, 10, 60]);
    }

    #[test]
    fn mixing_saturates_instead_of_wrapping() {
        // Wrapping would flip a loud positive peak to a loud negative one and
        // click audibly on every overflow.
        let mut loud = [30_000i16, -30_000];
        mix_into(&mut loud, &[10_000, -10_000]);
        assert_eq!(loud, [i16::MAX, i16::MIN]);
    }

    #[test]
    fn mixing_silence_changes_nothing() {
        let mut game = [1i16, 2, 3, 4];
        mix_into(&mut game, &[0, 0, 0, 0]);
        assert_eq!(game, [1, 2, 3, 4]);
    }

    #[test]
    fn mixing_stops_at_the_shorter_slice() {
        let mut game = [5i16, 5, 5];
        mix_into(&mut game, &[1, 1]);
        assert_eq!(game, [6, 6, 5]);
    }
}
