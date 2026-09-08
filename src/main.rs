// A GUI app, so Windows does not hand it a console window on launch, an
// empty black box appearing behind the strip looks broken even though nothing
// is wrong. The command-line modes still need somewhere to print, so when the
// process was started from a terminal it borrows that terminal's console
// instead of creating one. See `attach_parent_console`.
#![windows_subsystem = "windows"]

//! Sideband, stream one application, and only that application's audio,
//! to one person in a browser.
//!
//! Run with no arguments for the window; every command-line mode is listed in
//! `USAGE` below. Both routes drive the same engine through `Session`.

mod audio;
mod bwe;
mod capture;
mod encoder;
mod gui;
mod hotkey;
mod icons;
mod loopback;
mod mark;
mod mic;
mod net;
mod pipeline;
mod server;
mod session;
mod settings;
mod sources;
mod stream;
mod wav;

use std::io::{BufRead, Write};
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use crate::session::{Phase, Session};
use std::time::{Duration, Instant};

fn main() {
    // Only when there are arguments: a bare double-click is the GUI, and
    // attaching a console there would flash one up for no reason.
    if std::env::args().len() > 1 {
        attach_parent_console();
    }

    if let Err(e) = run() {
        eprintln!("\n  error: {e}");
        std::process::exit(1);
    }

    // Returning from `main` would be enough if every thread were ours, and it
    // is not. Capture, encode and audio each sit inside vendor DLLs, and a
    // thread parked in one of those at exit can hold the loader long enough
    // that the process outlives its own window: nothing on screen, no stream,
    // still resident, still holding the encoder session and the audio client.
    // The graceful wind down has already happened by this point, in `on_exit`
    // for the window and at the end of the session for the command line. This
    // is only the guarantee that nothing is left behind afterwards.
    let _ = std::io::stdout().flush();
    let _ = std::io::stderr().flush();
    std::process::exit(0);
}

/// Reuses the console of whatever launched us, if there was one.
///
/// A `windows` subsystem binary has no console of its own, so `println!` would
/// otherwise go nowhere when run from a terminal. This does not create a
/// console, if the process was started from Explorer there is nothing to
/// attach to and the call simply fails, which is the behaviour we want.
fn attach_parent_console() {
    use windows::Win32::System::Console::{
        AttachConsole, ATTACH_PARENT_PROCESS, STD_ERROR_HANDLE, STD_OUTPUT_HANDLE,
    };

    unsafe {
        if AttachConsole(ATTACH_PARENT_PROCESS).is_err() {
            return;
        }
    }

    // Re-open only the streams that have nowhere to go. If stdout was already
    // redirected, piped into another command, or captured to a file, then
    // pointing it at the console would silently steal that output, which
    // breaks every script that reads from us.
    unsafe {
        if !has_handle(STD_OUTPUT_HANDLE) {
            let _ = libc_freopen("CONOUT$\0", "w\0", Stream::Stdout);
        }
        if !has_handle(STD_ERROR_HANDLE) {
            let _ = libc_freopen("CONOUT$\0", "w\0", Stream::Stderr);
        }
    }
}

/// Whether a standard handle is already connected to something.
fn has_handle(which: windows::Win32::System::Console::STD_HANDLE) -> bool {
    use windows::Win32::Foundation::{HANDLE, INVALID_HANDLE_VALUE};
    use windows::Win32::System::Console::GetStdHandle;

    match unsafe { GetStdHandle(which) } {
        Ok(h) => h != HANDLE::default() && h != INVALID_HANDLE_VALUE,
        Err(_) => false,
    }
}

enum Stream {
    Stdout,
    Stderr,
}

/// Minimal `freopen` binding. The C runtime is already linked, and pulling in
/// a crate for two calls is not worth it.
unsafe fn libc_freopen(path: &str, mode: &str, which: Stream) -> Result<(), ()> {
    unsafe extern "C" {
        fn freopen(
            filename: *const u8,
            mode: *const u8,
            stream: *mut core::ffi::c_void,
        ) -> *mut core::ffi::c_void;
        fn __acrt_iob_func(index: u32) -> *mut core::ffi::c_void;
    }

    let index = match which {
        Stream::Stdout => 1,
        Stream::Stderr => 2,
    };

    let result = unsafe { freopen(path.as_ptr(), mode.as_ptr(), __acrt_iob_func(index)) };
    if result.is_null() {
        Err(())
    } else {
        Ok(())
    }
}

