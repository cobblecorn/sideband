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
use crate::{bwe, capture, encoder, hotkey, loopback, mic, net, pipeline, scale, server, sources};

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
/// Loss recovery does not depend on this, and no longer depends on being
/// lucky either. Two mechanisms cover it:
///
///   * NACK, from the default interceptors, which buffer outgoing RTP and
///     retransmit on request. This repairs the ordinary case.
///   * Rolling intra refresh in the encoder, which repairs everything else.
///     See `encoder::REFRESH_PERIOD`. A band of intra coded macroblocks
///     sweeps the picture every couple of seconds, so a decoder in any state,
///     however badly broken, converges on a correct picture within one cycle
///     without a keyframe existing at all.
///
/// Measured with 15% of frames deliberately discarded, sustained: 25.5 fps
/// decoded out of 30 sent, zero freezes, zero picture-loss requests, and one
/// keyframe in the whole session, the one at the start. Before the refresh
/// was turned on, that same loss broke the reference chain for good.
///
/// So this staying `None` is now a decision rather than a regret.
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

    // Waiting quietly for ever is the wrong thing to do here.
    //
    // Everything on this side can be perfectly healthy and the viewer still
    // never arrive, because sharing on your own network means an *inbound*
    // connection and Windows blocks those for programs it has no firewall rule
    // for. It blocks them silently: the server is listening, the link is
    // correct, and the other device simply cannot reach this one. The only
    // symptom is this wait never ending, which looks like the program is
    // broken rather than like a rule is missing.
    //
    // Worse, the rule is per executable path, so running the installed copy
    // after having previously run one from a build directory loses it with no
    // sign that anything changed.
    let answer = {
        let session = Arc::clone(&session);
        let mut hint = tokio::time::interval(UNREACHED_HINT);
        hint.tick().await; // fires immediately; the first tick is now
        loop {
            tokio::select! {
                answer = answer_rx.recv() => break answer.ok_or("signalling closed")?,
                _ = hint.tick() => {
                    session.note(
                        "nobody has reached this machine yet. If they are on your network,                          Windows Firewall may be blocking Sideband: run install.ps1 from an                          administrator PowerShell to add the rule."
                            .to_owned(),
                    );
                }
            }
        }
    };

    // An unanswered prompt puts the link back to waiting rather than ending
    // the session. The peer connection has not taken this answer, so a later
    // one is still perfectly acceptable to it, and the link on screen keeps
    // working. See `APPROVAL_TIMEOUT` for why silence is not a refusal.
    let answer = {
        let mut answer = answer;
        loop {
            match approved(&answer, &session).await {
                Decision::Allowed => break answer,
                Decision::Refused => return Err("you turned that viewer away".into()),
                Decision::Unanswered => {
                    session.note(
                        "nobody answered the prompt here, so that viewer was not let in.                          The same link still works: turn on auto admit if you are the one                          at the other end."
                            .to_owned(),
                    );
                    session.set_phase(Phase::Waiting {
                        code: None,
                        link: format!("http://{}:{port}/v/{secret}", local_address()),
                    });
                    answer = answer_rx.recv().await.ok_or("signalling closed")?;
                }
            }
        }
    };

    session.preparing("connecting");
    webrtc.accept_answer(&answer).await?;
    tokio::time::timeout(CONNECT_TIMEOUT, webrtc.wait_connected())
        .await
        .map_err(|_| "the viewer answered but never connected".to_string())?;

    session.set_phase(Phase::Live);
    pump(&webrtc, keyframe, session).await
}

/// How long a capture that has never produced a frame is given before it is
/// thrown away and opened again.
///
/// Long enough that a window which is merely slow to draw its first frame is
/// not disturbed, short enough that restoring a minimised window feels like it
/// worked rather than like it eventually recovered.
const CAPTURE_RETRY_AFTER: Duration = Duration::from_secs(2);

