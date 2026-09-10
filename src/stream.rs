//! The whole pipeline, running at once.
//!
//! Threading follows the hardware. Both capture APIs are synchronous and COM
//! apartment-bound, so each gets a dedicated OS thread that owns its own
//! apartment; only the sending side is async. The threads hand finished,
//! encoded units across bounded channels.
//!
//! Those channels block rather than drop when full, and that is deliberate.
//! Dropping an H.264 P-frame corrupts every frame that references it, so a
//! stalled network must apply back-pressure to capture instead. The pacer's
//! slot-skipping then absorbs the catch-up cleanly.
//!
//! Nothing here prints. Progress goes into `Session`, and the window or the
//! terminal renders it, which is what lets one engine back both.

use std::net::SocketAddr;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

use tokio::sync::mpsc;

use crate::audio::OpusPacket;
use crate::pipeline::VideoEncoder;
use crate::session::{Phase, Session};
use crate::{bwe, capture, encoder, hotkey, loopback, mic, net, pipeline, server, sources};

/// The fastest we ever capture or send. The rate controller may settle below
/// this, see `bwe`, but never above it, because the source cannot produce
/// more and a browser gains nothing from being told otherwise.
const FPS: u32 = 60;

/// How often to fold the viewer's feedback into a new target.
///
/// One second, because that is roughly how often a browser sends a receiver
/// report; reacting faster would mean reacting repeatedly to the same report.
const CONTROL_INTERVAL: Duration = Duration::from_secs(1);

/// Periodic keyframes, if any.
///
/// `None`, and that is a measured decision rather than an omission. Forcing an
/// IDR mid-stream against `rtc-rtp` 0.20.4 breaks decoding outright: measured
/// on a 1080p60 game source, a 2-second IDR gave 9.7 fps decoded with 3.8
/// picture-loss requests per second, while the identical stream with no
/// periodic IDR gave 64 fps and zero. Two separate causes were fixed along the
/// way, repeated parameter sets (see `encoder::strip_parameter_sets`) and a
/// one-frame VBV budget, and neither accounted for it. The remaining fault is
/// in how the library packetises a forced IDR, and a "safety net" that costs
/// six sevenths of the frame rate is not a safety net.
///
/// Loss recovery does not depend on this. The default interceptors run a NACK
/// responder that buffers outgoing RTP and retransmits it on request, which is
/// what repairs ordinary packet loss, keyframes are only needed for loss too
/// large to retransmit through, and a viewer in that position can reload the
/// page for a fresh session.
///
/// Set this to `Some(interval)` if the library's payloader is ever fixed.
const KEYFRAME_INTERVAL: Option<Duration> = None;

/// Serve the viewer page ourselves. Nothing external is involved, which is why
/// this is the right choice on a LAN or a tailnet.
pub fn run_local(pid: u32, port: u16, session: Arc<Session>) -> Result<(), String> {
    with_runtime(Arc::clone(&session), move |rt| {
        rt.block_on(async move { serve_local(pid, port, session).await })
    })
}

/// Pair by code through a relay, for when the two machines cannot reach each
/// other directly.
pub fn run_relay(pid: u32, relay: &str, session: Arc<Session>) -> Result<(), String> {
    let relay = relay.trim_end_matches('/').to_owned();
    with_runtime(Arc::clone(&session), move |rt| {
        rt.block_on(async move { serve_relay(pid, relay, session).await })
    })
}

fn with_runtime<F>(session: Arc<Session>, body: F) -> Result<(), String>
where
    F: FnOnce(tokio::runtime::Runtime) -> Result<(), String>,
{
    let rt = match tokio::runtime::Builder::new_multi_thread().enable_all().build() {
        Ok(rt) => rt,
        Err(e) => {
            let msg = format!("could not start async runtime: {e}");
            session.fail(msg.clone());
            return Err(msg);
        }
    };

    let result = body(rt);
    if let Err(e) = &result {
        // Only the first failure is interesting; a later generic error should
        // not overwrite a specific one already recorded.
        if !matches!(session.phase(), Phase::Failed(_)) {
            session.fail(e.clone());
        }
    }
    result
}

