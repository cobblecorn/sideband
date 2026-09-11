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
use crate::chime::{self, Chime};
use crate::pipeline::VideoEncoder;
use crate::session::{Phase, Session, ViewerInfo};
use crate::settings::Settings;
use crate::{
    bwe, capture, card, encoder, hotkey, loopback, mic, net, pipeline, portmap, scale, server,
    sources,
};

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
    // The hotkeys follow whichever session is current. Registered once for
    // the process, see `hotkey`, and pointed here.
    hotkey::attach(&session);

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
                        "nobody has reached this machine yet. If they are on your network, Windows Firewall may be blocking Sideband: run install.ps1 again and say yes to the Windows prompt."
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
            match approved(&describe_viewer(&answer), &session).await {
                Decision::Allowed => break answer,
                Decision::Refused => return Err("you turned that viewer away".into()),
                Decision::Unanswered => {
                    session.note(
                        "nobody answered the prompt here, so that viewer was not let in. The same link still works: turn on auto admit if you are the one at the other end."
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
    pump(&webrtc, keyframe, session, None).await
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

    // The link handed out is the permanent one whenever the relay keeps one:
    // the same link as last time and the time before, so a bookmark finds
    // this session without anybody sending anything. The code still works
    // on its own, for reading out.
    let room = claim_room(&relay, &ticket.code, &session).await;
    let link = share_link(&relay, &ticket, room.as_ref());
    let mut share = Share { ticket, link, room };

    // Whatever happens from here, the session is torn down rather than left
    // sitting on the relay until it expires, and the permanent link stops
    // pointing at it, so a page open on it says "not live" straight away.
    let outcome = relay_attempts(&relay, &mut share, Arc::clone(&session)).await;
    if let Some(room) = share.room.clone() {
        let relay = relay.clone();
        let _ = tokio::task::spawn_blocking(move || room_clear(&relay, &room)).await;
    }
    destroy_session(&relay, &share.ticket).await;
    outcome
}

/// What is being handed out right now. All of it changes when a new link is
/// asked for.
struct Share {
    ticket: Ticket,
    link: String,
    /// The permanent link, when the relay has one for this machine.
    room: Option<Room>,
}

/// A permanent link, see the relay's `Room`.
#[derive(Clone)]
struct Room {
    name: String,
    /// Proves this machine owns it. Held in the settings file, and only ever
    /// sent to the relay.
    key: String,
}

fn share_link(relay: &str, ticket: &Ticket, room: Option<&Room>) -> String {
    match room {
        Some(r) => format!("{relay}/r/{}", r.name),
        None => format!("{relay}/{}", ticket.code),
    }
}

/// Points this machine's permanent link at `code`, making the link first if
/// there has never been one.
///
/// Failing is not failing to share: the code, and the link made from it,
/// work exactly as they always have, and the read-out says the permanent one
/// is not available this time.
async fn claim_room(relay: &str, code: &str, session: &Arc<Session>) -> Option<Room> {
    let mut settings = Settings::load_stored();
    if settings.ensure_room() {
        settings.save();
    }
    let room = Room { name: settings.room, key: settings.room_key };

    let (r, c, at) = (room.clone(), code.to_owned(), relay.to_owned());
    let published = tokio::task::spawn_blocking(move || room_publish(&at, &r, &c))
        .await
        .unwrap_or_else(|e| Err(format!("{e}")));

    match published {
        Ok(()) => Some(room),
        Err(e) => {
            session.note(format!(
                "the permanent link is not available ({e}), so this link only works while you share"
            ));
            None
        }
    }
}

fn room_publish(relay: &str, room: &Room, code: &str) -> Result<(), String> {
    ureq::put(&format!("{relay}/api/room/{}", room.name))
        .header("Authorization", &format!("Bearer {}", room.key))
        .send_json(serde_json::json!({ "code": code }))
        .map(|_| ())
        .map_err(|e| match e {
            ureq::Error::StatusCode(403) => "the relay says that link belongs to another machine".into(),
            ureq::Error::StatusCode(404) => "the relay needs updating to keep one".into(),
            e => e.to_string(),
        })
}

/// Best effort, as tearing down the session is: the relay forgets a room
/// nobody has shared to in months regardless.
fn room_clear(relay: &str, room: &Room) {
    let _ = ureq::delete(&format!("{relay}/api/room/{}/live", room.name))
        .header("Authorization", &format!("Bearer {}", room.key))
        .call();
}

/// Retires a permanent link for good, so whoever holds it finds nothing
/// there ever again. Blocking; the window calls it from a thread of its own
/// when a new link is asked for between sessions.
pub fn room_forget(relay: &str, name: &str, key: &str) {
    let relay = relay.trim().trim_end_matches('/');
    if relay.is_empty() || name.is_empty() {
        return;
    }
    let _ = ureq::delete(&format!("{relay}/api/room/{name}"))
        .header("Authorization", &format!("Bearer {key}"))
        .call();
}

/// A new code and a new permanent link, the old ones stopped.
///
/// For a link that has been passed on further than it should have been.
/// Everybody already watching stays: they are connected to this machine
/// directly and the link has nothing more to do with them. Removing one of
/// them is what the remove button is for.
async fn new_link(relay: &str, share: &mut Share, session: &Arc<Session>, someone_watching: bool) {
    let fresh = match create_session(relay).await {
        Ok(t) => t,
        Err(e) => {
            session.note(format!("could not make a new code: {e}"));
            return;
        }
    };
    let old = std::mem::replace(&mut share.ticket, fresh);
    destroy_session(relay, &old).await;

    if let Some(room) = share.room.take() {
        let relay = relay.to_owned();
        let _ = tokio::task::spawn_blocking(move || room_forget(&relay, &room.name, &room.key)).await;
    }
    let mut settings = Settings::load_stored();
    settings.new_room();
    settings.save();
    share.room = claim_room(relay, &share.ticket.code, session).await;
    share.link = share_link(relay, &share.ticket, share.room.as_ref());

    session.set_share(Some(share.ticket.code.clone()), share.link.clone());
    if !someone_watching {
        session.set_phase(Phase::Waiting {
            code: Some(share.ticket.code.clone()),
            link: share.link.clone(),
        });
    }
    session.tell("new link and code. The old ones have stopped working; anyone watching stays.");
}

/// Tells the relay whether to turn new viewers away. Best effort: if this
/// does not arrive, nothing new is offered while locked anyway, so the worst
/// case is a viewer waiting rather than being told why.
async fn set_relay_lock(relay: &str, ticket: &Ticket, locked: bool) {
    let url = format!("{relay}/api/session/{}/lock", ticket.code);
    let token = ticket.token.clone();
    let _ = tokio::task::spawn_blocking(move || {
        ureq::put(&url)
            .header("Authorization", &format!("Bearer {token}"))
            .send(if locked { "1" } else { "0" })
    })
    .await;
}

/// Results of admitting somebody that are not failures, and must not count
/// towards giving up. A session left open all evening sees plenty of them.
const INTERRUPTED: &str = "stopped waiting for this viewer";
const TURNED_AWAY: &str = "that viewer was turned away earlier";
const REFUSED: &str = "you turned that viewer away";
const NOBODY_YET: &str = "nobody has joined yet";
const UNANSWERED: &str = "nobody answered";

fn benign(e: &str) -> bool {
    [INTERRUPTED, TURNED_AWAY, REFUSED, NOBODY_YET, UNANSWERED, "cancelled"].contains(&e)
}

/// Why a viewer's send loop ended when they simply went away.
const VIEWER_LEFT: &str = "the viewer's connection dropped";

/// What a viewer's page says about itself, on two lines ahead of its answer.
#[derive(Debug, Default, PartialEq, Eq)]
struct About {
    /// The random name the page keeps in its browser.
    browser: String,
    /// "Android, Chrome" and the like.
    device: String,
}

/// Takes the page's own lines off an answer, leaving the answer.
///
/// They have to come off: they are not SDP, and the answer is handed to the
/// WebRTC stack exactly as the browser wrote it. Cleaned on the way, because
/// anybody holding a code can put anything they like there, and it ends up
/// in the window.
fn split_about(answer: &str) -> (About, String) {
    let clean = |v: &str| -> String {
        v.trim().chars().filter(|c| !c.is_control()).take(40).collect()
    };
    let mut about = About::default();
    let mut sdp = String::with_capacity(answer.len());
    for line in answer.split_inclusive('\n') {
        let t = line.trim();
        if let Some(v) = t.strip_prefix("x-sideband-viewer:") {
            about.browser = clean(v);
        } else if let Some(v) = t.strip_prefix("x-sideband-device:") {
            about.device = clean(v);
        } else {
            sdp.push_str(line);
        }
    }
    (about, sdp)
}

/// Somebody let in, and what comes with them.
struct Admitted<P: webrtc::peer_connection::PeerConnection> {
    net: net::Session<P>,
    /// The port the router opened for them. Held for as long as they watch;
    /// dropping it closes the port.
    opening: Option<portmap::Opening>,
    viewer: ViewerInfo,
}

/// Admits one viewer: a fresh offer, published under the same code, and the
/// answer to it.
///
/// One connection per viewer is not a choice, it is how WebRTC works: a peer
/// connection takes exactly one answer. Publishing a new offer also re-arms
/// the code, so the next person to open the link finds it working.
#[allow(clippy::too_many_arguments)]
async fn admit_one(
    relay: &str,
    ticket: &Ticket,
    link: &str,
    session: &Arc<Session>,
    watching: usize,
    ice: &[net::RTCIceServer],
    router: Option<&Arc<portmap::Router>>,
    id: u64,
//
// `use<>` captures nothing: without it the returned connection borrows every
// argument for its whole life, which makes it unable to leave the task that
// built it, and admitting viewers happens on a task of its own.
) -> Result<Admitted<impl webrtc::peer_connection::PeerConnection + use<>>, String> {
    let webrtc = net::connect(ice.to_vec()).await?;

    // Every way out that is not success closes the connection. One left
    // open keeps its ICE agent answering on a port nobody is using.
    match admit_on(&webrtc, relay, ticket, link, session, watching, router, id).await {
        Ok((opening, viewer)) => Ok(Admitted { net: webrtc, opening, viewer }),
        Err(e) => {
            webrtc.close().await;
            Err(e)
        }
    }
}

#[allow(clippy::too_many_arguments)]
async fn admit_on<P: webrtc::peer_connection::PeerConnection>(
    webrtc: &net::Session<P>,
    relay: &str,
    ticket: &Ticket,
    link: &str,
    session: &Arc<Session>,
    watching: usize,
    router: Option<&Arc<portmap::Router>>,
    id: u64,
) -> Result<(Option<portmap::Opening>, ViewerInfo), String> {
    // The read-out is only narrated while nobody is watching yet.
    //
    // Admitting the next person happens continuously in the background, and
    // it must not describe itself: a session with somebody watching is Live,
    // and saying "publishing to the relay" over the top of that takes the
    // window out of Live and stops the read-out mid-stream, for an errand the
    // person watching has no part in.
    let narrate = watching == 0;

    if narrate {
        session.preparing("gathering network candidates");
    }
    let offer = webrtc.offer().await?;

    // A door through the router for this viewer, when the router will give
    // one, advertised in the offer as one more place to knock. It is what
    // lets in the viewers nothing else can, phones on mobile data above all:
    // see `portmap`.
    let opening = match (router, portmap::host_port(&offer)) {
        (Some(router), Some(port)) => {
            let router = Arc::clone(router);
            match tokio::task::spawn_blocking(move || portmap::open(&router, port)).await {
                Ok(Ok(opening)) => Some(opening),
                Ok(Err(e)) => {
                    eprintln!("  router: {e}");
                    session.set_reach(false, e);
                    None
                }
                Err(_) => None,
            }
        }
        _ => None,
    };
    let offer = match &opening {
        Some(o) => portmap::with_public_candidate(&offer, o.port(), o.public()),
        None => offer,
    };

    if narrate {
        session.preparing("publishing to the relay");
    }
    put_offer(relay, ticket, offer).await?;

    if narrate {
        session.set_phase(Phase::Waiting {
            code: Some(ticket.code.clone()),
            link: link.to_owned(),
        });
    }

    let answer = poll_answer(relay, ticket, session).await?;
    let (about, answer) = split_about(&answer);
    let address = describe_viewer(&answer);
    let viewer = ViewerInfo {
        id,
        device: about.device,
        address: if address == NO_ADDRESS { String::new() } else { address },
        browser: about.browser,
        since: Instant::now(),
    };

    // Removed, or turned away, earlier in this session. Not asked about again
    // and not announced: the point of removing somebody is not hearing from
    // them, and their page keeps retrying.
    if session.is_banned(&viewer.browser, &viewer.address) {
        return Err(TURNED_AWAY.into());
    }

    match approved(&viewer.describe(), session).await {
        Decision::Allowed => {}
        Decision::Refused => {
            // Kept out for the rest of the session, so saying no once is
            // enough. It used to end the whole session instead, which was
            // a fair answer when a code admitted one person, and is not one
            // when others are watching on the same link.
            session.ban(&viewer.browser, &viewer.address);
            return Err(REFUSED.into());
        }
        Decision::Unanswered => {
            session.note(
                "nobody answered the prompt here, so that viewer was not let in. \
                 The same link still works: turn on auto admit if you are the one \
                 at the other end."
                    .to_owned(),
            );
            return Err(UNANSWERED.into());
        }
    }

    if narrate {
        session.preparing("connecting");
    }
    webrtc.accept_answer(&answer).await?;

    match tokio::time::timeout(CONNECT_TIMEOUT, webrtc.wait_connected()).await {
        Ok(()) => Ok((opening, viewer)),
        Err(_) => {
            let routes = viewer_routes(&answer);
            Err(format!(
                "{} answered but never connected. They offered {routes}{}",
                viewer.describe(),
                if net::have_turn() || opening.is_some() {
                    "."
                } else {
                    ", and there is no way through the router for them to fall back on."
                }
            ))
        }
    }
}

/// Shares to everyone who opens the link, for as long as this is running.
///
/// Admitting somebody and admitting somebody *else* are the same operation, so
/// the code keeps working rather than being spent on whoever got there first.
async fn relay_attempts(relay: &str, share: &mut Share, session: Arc<Session>) -> Result<(), String> {
    // Looked for while the relay is asked for its route options, so neither
    // waits on the other. A router that answers does so in milliseconds, and
    // one that does not costs a few seconds here, once, rather than per viewer.
    let router_search = Settings::load()
        .open_ports
        .then(net::local_ip)
        .flatten()
        .map(|ip| tokio::task::spawn_blocking(move || portmap::Router::find(ip)));

    // Asked of the relay once, and shared by every viewer admitted after.
    //
    // The relay is the piece of the setup that is already shared, so putting
    // the TURN credentials there means no machine running this needs any
    // configuration of its own. Anything the relay cannot tell us falls back
    // to whatever is set locally, and then to STUN alone.
    let (ice, have_turn) = match net::ice_from_relay(relay) {
        Some(found) => found,
        None => (net::ice_servers(), net::have_turn()),
    };

    let router = match router_search {
        Some(search) => match search.await {
            Ok(Ok(found)) => {
                session.set_reach(
                    true,
                    format!("router opens a port for each viewer, at {}", found.external()),
                );
                Some(Arc::new(found))
            }
            Ok(Err(why)) => {
                session.set_reach(have_turn, why);
                None
            }
            Err(_) => None,
        },
        None => {
            session.set_reach(have_turn, "opening router ports is turned off");
            None
        }
    };

    // Said once, and only when it is true: with neither a relay of last
    // resort nor a way through the router, the viewers who cannot reach this
    // machine directly have no way in at all, and nothing else would say so.
    if router.is_none() && !have_turn {
        let why = session.reach().map(|(_, w)| w).unwrap_or_default();
        let warning = format!(
            "{why}. Viewers on mobile data may not be able to connect; most others will."
        );
        eprintln!("  note: {warning}");
        session.note(warning);
    }

    let mut watching: tokio::task::JoinSet<Result<(), String>> = tokio::task::JoinSet::new();
    // Which viewer each task is serving, so the one that finished can be
    // taken off the list, even if it finished by panicking.
    let mut serving: std::collections::HashMap<tokio::task::Id, u64> = Default::default();
    let mut next_id = 1u64;
    let mut failures = 0usize;
    // What the relay was last told, so it is only told again on a change.
    let mut relay_locked = false;

    loop {
        if session.should_stop() {
            break;
        }

        if session.take_new_link() {
            new_link(relay, share, &session, !watching.is_empty()).await;
            // A new code on the relay starts unlocked.
            relay_locked = false;
        }

        let locked = session.locked();
        if locked != relay_locked {
            set_relay_lock(relay, &share.ticket, locked).await;
            relay_locked = locked;
        }

        // Read before the select, because the arms below borrow the set.
        let already_watching = watching.len();

        tokio::select! {
            // Somebody left. With nobody watching this goes back to offering
            // and waiting; with others still connected it changes nothing.
            Some(finished) = watching.join_next_with_id(), if !watching.is_empty() => {
                let (task, result) = match finished {
                    Ok((task, result)) => (task, result),
                    Err(e) => (e.id(), Err("stopped unexpectedly".to_owned())),
                };
                if let Some(id) = serving.remove(&task) {
                    let removed = session.is_kicked(id);
                    let who = session
                        .remove_viewer(id)
                        .map(|v| v.describe())
                        .unwrap_or_else(|| "someone".to_owned());
                    if removed {
                        session.tell(format!("removed {who}. They cannot come back this session."));
                    } else {
                        if session.sounds() {
                            chime::play(Chime::Left);
                        }
                        match result {
                            Err(e) if e != VIEWER_LEFT => session.note(format!("{who}: {e}")),
                            _ => session.tell(format!("{who} left")),
                        }
                    }
                }
                if watching.is_empty() && !session.should_stop() {
                    session.set_phase(Phase::Waiting {
                        code: Some(share.ticket.code.clone()),
                        link: share.link.clone(),
                    });
                }
            }

            admitted = admit_one(
                relay,
                &share.ticket,
                &share.link,
                &session,
                already_watching,
                &ice,
                router.as_ref(),
                next_id,
            ), if !locked => {
                match admitted {
                    Ok(Admitted { net: viewer, opening, viewer: info }) => {
                        failures = 0;
                        next_id += 1;
                        let id = info.id;
                        let who = info.describe();
                        session.add_viewer(info);
                        session.set_phase(Phase::Live);
                        session.tell(format!("{who} is watching"));
                        if session.sounds() {
                            chime::play(Chime::Joined);
                        }

                        let session = Arc::clone(&session);
                        let task = watching.spawn(async move {
                            // Held rather than used: dropping it is what
                            // closes the port on the router again.
                            let _opening = opening;
                            let keyframe = viewer.keyframe_signal();
                            let result = pump(&viewer, keyframe, session, Some(id)).await;
                            viewer.close().await;
                            result
                        });
                        serving.insert(task.id(), id);
                    }
                    Err(e) => {
                        // A prompt answered while others watch leaves the
                        // window on the prompt otherwise.
                        if !watching.is_empty() {
                            session.set_phase(Phase::Live);
                        } else if !benign(&e) {
                            // A failed admission leaves the code alive, so this
                            // goes round again and offers afresh. The count only
                            // gives up when nobody is watching at all.
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

            // Locked: nothing is offered, and this only wakes the loop to see
            // whether that has changed.
            _ = tokio::time::sleep(Duration::from_millis(300)), if locked => {}
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
            // Both of these change what should be on offer, so the offer
            // waiting here is withdrawn and the loop decides what comes next.
            if session.locked() || session.new_link_pending() {
                return Err(INTERRUPTED.into());
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
        // Not a failure. Going round again publishes a fresh offer, which is
        // also what keeps the relay from expiring a code nobody has used yet.
        Err(NOBODY_YET.to_owned())
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

async fn approved(who: &str, session: &Arc<Session>) -> Decision {
    // Asked for explicitly, so there is nobody to ask. They are still named
    // once they are connected, with a sound: not having to answer is the
    // point, not being unable to see who arrived.
    if session.auto_approve() {
        return Decision::Allowed;
    }

    session.request_approval(who);

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
/// The kinds of route a viewer offered, most useful first.
///
/// `srflx` means they found themselves through STUN and a direct route is at
/// least possible. Only `host` means STUN told them nothing, and `relay` means
/// they are prepared to go the long way round. A viewer offering neither srflx
/// nor relay is one that will only ever connect on the same network.
fn viewer_routes(answer: &str) -> String {
    let mut host = 0;
    let mut srflx = 0;
    let mut relay = 0;

    for line in answer.lines() {
        let Some(rest) = line.strip_prefix("a=candidate:") else { continue };
        let fields: Vec<&str> = rest.split_whitespace().collect();
        if fields.len() < 8 {
            continue;
        }
        match fields[7] {
            "host" => host += 1,
            "srflx" => srflx += 1,
            "relay" => relay += 1,
            _ => {}
        }
    }

    format!("{host} local, {srflx} through STUN, {relay} relayed")
}

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
        _ => NO_ADDRESS.to_owned(),
    }
}

const NO_ADDRESS: &str = "address not shared";

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
}

impl Media {
    fn start(keyframe: net::KeyframeSignal, session: &Arc<Session>) -> Self {
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
        //
        // The on switch is the session's, so the button, the hotkey and every
        // viewer's microphone are one switch.
        let microphone = Arc::new(mic::Mic::start_on(
            Some(chosen_mic).filter(|id| !id.is_empty()),
            Arc::clone(&stop),
            session.mic_flag(),
        ));
        if let Some(device) = microphone.opened() {
            session.set_mic_name(device.name.clone());
        }
        session.set_mic_available(microphone.available());

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
///
/// `id` is who this is on the session's list of viewers, when there is one,
/// which is what lets the host remove them.
async fn pump<P: webrtc::peer_connection::PeerConnection>(
    webrtc: &net::Session<P>,
    keyframe: net::KeyframeSignal,
    session: Arc<Session>,
    id: Option<u64>,
) -> Result<(), String> {
    let mut media = Media::start(keyframe, &session);
    let mut viewer = Viewer::new(webrtc, id);

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
    // Only for the one viewer served locally. Through a relay, one person
    // finishing is not the session finishing, and the loop admitting people
    // is what decides the phase.
    if result.is_ok() && id.is_none() {
        session.set_phase(Phase::Ended);
    }
    result
}

/// One person watching.
struct Viewer<'a, P: webrtc::peer_connection::PeerConnection> {
    net: &'a net::Session<P>,
    id: Option<u64>,
}

impl<'a, P: webrtc::peer_connection::PeerConnection> Viewer<'a, P> {
    fn new(net: &'a net::Session<P>, id: Option<u64>) -> Self {
        Self { net, id }
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

    loop {
        tokio::select! {
            Some((au, ts)) = media.video_rx.recv() => {
                // Sent whether or not the picture is hidden: hiding changes
                // what is encoded, see `card`, never whether it is sent.
                if !sabotage.should_drop() {
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
                for viewer in viewers.iter() {
                    let _ = viewer
                        .net
                        .send_audio(&packet.data, packet.timestamp_us, packet_duration)
                        .await;
                }
                session.note_audio();
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
                    break Served::Ended(Err(VIEWER_LEFT.to_owned()));
                }

                // Removed by the host. Ending here closes their connection,
                // which their page sees as the stream stopping.
                if viewers.iter().any(|v| v.id.is_some_and(|id| session.is_kicked(id))) {
                    break Served::Ended(Ok(()));
                }

                // Measured whether or not the microphone is live, so the meter
                // can say the right device is listening before anyone is
                // heard through it.
                session.set_mic_peak(media.microphone.take_peak());
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

    // What stands in for the picture while it is hidden, drawn at the size of
    // whatever it replaces, and the last real frame, for putting back the
    // moment it is shown again. Both belong to the capture's device, so they
    // go whenever the capture does.
    let mut cover: Option<(windows::Win32::Graphics::Direct3D11::ID3D11Texture2D, u32, u32)> = None;
    let mut last_real: Option<(windows::Win32::Graphics::Direct3D11::ID3D11Texture2D, u32, u32)> =
        None;
    let mut was_hidden = false;

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
                    cover = None;
                    last_real = None;
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
                            "{e}. A minimised window cannot be captured: restore it, or pick another application."
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
            cover = None;
            last_real = None;
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
                cover = None;
                last_real = None;
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

        // Hidden: the card goes to the encoder in place of every frame, at
        // that frame's size, and the switch in either direction is itself a
        // frame, so a window that is not changing still shows the change.
        //
        // If the card cannot be made, nothing is sent rather than the real
        // picture. Somebody who pressed hide wanted the picture gone, and a
        // held frame is a worse outcome than a broken card but a far better
        // one than what they were hiding.
        let hidden = session.hidden();
        let fresh = match fresh {
            Some((texture, w, h)) => {
                last_real = Some((texture.clone(), w, h));
                if hidden {
                    covering(&mut cover, source.device(), w, h, &session).map(|t| (t, w, h))
                } else {
                    Some((texture, w, h))
                }
            }
            None if hidden != was_hidden && dims != (0, 0) => {
                if hidden {
                    covering(&mut cover, source.device(), dims.0, dims.1, &session)
                        .map(|t| (t, dims.0, dims.1))
                } else {
                    last_real.clone()
                }
            }
            None => None,
        };
        was_hidden = hidden;

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

/// The pause card at this size, made the first time it is needed at it.
fn covering(
    cover: &mut Option<(windows::Win32::Graphics::Direct3D11::ID3D11Texture2D, u32, u32)>,
    device: &windows::Win32::Graphics::Direct3D11::ID3D11Device,
    w: u32,
    h: u32,
    session: &Session,
) -> Option<windows::Win32::Graphics::Direct3D11::ID3D11Texture2D> {
    if let Some((texture, cw, ch)) = cover.as_ref()
        && (*cw, *ch) == (w, h)
    {
        return Some(texture.clone());
    }
    match card::texture(device, w, h) {
        Ok(texture) => {
            *cover = Some((texture.clone(), w, h));
            Some(texture)
        }
        Err(e) => {
            session.note(e);
            None
        }
    }
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
    use super::{benign, describe_viewer, split_about, About, NOBODY_YET, REFUSED};

    #[test]
    fn the_pages_own_lines_come_off_the_answer() {
        let posted = "x-sideband-viewer:abc123\r\nx-sideband-device:Android, Chrome\r\nv=0\r\na=candidate:1 1 udp 1 203.0.113.9 5000 typ srflx\r\n";
        let (about, sdp) = split_about(posted);
        assert_eq!(about, About { browser: "abc123".into(), device: "Android, Chrome".into() });
        assert!(sdp.starts_with("v=0"), "{sdp:?}");
        assert!(!sdp.contains("x-sideband"));
        assert_eq!(describe_viewer(&sdp), "203.0.113.9");
    }

    #[test]
    fn an_answer_with_nothing_extra_is_left_as_it_was() {
        let plain = "v=0\r\ns=-\r\n";
        let (about, sdp) = split_about(plain);
        assert_eq!(about, About::default());
        assert_eq!(sdp, plain);
    }

    #[test]
    fn what_a_page_says_about_itself_is_cleaned_and_kept_short() {
        let posted = format!("x-sideband-device:{}\u{7}evil\r\nv=0\r\n", "x".repeat(100));
        let (about, _) = split_about(&posted);
        assert!(about.device.chars().count() <= 40);
        assert!(!about.device.chars().any(char::is_control));
    }

    #[test]
    fn waiting_and_saying_no_are_not_failures() {
        assert!(benign(NOBODY_YET) && benign(REFUSED));
        assert!(!benign("could not publish to the relay: timeout"));
    }

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