/// How long a live session may produce no video at all before saying so.
const NO_VIDEO_HINT: Duration = Duration::from_secs(8);

/// How long to wait for a viewer on the local network before suggesting the
/// thing that is usually wrong.
///
/// Long enough not to nag someone who is still reading the link out, short
/// enough to save an evening.
const UNREACHED_HINT: Duration = Duration::from_secs(40);

/// How long to give a viewer to actually connect after they answer. Past
/// this the attempt is written off and a fresh offer goes up under the same
/// code, so their retry has something current to answer.
const CONNECT_TIMEOUT: Duration = Duration::from_secs(25);

/// How many failed viewer attempts in a row before giving up.
///
/// Counted in a row, and reset by anything that works, so a session left open
/// all evening is not spending a budget it set aside at the start.
const MAX_ATTEMPTS: usize = 5;

/// How long the approval prompt waits before giving up on an answer.
///
/// Silence here is not a no. It used to be, and it ended the session, which is
/// the wrong reading of by far the commonest case: one person with two devices,
/// standing at the second one, with the prompt sitting on the first. Nobody was
/// refused, nobody was there. The offer goes up again instead, under the same
/// code, so walking back and pressing the button actually works.
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

/// Admits one viewer: a fresh offer, published under the same code, and the
/// answer to it.
///
/// One connection per viewer is not a choice, it is how WebRTC works: a peer
/// connection takes exactly one answer. Publishing a new offer also re-arms
/// the code, so the next person to open the link finds it working.
async fn admit_one(
    relay: &str,
    ticket: &Ticket,
    link: &str,
    session: &Arc<Session>,
    watching: usize,
//
// `use<>` captures nothing: without it the returned connection borrows every
// argument for its whole life, which makes it unable to leave the task that
// built it, and admitting viewers happens on a task of its own.
) -> Result<net::Session<impl webrtc::peer_connection::PeerConnection + use<>>, String> {
    let webrtc = net::connect(net::ice_servers()).await?;

    session.preparing("gathering network candidates");
    let offer = webrtc.offer().await?;

    session.preparing("publishing to the relay");
    put_offer(relay, ticket, offer).await?;

    // The read-out only goes back to "waiting" when nobody is watching yet.
    // With somebody already connected the session is live and stays live; a
    // second person arriving must not make the window look like it dropped
    // the first.
    if watching == 0 {
        session.set_phase(Phase::Waiting {
            code: Some(ticket.code.clone()),
            link: link.to_owned(),
        });
    }

    let answer = poll_answer(relay, ticket, session).await?;

    match approved(&answer, session).await {
        Decision::Allowed => {}
        Decision::Refused => {
            webrtc.close().await;
            return Err("you turned that viewer away".into());
        }
        Decision::Unanswered => {
            webrtc.close().await;
            session.note(
                "nobody answered the prompt here, so that viewer was not let in. \
                 The same code still works: turn on auto admit if you are the one \
                 at the other end."
                    .to_owned(),
            );
            return Err("nobody answered".into());
        }
    }

    if watching == 0 {
        session.preparing("connecting");
    }
    webrtc.accept_answer(&answer).await?;

    match tokio::time::timeout(CONNECT_TIMEOUT, webrtc.wait_connected()).await {
        Ok(()) => Ok(webrtc),
        Err(_) => {
            webrtc.close().await;
            Err("that viewer answered but never connected".into())
        }
    }
}