fn run() -> Result<(), String> {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let cmd = args.first().map(String::as_str).unwrap_or("");

    match cmd {
        // Test modes, kept because each one isolates a single stage.
        "audio" if args.len() == 3 => {
            let pid = parse_pid(&args[1])?;
            let secs = parse_secs(&args[2])?;
            let exe = exe_for(pid);
            capture(pid, &exe, Some(secs))
        }
        "window" if args.len() == 3 => capture_window(parse_pid(&args[1])?, parse_secs(&args[2])?),
        "encode" if args.len() == 3 => encode_window(parse_pid(&args[1])?, parse_secs(&args[2])?),

        // Serve the viewer page yourself. LAN and Tailscale need nothing else.
        "stream" => {
            let pid = match args.get(1) {
                Some(a) => parse_pid(a)?,
                None => pick_source()?,
            };
            let port: u16 = match args.get(2) {
                Some(a) => a.parse().map_err(|_| "port must be a number")?,
                None => 8088,
            };
            with_progress(move |s| stream::run_local(pid, port, s))
        }

        // Pair by code through a relay.
        "share" => {
            let pid = match args.get(1) {
                Some(a) => parse_pid(a)?,
                None => pick_source()?,
            };
            // Explicit argument first, then whatever the window last used,
            // which is also where SIDEBAND_RELAY lands. One relay, however you
            // told it.
            let relay = match args.get(2) {
                Some(a) => a.clone(),
                None => {
                    let remembered = settings::Settings::load().relay;
                    if remembered.is_empty() {
                        return Err(
                            "no relay given - pass one, set SIDEBAND_RELAY, or enter one in the window"
                                .into(),
                        );
                    }
                    remembered
                }
            };
            with_progress(move |s| stream::run_relay(pid, &relay, s))
        }

        // No arguments means the window.
        "" => gui::run(),

        "help" | "-h" | "--help" => {
            println!("{USAGE}");
            Ok(())
        }

        _ => Err(format!("unknown command {cmd:?}
{USAGE}")),
    }
}

const USAGE: &str = "  sideband                      pick a window and share it
  sideband share  [pid] [relay] pair by code through a relay
  sideband stream [pid] [port]  serve the viewer page yourself

  sideband audio  <pid> <secs>  record that app's audio to a WAV
  sideband window <pid> <secs>  capture a frame to a BMP
  sideband encode <pid> <secs>  encode to out.h264

  Ctrl+Alt+M toggles the microphone while streaming.
  While streaming, Enter lists applications and a number switches to one.
  Set SIDEBAND_RELAY to your Worker URL to share by code by default.";

/// Reads stdin on its own thread, one line at a time.
///
/// The read-out loop cannot block on stdin: it has a stream to keep reporting
/// on, and it needs to answer an approval prompt *and* accept a source change
/// from the same keyboard. A channel lets it take whichever arrives.
fn stdin_lines() -> std::sync::mpsc::Receiver<String> {
    let (tx, rx) = std::sync::mpsc::channel();
    std::thread::spawn(move || {
        for line in std::io::stdin().lock().lines() {
            let Ok(line) = line else { break };
            if tx.send(line).is_err() {
                break;
            }
        }
    });
    rx
}

fn print_sources(list: &[sources::Source]) {
    println!();
    for (i, s) in list.iter().enumerate() {
        let note = if s.frame_hosted { "  (no audio, see README)" } else { "" };
        println!("  {:>3}  {:<24} pid {}{note}", i + 1, s.exe, s.pid);
    }
    println!("
  Type a number to share that one instead.
");
}

/// Runs a streaming session with a terminal read-out of its progress. The GUI
/// reads the same `Session`; this is only a different way of drawing it.
fn with_progress<F>(body: F) -> Result<(), String>
where
    F: FnOnce(Arc<Session>) -> Result<(), String>,
{
    let session = Arc::new(Session::default());
    // One setting, however it was turned on. Someone who ticked the box in the
    // window and then ran the command line would otherwise be asked to approve
    // a viewer by a prompt they had already said they did not want.
    session.set_auto_approve(settings::Settings::load().auto_approve);
    let input = stdin_lines();

    let printer = {
        let session = Arc::clone(&session);
        std::thread::spawn(move || {
            let mut last: Option<Phase> = None;
            let mut ticks = 0u32;
            // The list the numbers typed below refer to. Remembered rather
            // than re-enumerated, so a window opening between printing and
            // typing cannot shift what a number means.
            let mut listed: Vec<sources::Source> = Vec::new();
            // A notice stays readable for several seconds so a window can
            // draw it; a terminal prints it once.
            let mut said: Option<String> = None;
            loop {
                let phase = session.phase();
                if Some(&phase) != last.as_ref() {
                    match &phase {
                        Phase::Preparing(what) => println!("  {what}…"),
                        Phase::Waiting { code, link } => {
                            println!();
                            if let Some(code) = code {
                                println!("  Code   {code}");
                            }
                            println!("  Link   {link}");
                            println!("
  Waiting for a viewer…
");
                        }
                        Phase::Approving { viewer } => {
                            println!();
                            println!("  Someone with your code wants to watch, from {viewer}.");
                            println!("  Only allow this if you were expecting them.");
                            print!("  Allow? [y/N]  ");
                            std::io::stdout().flush().ok();

                            // Bounded, because the engine writes the request
                            // off as a refusal after a minute and a reader
                            // still waiting past that would swallow the next
                            // thing typed.
                            let allowed = input
                                .recv_timeout(Duration::from_secs(60))
                                .map(|line| matches!(line.trim(), "y" | "Y" | "yes"))
                                .unwrap_or(false);

                            if allowed {
                                session.approve();
                            } else {
                                session.deny();
                            }
                            println!();
                        }
                        Phase::Live => {
                            println!("  Viewer connected.");
                            println!("  Press Enter for the application list, or a number to switch.
");
                        }
                        Phase::Failed(why) => println!("
  Failed: {why}
"),
                        Phase::Ended => println!("
  Sharing ended.
"),
                        Phase::Idle => {}
                    }
                    last = Some(phase.clone());
                }

                if matches!(phase, Phase::Failed(_) | Phase::Ended) {
                    return;
                }

                // Whatever has been typed since the last pass. Swapping the
                // shared application does not touch the connection, so there
                // is no reason to make someone stop and start again for it.
                if matches!(phase, Phase::Live) {
                    while let Ok(line) = input.try_recv() {
                        let line = line.trim().to_owned();
                        match line.parse::<usize>() {
                            Ok(n) if n >= 1 && n <= listed.len() => {
                                let picked = &listed[n - 1];
                                println!("  Now sharing {} (pid {}).
", picked.exe, picked.pid);
                                session.select_source(picked.pid);
                            }
                            _ => {
                                listed = sources::list().unwrap_or_default();
                                print_sources(&listed);
                            }
                        }
                    }

                    let note = session.notice();
                    if note.is_some() && note != said {
                        println!("  {}
", note.as_deref().unwrap_or_default());
                    }
                    said = note;
                }

                // A line every five seconds while live, so it is obvious the
                // stream is alive without scrolling the terminal away.
                if matches!(phase, Phase::Live) {
                    ticks += 1;
                    if ticks % 20 == 0 {
                        let (frames, packets, _) = session.counters();
                        let mic = if session.mic_on() {
                            format!("  mic ON peak {:.0}%", session.mic_peak() * 100.0)
                        } else {
                            String::new()
                        };
                        // The packet count says the audio stream is running,
                        // never that it carries anything: silence is gap
                        // filled and paces identically. This is the figure
                        // that answers "why can she not hear the game".
                        let app = if !session.app_audio_ok() {
                            "  app audio UNAVAILABLE".to_owned()
                        } else {
                            format!("  app {:.0}%", session.app_peak() * 100.0)
                        };
                        // The quality figure is what the viewer's connection
                        // turned out to support, which is the number worth
                        // watching when a stream is struggling.
                        let quality = match session.quality() {
                            Some((kbps, fps)) => {
                                format!("  {:.1} Mbit/s at {fps} fps", kbps as f32 / 1000.0)
                            }
                            None => String::new(),
                        };
                        println!("  {frames} video / {packets} audio sent{quality}{app}{mic}");
                    }
                }

                std::thread::sleep(std::time::Duration::from_millis(250));
            }
        })
    };

    println!("
  sideband");
    let result = body(Arc::clone(&session));
    session.request_stop();
    let _ = printer.join();
    result
}

fn parse_pid(s: &str) -> Result<u32, String> {
    s.parse().map_err(|_| format!("{s:?} is not a pid"))
}

fn parse_secs(s: &str) -> Result<f64, String> {
    s.parse().map_err(|_| format!("{s:?} is not a number of seconds"))
}

fn exe_for(pid: u32) -> String {
    sources::list()
        .ok()
        .and_then(|l| l.into_iter().find(|s| s.pid == pid).map(|s| s.exe))
        .unwrap_or_else(|| format!("pid{pid}"))
}

/// Lists what can be captured and asks which one.
fn pick_source() -> Result<u32, String> {
    let list = sources::list().map_err(|e| format!("could not enumerate windows: {e}"))?;
    if list.is_empty() {
        return Err("no visible windows found".into());
    }

    println!("
  sideband
");
    for (i, s) in list.iter().enumerate() {
        let title = if s.title.chars().count() > 44 {
            let t: String = s.title.chars().take(43).collect();
            format!("{t}…")
        } else {
            s.title.clone()
        };
        let note = if s.frame_hosted { "  (no audio)" } else { "" };
        println!("  {:>3}  {:<24} {:<45} pid {}{note}", i + 1, s.exe, title, s.pid);
    }

    print!("
  share which? [1-{}]  ", list.len());
    std::io::stdout().flush().ok();

    let mut line = String::new();
    std::io::stdin()
        .lock()
        .read_line(&mut line)
        .map_err(|e| format!("could not read input: {e}"))?;

    let idx: usize = line
        .trim()
        .parse::<usize>()
        .ok()
        .filter(|n| *n >= 1 && *n <= list.len())
        .ok_or("not a valid choice")?;

    Ok(list[idx - 1].pid)
}

/// Stage 4, the full video path: capture a window, pace it to a steady
/// cadence, encode with NVENC, write raw H.264. Play the result with
/// `ffplay out.h264`, or just check it decodes.
fn encode_window(pid: u32, seconds: f64) -> Result<(), String> {
    use std::io::Write as _;

    let src = sources::list()
        .map_err(|e| format!("could not enumerate windows: {e}"))?
        .into_iter()
        .find(|s| s.pid == pid)
        .ok_or("no visible window belongs to that pid")?;

    unsafe {
        windows::Win32::System::Com::CoInitializeEx(
            None,
            windows::Win32::System::Com::COINIT_MULTITHREADED,
        )
        .ok()
        .map_err(|e| format!("CoInitializeEx failed: {e}"))?;
    }

    const FPS: u32 = 60;
    const BITRATE: u32 = 10_000_000;

    // Halfway through, the encoder is retuned in place, the same call the
    // rate controller makes when a viewer's connection cannot keep up. It is
    // exercised here because a driver that quietly refuses it would leave
    // adaptation doing nothing at all, and nothing else would say so.
    const REDUCED_BITRATE: u32 = 2_000_000;
    const REDUCED_FPS: u32 = 30;

    let cap = capture::WindowCapture::start(src.hwnd)
        .map_err(|e| format!("could not start window capture: {e}"))?;

    let out = PathBuf::from("out.h264");
    let mut file =
        std::fs::File::create(&out).map_err(|e| format!("could not create {}: {e}", out.display()))?;

    println!("\n  encoding window of {} (pid {})", src.exe, pid);
    println!("  target      {FPS} fps, {} Mbit/s CBR", BITRATE / 1_000_000);
    println!("\n  Encoding for {seconds}s…\n");

    let clock = pipeline::MediaClock::start();
    let mut pacer: pipeline::Pacer<
        windows::Win32::Graphics::Direct3D11::ID3D11Texture2D,
    > = pipeline::Pacer::new(FPS);
    let mut enc: Option<encoder::NvencEncoder> = None;
    let mut dims: (u32, u32) = (0, 0);
    let mut bytes_out: u64 = 0;

    // Per-half tallies, so the retune can be shown to have taken effect
    // rather than merely reported as accepted.
    let midpoint = seconds / 2.0;
    let mut retuned = false;
    let mut halves = [(0u64, 0u64); 2]; // (bytes, frames)

    let start = Instant::now();
    while start.elapsed().as_secs_f64() < seconds {
        let fresh = cap
            .next_texture()
            .map_err(|e| format!("frame capture failed: {e}"))?;

        // Rebuild on any geometry change. Dropping the old session first
        // matters: consumer cards cap concurrent NVENC sessions, so holding
        // two open across the swap can fail on the third or fourth resize.
        if let Some((_, w, h)) = &fresh {
            if (*w, *h) != dims {
                drop(enc.take());
                enc = Some(encoder::NvencEncoder::new(
                    cap.device(),
                    *w,
                    *h,
                    FPS,
                    BITRATE,
                )?);
                // The cached frame is the old size and must not reach the
                // freshly-sized encoder.
                pacer.reset();
                dims = (*w, *h);
                println!("  source      {w}x{h}");
            }
        }

        if !retuned && start.elapsed().as_secs_f64() >= midpoint {
            retuned = true;
            if let Some(e) = enc.as_mut() {
                match e.reconfigure(REDUCED_BITRATE, REDUCED_FPS) {
                    Ok(()) => {
                        pacer.set_fps(e.fps());
                        println!(
                            "  retuned     {} Mbit/s at {} fps",
                            e.bitrate() / 1_000_000,
                            e.fps()
                        );
                    }
                    Err(err) => println!("  retune      REFUSED - {err}"),
                }
            }
        }

        let fresh_tex = fresh.map(|(t, _, _)| t);

        if let Some(paced) = pacer.tick_at(clock.now_us(), fresh_tex) {
            if let Some(e) = enc.as_mut() {
                if let Some(au) = e.encode(&paced.frame, paced.timestamp_us)? {
                    bytes_out += au.len() as u64;
                    let half = usize::from(retuned);
                    halves[half].0 += au.len() as u64;
                    halves[half].1 += 1;
                    file.write_all(&au)
                        .map_err(|e| format!("could not write {}: {e}", out.display()))?;
                }
            }
        }

        std::thread::sleep(Duration::from_millis(1));
    }

    let elapsed = start.elapsed().as_secs_f64();
    let stats = pacer.stats();
    let encoded = enc.as_ref().map(|e| e.frames_encoded()).unwrap_or(0);

    println!("\n  paced       {} frames ({:.0}% repeats, {} slots dropped)",
        stats.delivered, stats.repeat_ratio() * 100.0, stats.dropped_slots);
    println!("  encoded     {encoded} frames in {elapsed:.1}s");
    println!("  wrote       {} KB to {}", bytes_out / 1024, out.display());
    println!("  bitrate     {:.1} Mbit/s actual",
        (bytes_out as f64 * 8.0) / elapsed / 1_000_000.0);

    // The two halves are the evidence. If they read the same, the retune was
    // accepted and then ignored, and adaptive quality is doing nothing.
    let half_secs = elapsed / 2.0;
    for (n, label) in [(0usize, "before"), (1, "after ")] {
        println!(
            "  {label}      {:.1} Mbit/s, {:.0} fps",
            (halves[n].0 as f64 * 8.0) / half_secs / 1_000_000.0,
            halves[n].1 as f64 / half_secs,
        );
    }
    println!();

    Ok(())
}

/// Stage 2, pull frames off a window for `seconds` and report the rate.
/// Saves one frame as a BMP so there is something to actually look at.
fn capture_window(pid: u32, seconds: f64) -> Result<(), String> {
    let src = sources::list()
        .map_err(|e| format!("could not enumerate windows: {e}"))?
        .into_iter()
        .find(|s| s.pid == pid)
        .ok_or("no visible window belongs to that pid")?;

    unsafe {
        windows::Win32::System::Com::CoInitializeEx(
            None,
            windows::Win32::System::Com::COINIT_MULTITHREADED,
        )
        .ok()
        .map_err(|e| format!("CoInitializeEx failed: {e}"))?;
    }

    let cap = capture::WindowCapture::start(src.hwnd)
        .map_err(|e| format!("could not start window capture: {e}"))?;

    let out = PathBuf::from(format!("frame-{}.bmp", src.exe.replace(".exe", "")));

    println!("\n  capturing window of {} (pid {})", src.exe, pid);
    println!("  item title  {}", cap.title());
    println!("\n  Capturing for {seconds}s…\n");

    let start = Instant::now();
    let mut frames = 0u32;
    let mut dims = (0u32, 0u32);
    let mut saved = false;

    while start.elapsed().as_secs_f64() < seconds {
        // Save the first frame that arrives after the window has settled.
        // Time-based, not frame-count-based: WGC is redraw-driven, so a
        // mostly-static window may only produce a couple of frames a second.
        let save = if !saved && start.elapsed().as_secs_f64() > 0.5 {
            Some(out.as_path())
        } else {
            None
        };

        match cap.try_frame(save) {
            Ok(Some(d)) => {
                dims = d;
                frames += 1;
                if save.is_some() {
                    saved = true;
                }
            }
            Ok(None) => std::thread::sleep(Duration::from_millis(2)),
            Err(e) => return Err(format!("frame capture failed: {e}")),
        }
    }

    let elapsed = start.elapsed().as_secs_f64();
    println!("  {} frames in {:.1}s - {:.1} fps", frames, elapsed, frames as f64 / elapsed);
    println!("  frame size {}x{}", dims.0, dims.1);
    if saved {
        println!("  wrote {}\n", out.display());
    } else {
        println!("  no frame saved (fewer than 11 frames arrived)\n");
    }

    Ok(())
}

fn capture(pid: u32, exe: &str, seconds: Option<f64>) -> Result<(), String> {
    let out = PathBuf::from(format!("capture-{}.wav", exe.replace(".exe", "")));

    println!("\n  capturing {exe} (pid {pid})");
    println!("  writing   {}", out.display());
    match seconds {
        Some(s) => println!("\n  Recording for {s}s…\n"),
        None => {
            println!("\n  Play some game audio, and get someone talking in Discord.");
            println!("  Press Enter to stop.\n");
        }
    }

    let stop = Arc::new(AtomicBool::new(false));
    let worker = {
        let stop = Arc::clone(&stop);
        let out = out.clone();
        std::thread::spawn(move || loopback::record_to_wav(pid, &out, stop))
    };

    match seconds {
        Some(s) => std::thread::sleep(std::time::Duration::from_secs_f64(s)),
        None => {
            let mut discard = String::new();
            std::io::stdin().lock().read_line(&mut discard).ok();
        }
    }
    stop.store(true, Ordering::Relaxed);

    let report = worker
        .join()
        .map_err(|_| "capture thread panicked".to_string())??;

    let secs =
        report.wav_bytes as f64 / (loopback::SAMPLE_RATE as f64 * loopback::BLOCK_ALIGN as f64);
    let silence = report.silence_ratio;

    println!("\n  wav     {:.1}s ({} KB) to {}", secs, report.wav_bytes / 1024, out.display());
    println!("  silence {:.0}% synthesised", silence * 100.0);
    println!(
        "  opus    {} frames, {} KB, {:.0} kbit/s",
        report.opus_frames,
        report.opus_bytes / 1024,
        report.opus_bitrate_bps / 1000.0
    );
    println!();

    if silence > 0.95 {
        println!("  Nearly all silence - either the app made no sound, or its audio");
        println!("  comes from a process outside the tree. Try picking a different");
        println!("  window belonging to the same app.\n");
    }

    Ok(())
}