fn describe_source(pid: u32, session: &Session) -> Result<(), String> {
    let src = sources::list()
        .map_err(|e| format!("could not enumerate windows: {e}"))?
        .into_iter()
        .find(|s| s.pid == pid)
        .ok_or("no visible window belongs to that pid")?;
    session.set_source(&src.exe, &src.title);

    // From here the session, not this argument, is what says which
    // application is being shared, so that it can be changed later without
    // taking the connection down.
    session.select_source(pid);
    Ok(())
}

async fn serve_local(pid: u32, port: u16, session: Arc<Session>) -> Result<(), String> {
    describe_source(pid, &session)?;

    session.preparing("starting up");
    let webrtc = net::connect(net::ice_servers()).await?;
    let keyframe = webrtc.keyframe_signal();

    session.preparing("gathering network candidates");
    let offer = webrtc.offer().await?;

    let (answer_tx, mut answer_rx) = mpsc::channel::<String>(1);
    let addr: SocketAddr = ([0, 0, 0, 0], port).into();

    // Being on the same network is not consent to watch someone's screen, so
    // the link carries a secret and every route on the server checks it.
    let secret = server::make_secret();
    let local = server::LocalSession::new(secret.clone(), offer);
    tokio::spawn(server::serve(addr, local, answer_tx));

    session.set_phase(Phase::Waiting {
        code: None,
        link: format!("http://{}:{port}/v/{secret}", local_address()),
    });

    let answer = answer_rx.recv().await.ok_or("signalling closed")?;

    if !approved(&answer, &session).await {
        return Err("you turned that viewer away".into());
    }

    session.preparing("connecting");
    webrtc.accept_answer(&answer).await?;
    tokio::time::timeout(CONNECT_TIMEOUT, webrtc.wait_connected())
        .await
        .map_err(|_| "the viewer answered but never connected".to_string())?;

    session.set_phase(Phase::Live);
    pump(&webrtc, keyframe, session).await
}

/// How long to give a viewer to actually connect after they answer. Past
/// this the attempt is written off and a fresh offer goes up under the same
/// code, so their retry has something current to answer.
const CONNECT_TIMEOUT: Duration = Duration::from_secs(25);

/// How many viewer attempts to sit through before giving up.
const MAX_ATTEMPTS: usize = 3;

/// How long the approval prompt waits before treating silence as a refusal.
/// An unanswered prompt means nobody was at the keyboard, and the safe reading
/// of that is no.
const APPROVAL_TIMEOUT: Duration = Duration::from_secs(60);

async fn serve_relay(pid: u32, relay: String, session: Arc<Session>) -> Result<(), String> {
    describe_source(pid, &session)?;

    session.preparing("starting up");

    // The relay issues the code and a secret token. The host cannot choose its
    // own code, that would let one be squatted, and would leave the relay
    // nothing to attach a per-client limit to.
    let ticket = create_session(&relay).await?;
    let link = format!("{relay}/{}", ticket.code);

    // Whatever happens from here, the session is torn down rather than left
    // sitting on the relay until it expires.
    let outcome = relay_attempts(&relay, &ticket, &link, Arc::clone(&session)).await;
    destroy_session(&relay, &ticket).await;
    outcome
}

async fn relay_attempts(
    relay: &str,
    ticket: &Ticket,
    link: &str,
    session: Arc<Session>,
) -> Result<(), String> {
    for attempt in 0..MAX_ATTEMPTS {
        if attempt > 0 {
            session.preparing("that attempt did not connect - offering again");
        }

        let webrtc = net::connect(net::ice_servers()).await?;
        let keyframe = webrtc.keyframe_signal();

        session.preparing("gathering network candidates");
        let offer = webrtc.offer().await?;

        session.preparing("publishing to the relay");
        put_offer(relay, ticket, offer).await?;

        session.set_phase(Phase::Waiting {
            code: Some(ticket.code.clone()),
            link: link.to_owned(),
        });

        let answer = poll_answer(relay, ticket, &session).await?;

        // Knowing the code is not enough. Someone has to say yes.
        if !approved(&answer, &session).await {
            return Err("you turned that viewer away".into());
        }

        session.preparing("connecting");
        webrtc.accept_answer(&answer).await?;

        match tokio::time::timeout(CONNECT_TIMEOUT, webrtc.wait_connected()).await {
            Ok(()) => {
                // Connected. Nothing about this session needs to remain
                // fetchable, so it goes now rather than at its expiry.
                destroy_session(relay, ticket).await;
                session.set_phase(Phase::Live);
                return pump(&webrtc, keyframe, session).await;
            }
            // A peer connection cannot take a second answer once it has one,
            // so recovering means a whole new connection and a new offer.
            Err(_) => continue,
        }
    }

    Err("the viewer could not connect after several attempts".into())
}