/// Shares to everyone who opens the link, for as long as this is running.
///
/// Admitting somebody and admitting somebody *else* are the same operation, so
/// the code keeps working rather than being spent on whoever got there first.
async fn relay_attempts(
    relay: &str,
    ticket: &Ticket,
    link: &str,
    session: Arc<Session>,
) -> Result<(), String> {
    let mut watching: tokio::task::JoinSet<Result<(), String>> = tokio::task::JoinSet::new();
    let mut failures = 0usize;

    loop {
        if session.should_stop() {
            break;
        }

        // Read before the select, because the arms below borrow the set.
        let already_watching = watching.len();

        tokio::select! {
            // Somebody left. With nobody watching this goes back to offering
            // and waiting; with others still connected it changes nothing.
            Some(finished) = watching.join_next(), if !watching.is_empty() => {
                if let Ok(Err(e)) = finished {
                    session.note(format!("{e}. The same code still works."));
                }
                if watching.is_empty() && !session.should_stop() {
                    session.set_phase(Phase::Waiting {
                        code: Some(ticket.code.clone()),
                        link: link.to_owned(),
                    });
                }
            }

            admitted = admit_one(relay, ticket, link, &session, already_watching) => {
                match admitted {
                    Ok(viewer) => {
                        failures = 0;
                        if already_watching > 0 {
                            session.note("somebody else is watching too.".to_owned());
                        }
                        session.set_phase(Phase::Live);

                        let session = Arc::clone(&session);
                        watching.spawn(async move {
                            let keyframe = viewer.keyframe_signal();
                            let result = pump(&viewer, keyframe, session).await;
                            viewer.close().await;
                            result
                        });
                    }
                    Err(e) if e == "you turned that viewer away" => {
                        if watching.is_empty() {
                            return Err(e);
                        }
                    }
                    Err(_) => {
                        // A failed admission leaves the code alive, so this
                        // goes round again and offers afresh. The count only
                        // gives up when nobody is watching at all.
                        if watching.is_empty() {
                            failures += 1;
                            if failures >= MAX_ATTEMPTS {
                                return Err(
                                    "the viewer could not connect after several attempts".into(),
                                );
                            }
                        }
                    }
                }
            }
        }
    }

    watching.abort_all();
    session.set_phase(Phase::Ended);
    Ok(())
}

/// What the relay hands back when a session is created.
#[derive(Clone)]
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
/// What the host said about a viewer, and the difference that matters.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Decision {
    /// Let in, by a person or by the setting.
    Allowed,
    /// Actively turned away. A person decided this, so it stands.
    Refused,
    /// Nobody answered. That is not a decision, and must not be treated as
    /// one: see `APPROVAL_TIMEOUT`.
    Unanswered,
}