/// What the relay hands back when a session is created.
struct Ticket {
    code: String,
    /// Proves we are the host. Never shown, never spoken, never in a URL,
    /// only this process and the relay ever hold it.
    token: String,
}

async fn create_session(relay: &str) -> Result<Ticket, String> {
    let url = format!("{relay}/api/session");
    let body: serde_json::Value = tokio::task::spawn_blocking(move || {
        ureq::post(&url)
            .send_empty()
            .map_err(|e| format!("could not reach the relay: {e}"))?
            .body_mut()
            .read_json()
            .map_err(|e| format!("relay sent something unexpected: {e}"))
    })
    .await
    .map_err(|e| format!("create task failed: {e}"))??;

    let code = body
        .get("code")
        .and_then(|v| v.as_str())
        .ok_or("relay did not return a code")?
        .to_owned();
    let token = body
        .get("token")
        .and_then(|v| v.as_str())
        .ok_or("relay did not return a token")?
        .to_owned();

    Ok(Ticket { code, token })
}

async fn put_offer(relay: &str, ticket: &Ticket, offer: String) -> Result<(), String> {
    let url = format!("{relay}/api/session/{}/offer", ticket.code);
    let token = ticket.token.clone();
    tokio::task::spawn_blocking(move || {
        ureq::put(&url)
            .header("Authorization", &format!("Bearer {token}"))
            .send(&offer)
            .map(|_| ())
            .map_err(|e| format!("could not publish to the relay: {e}"))
    })
    .await
    .map_err(|e| format!("publish task failed: {e}"))?
}

/// Waits for the viewer's answer.
///
/// Only the host can read this, which is the point: an answer carries the
/// viewer's ICE candidates, and those contain their public IP address. Leaving
/// it readable to anyone holding the code would hand out the address of the
/// person watching, which they never agreed to.
async fn poll_answer(
    relay: &str,
    ticket: &Ticket,
    session: &Arc<Session>,
) -> Result<String, String> {
    let url = format!("{relay}/api/session/{}/answer", ticket.code);
    let token = ticket.token.clone();
    let session = Arc::clone(session);

    tokio::task::spawn_blocking(move || -> Result<String, String> {
        let deadline = Instant::now() + Duration::from_secs(300);
        while Instant::now() < deadline {
            if session.should_stop() {
                return Err("cancelled".into());
            }
            let request = ureq::get(&url).header("Authorization", &format!("Bearer {token}"));
            match request.call() {
                Ok(mut r) => {
                    return r
                        .body_mut()
                        .read_to_string()
                        .map_err(|e| format!("could not read the answer: {e}"));
                }
                // Nobody has answered yet. This is the normal case.
                Err(ureq::Error::StatusCode(404)) => {}
                Err(ureq::Error::StatusCode(403)) => {
                    return Err("the relay rejected our token".into())
                }
                Err(e) => return Err(format!("relay poll failed: {e}")),
            }
            std::thread::sleep(Duration::from_millis(700));
        }
        Err("nobody joined before the code expired".to_owned())
    })
    .await
    .map_err(|e| format!("poll task failed: {e}"))?
}

/// Best effort: a failure here costs nothing, because the relay expires the
/// session on its own regardless.
async fn destroy_session(relay: &str, ticket: &Ticket) {
    let url = format!("{relay}/api/session/{}", ticket.code);
    let token = ticket.token.clone();
    let _ = tokio::task::spawn_blocking(move || {
        ureq::delete(&url)
            .header("Authorization", &format!("Bearer {token}"))
            .call()
    })
    .await;
}

/// Puts the request to the host and waits for an answer.
///
/// A denial ends the session rather than going back to waiting. The code has
/// already been claimed at that point, and someone the host just refused is
/// exactly the person who should not get another go at it, starting again
/// issues a fresh code.
async fn approved(answer: &str, session: &Arc<Session>) -> bool {
    // Asked for explicitly, so there is nobody to ask. The viewer is still
    // named in the read-out rather than let in silently: not having to answer
    // is the point, not being unable to see who arrived.
    if session.auto_approve() {
        session.note(format!("let {} in without asking", describe_viewer(answer)));
        return true;
    }

    session.request_approval(&describe_viewer(answer));

    let deadline = Instant::now() + APPROVAL_TIMEOUT;
    while Instant::now() < deadline {
        if session.should_stop() {
            return false;
        }
        if let Some(decision) = session.approval_decision() {
            return decision;
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    false
}

/// A short description of whoever is asking, taken from their SDP.
///
/// The server-reflexive candidate is their address as the world sees it, which
/// is the one worth showing: it is what tells the host whether this is the
/// person they just sent a code to or someone else entirely.
fn describe_viewer(answer: &str) -> String {
    let mut host_candidate = None;

    for line in answer.lines() {
        let Some(rest) = line.strip_prefix("a=candidate:") else {
            continue;
        };
        let fields: Vec<&str> = rest.split_whitespace().collect();
        // foundation component transport priority ADDRESS port typ TYPE
        if fields.len() < 8 {
            continue;
        }
        let (address, kind) = (fields[4], fields[7]);

        match kind {
            "srflx" | "relay" => return address.to_owned(),
            "host" if host_candidate.is_none() => host_candidate = Some(address.to_owned()),
            _ => {}
        }
    }

    // mDNS hides host candidates behind a random .local name, so falling back
    // to one is often useless, say so rather than showing noise.
    match host_candidate {
        Some(a) if !a.ends_with(".local") => a,
        _ => "address not shared".to_owned(),
    }
}

/// One line per second of what the viewer reported and what was decided.
///
/// Off unless `SIDEBAND_DEBUG_RATE` is set. This exists because the connection
/// that matters is somebody else's, on the other side of the internet, and
/// cannot be reproduced here, when a stream misbehaves, this is the record of
/// whether the viewer was reporting loss, reporting a low estimate, or
/// reporting nothing at all, which are three different problems.
fn trace_rate(feedback: &bwe::Feedback, target: bwe::Target) {
    if std::env::var_os("SIDEBAND_DEBUG_RATE").is_none() {
        return;
    }
    let loss = match feedback.loss {
        Some(l) => format!("{:>5.1}%", l * 100.0),
        None => "  n/a".to_owned(),
    };
    let estimate = match feedback.receiver_estimate {
        Some(bps) => format!("{:.2}", bps as f64 / 1_000_000.0),
        None => " n/a".to_owned(),
    };
    eprintln!(
        "  rate  loss {loss}  remb {estimate} Mbit/s  pli {}  nack {}  out {:.2} Mbit/s  ->  {:.2} Mbit/s at {} fps",
        feedback.picture_loss,
        feedback.nacks,
        feedback.sent_bitrate as f64 / 1_000_000.0,
        target.bitrate as f64 / 1_000_000.0,
        target.fps,
    );
}

/// Capture, encode and send until the connection ends or a stop is requested.
async fn pump<P: webrtc::peer_connection::PeerConnection>(
    webrtc: &net::Session<P>,
    keyframe: net::KeyframeSignal,
    session: Arc<Session>,
) -> Result<(), String> {
    // One clone stays here to force an IDR on resume; the other is moved
    // into the encode thread, which services the request.
    let resume_keyframe = keyframe.clone();
    let stop = Arc::new(AtomicBool::new(false));

    // Nothing is known about the viewer's connection yet, so the stream opens
    // at a rate almost any home link can carry and earns its way up from
    // there. Sending the ceiling first and waiting to be told to stop is how
    // the picture freezes within seconds of connecting.
    let mut controller = bwe::Controller::new(bwe::START_BITRATE, FPS);
    let quality = bwe::Quality::new(controller.target());

    // Whichever device was chosen in the window, or the default if that one
    // has since been unplugged. Read here rather than passed in, so the
    // command line honours the same choice.
    let chosen_mic = crate::settings::Settings::load().mic_device;
    let microphone = Arc::new(mic::Mic::start_on(
        Some(chosen_mic).filter(|id| !id.is_empty()),
        Arc::clone(&stop),
    ));
    if let Some(device) = microphone.opened() {
        session.set_mic_name(device.name.clone());
    }
    session.set_mic_available(microphone.available());
    if microphone.available() {
        let ui = Arc::clone(&session);
        hotkey::spawn_toggle(microphone.handle(), move |on| ui.set_mic_on(on));
    }

    // Small queues on purpose: they exist to smooth jitter, not to buffer.
    let (video_tx, mut video_rx) = mpsc::channel::<(Vec<u8>, u64)>(4);
    let (audio_tx, mut audio_rx) = mpsc::channel::<OpusPacket>(32);

    let video_thread = {
        let stop = Arc::clone(&stop);
        let session = Arc::clone(&session);
        let quality = quality.clone();
        std::thread::spawn(move || video_loop(keyframe, stop, video_tx, session, quality))
    };
    let audio_thread = {
        let stop = Arc::clone(&stop);
        let session = Arc::clone(&session);
        let microphone = Arc::clone(&microphone);
        std::thread::spawn(move || {
            loopback::stream_opus(session, stop, audio_tx, Some(microphone))
        })
    };

    // Audio keeps its own fixed rate whatever the video does. It is a small
    // fraction of the traffic, and a voice that stays intelligible while the
    // picture softens is the right way round.
    let packet_duration = Duration::from_millis(20);
    let mut ticker = tokio::time::interval(Duration::from_millis(200));
    let mut control = tokio::time::interval(CONTROL_INTERVAL);
    let mut was_paused = false;

    let result = loop {
        tokio::select! {
            Some((au, ts)) = video_rx.recv() => {
                // While paused the channels are still drained, so capture does
                // not block behind a full queue, the frames are simply not
                // sent, and the viewer holds the last picture it decoded.
                if !session.paused() {
                    let len = au.len();
                    // The frame duration follows whatever cadence the rate
                    // controller settled on, because it is what the RTP
                    // timestamps are derived from, leaving it at the original
                    // 60ths of a second while sending 30 would tell the viewer
                    // to play everything at double speed.
                    let frame_duration =
                        Duration::from_micros(1_000_000 / quality.get().fps.max(1) as u64);
                    if let Err(e) = webrtc.send_video(&au, ts, frame_duration).await {
                        break Err(e);
                    }
                    session.note_video(len);
                }
            }
            Some(packet) = audio_rx.recv() => {
                if !session.paused() {
                    if let Err(e) = webrtc
                        .send_audio(&packet.data, packet.timestamp_us, packet_duration)
                        .await
                    {
                        break Err(e);
                    }
                    session.note_audio();
                }
            }
            _ = control.tick() => {
                // Everything the viewer has said about the last second, turned
                // into one decision about the next one.
                let feedback = webrtc.viewer_feedback();
                let target = controller.update(&feedback);
                quality.set(target);
                trace_rate(&feedback, target);
            }
            _ = ticker.tick() => {
                if session.should_stop() {
                    break Ok(());
                }

                // A connection that has failed or closed will never carry
                // another frame, and without this the loop would keep
                // encoding into it while the window still said "live".
                if webrtc.lost() {
                    break Err("the viewer's connection dropped".to_owned());
                }

                // Resuming needs a fresh IDR: every frame dropped while paused
                // was a reference some later frame depends on, so without one
                // the viewer decodes garbage until the next keyframe.
                let paused = session.paused();
                if was_paused && !paused {
                    resume_keyframe.request();
                }
                was_paused = paused;

                session.set_mic_peak(if microphone.is_on() {
                    microphone.take_peak()
                } else {
                    0.0
                });
            }
            else => break Ok(()),
        }
    };

    stop.store(true, Ordering::Relaxed);

    // Dropped before the joins below, and that ordering is the whole point.
    //
    // Both threads check `stop` at the top of their loop, but a thread parked
    // inside `blocking_send` on a full queue never reaches the top of its loop
    // again. The queues are small and this loop has just stopped draining
    // them, so that is the *likely* state at this moment, not an unlucky one.
    // Closing the receiving ends turns the block into the send error both
    // threads already treat as "shutting down".
    //
    // Without it, `join` waits for a thread that is waiting for us, and the
    // process survives its own window: no UI, no stream, still running, still
    // holding the capture and the encoder session. Every leftover `sideband`
    // in Task Manager came from here.
    drop(video_rx);
    drop(audio_rx);

    let _ = video_thread.join();

    // The audio thread's result is read, not discarded. It ending early is
    // survivable, the stream keeps its picture, but it is the exact shape of
    // "the viewer says they cannot hear anything" and it has to leave a trace
    // somewhere rather than being thrown away here.
    if let Ok(Err(e)) = audio_thread.join() {
        session.note(format!("audio stopped: {e}"));
    }

    if result.is_ok() {
        session.set_phase(Phase::Ended);
    }
    result
}

/// Opens a capture for one process, and describes it for the read-out.
fn open_source(pid: u32, session: &Session) -> Result<capture::WindowCapture, String> {
    let src = sources::list()
        .map_err(|e| format!("could not enumerate windows: {e}"))?
        .into_iter()
        .find(|s| s.pid == pid)
        .ok_or("that application no longer has a window")?;

    let cap = capture::WindowCapture::start(src.hwnd)
        .map_err(|e| format!("could not capture that window: {e}"))?;

    session.set_source(&src.exe, &src.title);
    Ok(cap)
}

fn video_loop(
    keyframe: net::KeyframeSignal,
    stop: Arc<AtomicBool>,
    tx: mpsc::Sender<(Vec<u8>, u64)>,
    session: Arc<Session>,
    quality: bwe::Quality,
) -> Result<(), String> {
    unsafe {
        windows::Win32::System::Com::CoInitializeEx(
            None,
            windows::Win32::System::Com::COINIT_MULTITHREADED,
        )
        .ok()
        .map_err(|e| format!("CoInitializeEx failed: {e}"))?;
    }

    // The first source has to work. A session that opens with nothing on
    // screen is a failure to report, not a state to recover from, unlike
    // every later source change, which is.
    let mut showing = session.selected_source();
    let mut cap = Some(open_source(showing, &session)?);

    let clock = pipeline::MediaClock::start();
    let mut pacer: pipeline::Pacer<windows::Win32::Graphics::Direct3D11::ID3D11Texture2D> =
        pipeline::Pacer::new(FPS);
    let mut enc: Option<encoder::NvencEncoder> = None;
    let mut dims: (u32, u32) = (0, 0);
    let mut last_idr = Instant::now();

    // The target the encoder was last set to. Compared against rather than
    // asking the encoder every frame, so a driver that refuses a rate change
    // is not asked again until the controller actually wants something else.
    let mut acted_on = quality.get();

    while !stop.load(Ordering::Relaxed) {
        // Someone picked a different application, or the one we had went
        // away. Either way the answer is the same: open the one that is
        // wanted now.
        let wanted = session.selected_source();
        if wanted != showing || cap.is_none() {
            match open_source(wanted, &session) {
                Ok(next) => {
                    // Order matters. The encoder holds texture registrations
                    // against the old capture's D3D11 device, so it goes
                    // first; replacing `cap` is what releases that device.
                    drop(enc.take());
                    cap = Some(next);

                    // Forcing a mismatch is what guarantees the encoder is
                    // rebuilt. Two applications can easily be the same size,
                    // and an encoder kept across the swap would be handed
                    // textures belonging to a device it was never opened
                    // against.
                    dims = (0, 0);
                    pacer.reset();
                    showing = wanted;
                }
                Err(e) => {
                    // Only worth saying once, when it was actually asked for.
                    // The retry below runs several times a second.
                    if wanted != showing {
                        session.note(e);
                        if cap.is_some() {
                            // There is still a working source to stay on, so
                            // put the choice back, the picker showing an
                            // application that is not on screen would be a lie.
                            session.select_source(showing);
                        } else {
                            showing = wanted;
                        }
                    }
                    std::thread::sleep(Duration::from_millis(200));
                    continue;
                }
            }
        }

        let Some(source) = cap.as_ref() else { continue };

        let fresh = match source.next_texture() {
            Ok(fresh) => fresh,
            Err(e) => {
                // The window closed, or the app did. This used to end the
                // stream; now that another application can be chosen without
                // reconnecting, holding the session open is the useful thing
                // to do, the viewer keeps the last frame until there is
                // something new to show them.
                session.note(format!("that window is gone ({e}) - pick another application"));
                drop(enc.take());
                cap = None;
                continue;
            }
        };

        // Rebuild on any geometry change. Dropping the old session first
        // matters: consumer cards cap concurrent NVENC sessions, so holding
        // two open across the swap can fail on the third or fourth resize.
        if let Some((_, w, h)) = &fresh {
            if (*w, *h) != dims {
                drop(enc.take());
                acted_on = quality.get();
                let fresh_enc = encoder::NvencEncoder::new(
                    source.device(),
                    *w,
                    *h,
                    acted_on.fps,
                    acted_on.bitrate,
                )?;
                session.set_quality(fresh_enc.bitrate(), fresh_enc.fps());
                enc = Some(fresh_enc);
                // The cached frame is the old size and must not reach the
                // freshly-sized encoder.
                pacer.reset();
                pacer.set_fps(acted_on.fps);
                dims = (*w, *h);
                session.set_resolution(*w, *h);
            }
        }

        // Retune in place when the controller has moved. Rebuilding the
        // session instead would cost a keyframe at exactly the moment the
        // link is least able to carry one.
        let wanted_rate = quality.get();
        if wanted_rate != acted_on {
            if let Some(e) = enc.as_mut() {
                // A driver refusing a rate change is not a reason to end a
                // working stream; it stays where it was, and `bitrate` and
                // `fps` keep reporting the truth for the read-out.
                let _ = e.reconfigure(wanted_rate.bitrate, wanted_rate.fps);
                pacer.set_fps(e.fps());
                session.set_quality(e.bitrate(), e.fps());
                acted_on = wanted_rate;
            }
        }

        let fresh_tex = fresh.map(|(t, _, _)| t);

        if let Some(paced) = pacer.tick_at(clock.now_us(), fresh_tex) {
            if let Some(e) = enc.as_mut() {
                // Serviced here rather than inside the encoder so the IDR
                // lands on a real frame boundary.
                if keyframe.take() || KEYFRAME_INTERVAL.is_some_and(|iv| last_idr.elapsed() >= iv) {
                    e.request_keyframe();
                    last_idr = Instant::now();
                }
                if let Some(au) = e.encode(&paced.frame, paced.timestamp_us)? {
                    if tx.blocking_send((au, paced.timestamp_us)).is_err() {
                        return Ok(()); // receiver gone; shutting down
                    }
                }
            }
        }

        std::thread::sleep(Duration::from_millis(1));
    }

    Ok(())
}

/// Best guess at the address a viewer on the same network should use.
fn local_address() -> String {
    use std::net::UdpSocket;
    // Nothing is actually sent. Connecting a UDP socket just asks the routing
    // table which local interface would be used to reach the outside world,
    // which is the one a viewer on the LAN can also reach.
    UdpSocket::bind("0.0.0.0:0")
        .and_then(|s| {
            s.connect("8.8.8.8:80")?;
            s.local_addr()
        })
        .map(|a| a.ip().to_string())
        .unwrap_or_else(|_| "localhost".to_owned())
}

#[cfg(test)]
mod tests {
    use super::describe_viewer;

    #[test]
    fn the_public_address_is_preferred() {
        // Built by joining, rather than one literal with line continuations:
        // those swallow the newlines and leave a single unparseable line.
        let sdp = [
            "v=0",
            "a=candidate:1 1 udp 2113937151 192.168.1.50 60040 typ host generation 0",
            "a=candidate:2 1 udp 1677729535 203.0.113.9 60040 typ srflx raddr 0.0.0.0 rport 0",
        ]
        .join("
");

        // The public address is the one that tells the host whether this is
        // the person they sent a code to, so a local one must not win.
        assert_eq!(describe_viewer(&sdp), "203.0.113.9");
    }

    #[test]
    fn a_host_candidate_is_used_when_there_is_nothing_better() {
        let sdp = "a=candidate:1 1 udp 2113937151 192.168.1.50 60040 typ host generation 0
";
        assert_eq!(describe_viewer(sdp), "192.168.1.50");
    }

    #[test]
    fn mdns_names_are_not_shown_as_an_address() {
        // Browsers hide local addresses behind a random .local name; printing
        // one would be noise dressed up as information.
        let sdp = "a=candidate:1 1 udp 2113937151 abc-def.local 60040 typ host generation 0
";
        assert_eq!(describe_viewer(sdp), "address not shared");
    }

    #[test]
    fn malformed_or_empty_input_is_survivable() {
        assert_eq!(describe_viewer(""), "address not shared");
        assert_eq!(describe_viewer("a=candidate:broken"), "address not shared");
    }

    #[test]
    fn local_address_is_usable_or_falls_back() {
        // Either a real interface address, or localhost, never empty, since
        // it goes straight into a link a person has to type.
        let a = super::local_address();
        assert!(!a.is_empty());
        assert!(a == "localhost" || a.contains('.') || a.contains(':'));
    }
}