async fn approved(answer: &str, session: &Arc<Session>) -> Decision {
    // Asked for explicitly, so there is nobody to ask. The viewer is still
    // named in the read-out rather than let in silently: not having to answer
    // is the point, not being unable to see who arrived.
    if session.auto_approve() {
        session.note(format!("let {} in without asking", describe_viewer(answer)));
        return Decision::Allowed;
    }

    session.request_approval(&describe_viewer(answer));

    let deadline = Instant::now() + APPROVAL_TIMEOUT;
    while Instant::now() < deadline {
        if session.should_stop() {
            return Decision::Refused;
        }
        if let Some(decision) = session.approval_decision() {
            return if decision { Decision::Allowed } else { Decision::Refused };
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    Decision::Unanswered
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

/// Deliberately throws video away, to see what the viewer does about it.
///
/// `SIDEBAND_DROP` is a percentage, and while it is set that share of encoded
/// frames is encoded and then not sent. Losing whole access units is a harsher
/// version of what a bad link does to a stream, and it is the only way to
/// answer the question this pipeline actually turns on: whether a viewer whose
/// reference chain has been broken ever gets a correct picture back.
///
/// It is a test facility rather than a feature, but it lives here rather than
/// in a branch, because "does it recover" is a question worth being able to
/// ask again on any future change.
struct Sabotage {
    percent: u32,
    counter: u64,
}

impl Sabotage {
    fn from_env() -> Self {
        let percent = std::env::var("SIDEBAND_DROP")
            .ok()
            .and_then(|v| v.trim().parse::<u32>().ok())
            .unwrap_or(0)
            .min(100);
        if percent > 0 {
            eprintln!("  sabotage: dropping {percent}% of video frames");
        }
        Self { percent, counter: 0 }
    }

    /// Deterministic rather than random, so two runs are comparable.
    fn should_drop(&mut self) -> bool {
        if self.percent == 0 {
            return false;
        }
        self.counter += 1;
        (self.counter * self.percent as u64) % 100 < self.percent as u64
    }
}

/// Capture, encode and send until the connection ends or a stop is requested.
/// The capture, encode and audio pipeline, and the threads running it.
///
/// Pulled out of `pump` because there are two callers now: one viewer served
/// locally, and any number of them through a relay. Only one of these exists
/// per session however many people are watching, which is the whole point of
/// the exercise: the picture is captured once, encoded once, and the same
/// bytes go to everybody.
struct Media {
    stop: Arc<AtomicBool>,
    quality: bwe::Quality,
    controller: bwe::Controller,
    video_rx: mpsc::Receiver<(Vec<u8>, u64)>,
    audio_rx: mpsc::Receiver<OpusPacket>,
    video_thread: std::thread::JoinHandle<Result<(), String>>,
    audio_thread: std::thread::JoinHandle<Result<(), String>>,
    microphone: Arc<mic::Mic>,
    resume_keyframe: net::KeyframeSignal,
}

impl Media {
    fn start(keyframe: net::KeyframeSignal, session: &Arc<Session>) -> Self {
        // One clone stays here to force a refresh on resume; the other is
        // moved into the encode thread, which services the request.
        let resume_keyframe = keyframe.clone();
        let stop = Arc::new(AtomicBool::new(false));

        // Nothing is known about the viewer's connection yet, so the stream
        // opens at a rate almost any home link can carry and earns its way up
        // from there. Sending the ceiling first and waiting to be told to stop
        // is how the picture freezes within seconds of connecting.
        let controller = bwe::Controller::new(bwe::START_BITRATE, FPS);
        let quality = bwe::Quality::new(controller.target());

        // Whichever device was chosen in the window, or the default if that
        // one has since been unplugged.
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
            let ui = Arc::clone(session);
            hotkey::spawn_toggle(microphone.handle(), move |on| ui.set_mic_on(on));
        }

        // Small queues on purpose: they exist to smooth jitter, not to buffer.
        let (video_tx, video_rx) = mpsc::channel::<(Vec<u8>, u64)>(4);
        let (audio_tx, audio_rx) = mpsc::channel::<OpusPacket>(32);


        let video_thread = {
            let stop = Arc::clone(&stop);
            let session = Arc::clone(session);
            let quality = quality.clone();
            std::thread::spawn(move || video_loop(keyframe, stop, video_tx, session, quality))
        };
        let audio_thread = {
            let stop = Arc::clone(&stop);
            let session = Arc::clone(session);
            let microphone = Arc::clone(&microphone);
            std::thread::spawn(move || {
                loopback::stream_opus(session, stop, audio_tx, Some(microphone))
            })
        };

        Self {
            stop,
            quality,
            controller,
            video_rx,
            audio_rx,
            video_thread,
            audio_thread,
            microphone,
            resume_keyframe,
        }
    }

    /// Stops the threads and reports anything they died of.
    ///
    /// The receiving ends are dropped first, and that ordering is the whole
    /// point: a thread parked in `blocking_send` on a full queue never reaches
    /// the top of its loop to see the stop flag, so joining without this waits
    /// for a thread that is waiting for us.
    fn finish(self, session: &Session) {
        self.stop.store(true, Ordering::Relaxed);
        drop(self.video_rx);
        drop(self.audio_rx);

        if let Ok(Err(e)) = self.video_thread.join() {
            session.note(format!("video stopped: {e}"));
        }
        if let Ok(Err(e)) = self.audio_thread.join() {
            session.note(format!("audio stopped: {e}"));
        }
    }
}

/// One viewer, served until it goes away.
///
/// A pipeline each, and that is not the obvious design. Capturing and encoding
/// once and sending the same bytes to everybody is cheaper and is what was
/// tried first, and it cannot work here: a viewer arriving late has no
/// reference frame, the only thing that gives them one is an IDR, and a forced
/// IDR does not survive this pipeline. Measured with two watching, the moment
/// one was sent for the newcomer's benefit, *both* viewers went to zero frames
/// a second and stayed there asking for pictures.
///
/// A fresh encoder session opens with an IDR and its own parameter sets, which
/// is the one case known to work, because it is what every first viewer has
/// always got. So everybody is a first viewer.
///
/// The cost is real: an encoder session and an encode pass per viewer. It buys
/// a second person who can actually see something, and a rate that follows
/// their connection rather than the worst one in the room.
async fn pump<P: webrtc::peer_connection::PeerConnection>(
    webrtc: &net::Session<P>,
    keyframe: net::KeyframeSignal,
    session: Arc<Session>,
) -> Result<(), String> {
    let mut media = Media::start(keyframe, &session);
    let mut viewer = Viewer::new(webrtc);

    // Nothing ever arrives on this: the local path serves the one viewer it
    // was given. Holding the sender keeps the channel open so the receiver
    // simply never fires, rather than closing and ending the loop.
    let (_never, mut nobody) = mpsc::channel::<()>(1);
    let result = match run_media(
        &mut media,
        std::slice::from_mut(&mut viewer),
        &session,
        &mut nobody,
    )
    .await
    {
        Served::Ended(result) => result,
        Served::Joined(()) => Ok(()),
    };

    media.finish(&session);
    if result.is_ok() {
        session.set_phase(Phase::Ended);
    }
    result
}

/// One person watching.
struct Viewer<'a, P: webrtc::peer_connection::PeerConnection> {
    net: &'a net::Session<P>,
}

impl<'a, P: webrtc::peer_connection::PeerConnection> Viewer<'a, P> {
    fn new(net: &'a net::Session<P>) -> Self {
        Self { net }
    }
}

/// Sends what the pipeline produces to everyone watching, until nobody is.
///
/// The viewers are a slice rather than one connection because the picture is
/// captured once, encoded once, and the same bytes go to all of them. Adding a
/// second person watching costs the upload and nothing else.
async fn run_media<P: webrtc::peer_connection::PeerConnection, N>(
    media: &mut Media,
    viewers: &mut [Viewer<'_, P>],
    session: &Arc<Session>,
    joining: &mut mpsc::Receiver<N>,
) -> Served<N> {
    let packet_duration = Duration::from_millis(20);
    let mut sabotage = Sabotage::from_env();
    let started = Instant::now();
    let mut said_no_video = false;
    let mut ticker = tokio::time::interval(Duration::from_millis(200));
    let mut control = tokio::time::interval(CONTROL_INTERVAL);
    let mut was_paused = false;

    loop {
        tokio::select! {
            Some((au, ts)) = media.video_rx.recv() => {
                // While paused the channels are still drained, so capture does
                // not block behind a full queue, the frames are simply not
                // sent, and the viewer holds the last picture it decoded.
                if !session.paused() && !sabotage.should_drop() {
                    let len = au.len();
                    // The frame duration follows whatever cadence the rate
                    // controller settled on, because it is what the RTP
                    // timestamps are derived from: leaving it at 60ths of a
                    // second while sending 30 tells the viewer to play
                    // everything at double speed.
                    let frame_duration =
                        Duration::from_micros(1_000_000 / media.quality.get().fps.max(1) as u64);

                    for viewer in viewers.iter() {
                        // A send failing is that viewer's problem rather than
                        // everyone's, so the rest carry on.
                        let _ = viewer.net.send_video(&au, ts, frame_duration).await;
                    }
                    session.note_video(len);
                }
            }
            Some(packet) = media.audio_rx.recv() => {
                if !session.paused() {
                    for viewer in viewers.iter() {
                        let _ = viewer
                            .net
                            .send_audio(&packet.data, packet.timestamp_us, packet_duration)
                            .await;
                    }
                    session.note_audio();
                }
            }
            _ = control.tick() => {
                // Everyone's feedback, reduced to the worst of it.
                //
                // The stream is one stream, so it has to suit the viewer
                // having the hardest time. Taking the best, or the first, would
                // mean the others are sent more than their connection can
                // carry, and the whole reason this rate control exists is that
                // doing so does not degrade, it freezes.
                let feedback = viewers
                    .iter()
                    .map(|v| v.net.viewer_feedback())
                    .reduce(bwe::Feedback::worst_of)
                    .unwrap_or_default();
                let target = media.controller.update(&feedback);
                media.quality.set(target);
                trace_rate(&feedback, target);
            }
            // Somebody else has joined. The borrow of the viewer list has to
            // end before it can be added to, so this hands back and is called
            // again with the newcomer included.
            Some(arrival) = joining.recv() => {
                break Served::Joined(arrival);
            }
            _ = ticker.tick() => {
                if session.should_stop() {
                    break Served::Ended(Ok(()));
                }

                // A picture that never starts is not the same as one that
                // stopped, and neither is visible from here without saying so.
                // Window capture delivers nothing at all while its window is
                // minimised, which is the ordinary explanation and not one
                // anybody guesses while staring at a blank viewer.
                if !said_no_video && session.counters().0 == 0 && started.elapsed() >= NO_VIDEO_HINT
                {
                    said_no_video = true;
                    session.note(
                        "no video yet. That window may be minimised: capture delivers \
                         nothing while it is, so restore it or pick another application."
                            .to_owned(),
                    );
                }

                // Everyone has gone. A connection that has failed or closed
                // will never carry another frame, and without this the loop
                // would keep encoding into it while the window said "live".
                if !viewers.is_empty() && viewers.iter().all(|v| v.net.lost()) {
                    break Served::Ended(Err("the viewer's connection dropped".to_owned()));
                }

                // Resuming needs a fresh refresh: every frame dropped while
                // paused was a reference some later frame depends on.
                let paused = session.paused();
                if was_paused && !paused {
                    media.resume_keyframe.request();
                }
                was_paused = paused;

                session.set_mic_peak(if media.microphone.is_on() {
                    media.microphone.take_peak()
                } else {
                    0.0
                });
            }
            else => break Served::Ended(Ok(())),
        }
    }
}

/// Why `run_media` handed control back.
enum Served<N> {
    /// Another viewer is waiting to be added to the list.
    Joined(N),
    /// Nobody is watching any more, or the host stopped.
    Ended(Result<(), String>),
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

    // The first source is opened by the same retry the loop uses for every
    // later one, rather than being required to work first time.
    //
    // It used to be required, on the reasoning that a session opening with
    // nothing on screen is a failure to report rather than a state to recover
    // from. That reasoning had a hole in it: a *minimised* window cannot be
    // captured at all, so opening one killed this thread outright, and the
    // session carried on with audio flowing beside a picture that never
    // arrived and nothing anywhere saying why. Restoring the window did not
    // help, because there was no longer a thread to notice.
    //
    // Now it retries, says so once, and starts sending the moment the window
    // is restored.
    let mut showing = session.selected_source();
    let mut cap: Option<capture::WindowCapture> = None;
    // Whether the current failure to open has already been reported. The
    // retry below runs several times a second and must not narrate that.
    let mut announced = false;
    // Whether this capture has ever produced a frame, and when it was opened.
    //
    // These exist to tell two identical-looking situations apart. A capture
    // that has delivered frames and then stops is an ordinary still window,
    // and disturbing it would throw away a working session for nothing. A
    // capture that has *never* delivered one is broken, and the only known
    // cure is a fresh one.
    let mut ever_framed = false;
    let mut opened_at = Instant::now();

    let clock = pipeline::MediaClock::start();
    let mut pacer: pipeline::Pacer<windows::Win32::Graphics::Direct3D11::ID3D11Texture2D> =
        pipeline::Pacer::new(FPS);
    let mut enc: Option<encoder::NvencEncoder> = None;
    let mut dims: (u32, u32) = (0, 0);
    // Shrinks the picture when the controller says to. Nothing here decides
    // how much: that is settled once per session in `bwe`, for the reason in
    // its `divisor_for`. At full size this is never even constructed, so a
    // fast link pays nothing for the existence of a slow one.
    let mut scaler: Option<scale::Scaler> = None;
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
                    announced = false;
                    ever_framed = false;
                    opened_at = Instant::now();

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
                    // Said once per failure, and said whether this was a
                    // switch or the opening attempt. Staying quiet about the
                    // opening one is what made a minimised window look like a
                    // program that had simply stopped working.
                    if !announced {
                        session.note(format!(
                            "{e}. A minimised window cannot be captured: restore it,                              or pick another application."
                        ));
                        announced = true;
                    }
                    if wanted != showing && cap.is_some() {
                        // There is still a working source to stay on, so put
                        // the choice back, the picker showing an application
                        // that is not on screen would be a lie.
                        session.select_source(showing);
                    } else {
                        showing = wanted;
                    }
                    std::thread::sleep(Duration::from_millis(200));
                    continue;
                }
            }
        }

        // A capture that has never produced anything is reopened.
        //
        // Opening a capture over a minimised window *succeeds*. It returns a
        // perfectly ordinary object whose session then produces nothing, ever,
        // and it does not start producing when the window is restored either,
        // because the session was wound up around a window that had no surface
        // at the time. Nothing about it reports a fault: no error, no closed
        // event, and the item keeps reporting the same size throughout, so
        // even rebuilding the frame pool underneath it changes nothing.
        //
        // Measured: audio flowing normally beside a picture that never
        // started, staying that way after the window was restored, with the
        // only symptom a viewer looking at nothing. A fresh capture fixes it
        // immediately, so that is what happens.
        if cap.is_some() && !ever_framed && opened_at.elapsed() >= CAPTURE_RETRY_AFTER {
            drop(enc.take());
            cap = None;
            dims = (0, 0);
            pacer.reset();
            opened_at = Instant::now();
            continue;
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

        // Fit the picture to the link as well as the bitrate.
        //
        // Deliberately upstream of the geometry check below: shrinking changes
        // the dimensions the encoder sees, and that check already knows how to
        // rebuild for a new size and re-send parameter sets, which is the
        // whole of what a resolution change needs. Doing it here means a
        // resolution change and a window resize are the same event.
        //
        // The size is decided by the controller, once a second, not here and
        // not per frame. This loop runs at the frame rate and the target rate
        // moves continuously, so deciding here meant re-deciding sixty times a
        // second against a number that was never still: the picture visibly
        // grew and shrank as the rate wandered across a threshold. The
        // controller holds the decision until the link has actually moved.
        let wanted = quality.get().divisor;
        let fresh = match fresh {
            Some((texture, w, h)) => {
                if wanted <= 1 {
                    Some((texture, w, h))
                } else {
                    let scaler = scaler.get_or_insert_with(|| {
                        scale::Scaler::new(source.device(), source.context())
                    });
                    match scaler.shrink(&texture, w, h, wanted) {
                        Ok(smaller) => {
                            let (sw, sh) = scale::Scaler::target(w, h, wanted);
                            Some((smaller, sw, sh))
                        }
                        Err(e) => {
                            // Full size is a worse picture than intended, and
                            // a far better outcome than no picture.
                            session.note(format!(
                                "could not shrink the picture ({e}), sending full size"
                            ));
                            Some((texture, w, h))
                        }
                    }
                }
            }
            None => None,
        };

        if fresh.is_some() {
            ever_framed = true;
        }

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
