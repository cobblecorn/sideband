#![allow(dead_code)] // the send loop that drives this arrives with stage 6

//! Stage 5, WebRTC transport.
//!
//! We feed *already encoded* H.264 and Opus into the tracks, so none of the
//! library's own encoders are involved. What we get in return is the part that
//! would be miserable to write: ICE hole punching, DTLS/SRTP, congestion
//! control, RTP packetisation and A/V sync.
//!
//! Non-trickle by choice. `offer()` does not return until ICE gathering has
//! completed, so the SDP it hands back is self-contained and the whole
//! handshake is four small requests against a Worker with no persistent
//! connection. It costs a second or two of setup and saves the entire
//! signalling-server complexity budget.

use std::sync::atomic::{AtomicBool, AtomicU16, AtomicU32, Ordering};
use std::sync::Arc;
use std::time::Duration;

use bytes::Bytes;
use rtc::interceptor::{interceptor, Interceptor, Packet as Wire, Registry, StreamInfo, TaggedPacket};
// The interceptor macro expands to code naming these by bare path, so they
// have to be in scope here even though nothing below mentions them. Same as
// in `bwe`, which has the other interceptor in this crate.
use rtc::sansio;
use rtc::shared::error::Error;
use rtc::media_stream::MediaStreamTrack;
use rtc::peer_connection::configuration::interceptor_registry::{
    configure_nack, configure_rtcp_reports,
};
use rtc::peer_connection::configuration::media_engine::{
    MediaEngine, MIME_TYPE_H264, MIME_TYPE_OPUS,
};
use rtc::peer_connection::configuration::RTCConfigurationBuilder;
use rtc::peer_connection::sdp::RTCSessionDescription;
pub use rtc::peer_connection::transport::RTCIceServer;
use rtc::rtp::codec::h264::H264Payloader;
use rtc::rtp::extension::abs_send_time_extension::AbsSendTimeExtension;
use rtc::rtp::extension::HeaderExtension;
use rtc::rtp::header::Header;
use rtc::rtp::packetizer::Payloader;
use rtc::rtp::Packet;
use rtc::rtp_transceiver::rtp_sender::*;
use rtc::rtp_transceiver::PayloadType;
use rtc::shared::time::SystemInstant;
use tokio::sync::{mpsc, Notify};
use webrtc::media_stream::track_local::static_rtp::TrackLocalStaticRTP;
use webrtc::media_stream::track_local::TrackLocal;
use webrtc::peer_connection::{
    PeerConnection, PeerConnectionBuilder, PeerConnectionEventHandler, RTCIceGatheringState,
    RTCPeerConnectionState,
};

use crate::bwe::{Feedback, FeedbackWatcher, ViewerFeedback};

/// The one address worth gathering candidates on.
///
/// Binding `0.0.0.0` gathers a candidate for every interface the machine has,
/// which sounds thorough and is actively harmful. A developer's machine has
/// several that go nowhere: a VirtualBox host-only adapter, a VPN client that
/// is installed but idle, and a link-local address on every unplugged NIC.
/// ICE cannot tell those apart from the real one, so it spends its time
/// sending STUN from networks with no route to anywhere.
///
/// Measured, with the stack's own logging on:
///
///     Failed to write packet to 162.159.207.0:3478 from 192.168.56.1:53401:
///     A socket operation was attempted to an unreachable network. (os 10051)
///     [controlling]: Setting new connection state: Failed
///
/// 192.168.56.1 is VirtualBox. Every check went out of an adapter that cannot
/// reach the internet. When the good pair happened to be tried first the
/// connection came up anyway, which is why this looked intermittent rather
/// than broken: sometimes slow, sometimes a connection that never completed.
///
/// So one address, the one the routing table would actually use. Nothing is
/// sent to find it out: connecting a UDP socket only asks the kernel which
/// local interface would carry a packet to the outside world.
fn local_bind() -> String {
    match local_ip() {
        Some(ip) => format!("{ip}:0"),
        // No route at all, which is a machine with no network rather than a
        // machine with too many. Everything is the best guess left.
        None => "0.0.0.0:0".to_owned(),
    }
}

/// The address of the interface that leads to the internet, see `local_bind`.
pub fn local_ip() -> Option<std::net::Ipv4Addr> {
    use std::net::{IpAddr, UdpSocket};

    let s = UdpSocket::bind("0.0.0.0:0").ok()?;
    s.connect("8.8.8.8:80").ok()?;
    match s.local_addr().ok()?.ip() {
        IpAddr::V4(ip) => Some(ip),
        IpAddr::V6(_) => None,
    }
}

/// How long to wait for ICE gathering before sending what we have.
const GATHER_TIMEOUT: Duration = Duration::from_secs(5);

/// RTP clock rates. These are fixed by the codec, not chosen: H.264 is always
/// carried at 90 kHz and Opus at 48 kHz.
const VIDEO_CLOCK_HZ: u64 = 90_000;
const AUDIO_CLOCK_HZ: u64 = 48_000;

const VIDEO_PAYLOAD_TYPE: PayloadType = 102;
const AUDIO_PAYLOAD_TYPE: PayloadType = 111;

/// High profile at level 5.1, one NAL unit per packet mode.
///
/// It used to say constrained baseline at level 3.1, which was a description
/// of neither end of what actually happens: the encoder is never told a
/// profile, so it uses the driver's own choice, and level 3.1 tops out around
/// 720p30 while a shared window is routinely 1080p60 or larger. Both halves
/// are now stated and matched, see `encoder::NvencEncoder::new`. Every
/// browser worth streaming to decodes High in hardware; it is what video
/// sites have served for a decade.
const H264_FMTP: &str = "level-asymmetry-allowed=1;packetization-mode=1;profile-level-id=640033";

/// How much faster than the rate control's target packets may leave.
///
/// Pacing exists because neither library paces: `write_sample` handed every
/// packet of a frame to the socket back to back, so a keyframe left here as a
/// burst of hundreds of packets and a Wi-Fi link drops bursts. The loss then
/// produced more keyframe requests, and each answer was another burst, which
/// is the likeliest reading of the keyframe storm measured earlier and blamed
/// at the time on the payloader.
///
/// Above one, so a frame larger than the average still finishes before the
/// next one is due; far below "all of it now".
const PACE_FACTOR: f64 = 2.5;

/// The most that may leave without waiting, in bytes. Small enough for any
/// link to absorb, large enough that the pacer wakes a few times per frame
/// rather than once per packet.
const PACE_BURST: f64 = 8_000.0;

/// Never pace slower than this, in bits per second, so a stream whose rate
/// control has wound right down still moves.
const PACE_FLOOR: u32 = 600_000;

/// Payload bytes per packet. 1200 leaves room for the RTP header, the
/// extension and SRTP's tag inside the 1280 byte path MTU that WebRTC
/// implementations settle on for the open internet.
const MTU: usize = 1200;

/// Roughly what a packet costs beyond its payload: headers, extension, SRTP
/// tag, UDP and IP. Only used for pacing arithmetic.
const PACKET_OVERHEAD: usize = 60;

/// How many packets may wait to go out. About 0.6 MB of video, which is a
/// fraction of a second at any rate this sends at: a queue deeper than that
/// would be latency nobody asked for.
const VIDEO_QUEUE: usize = 512;
const AUDIO_QUEUE: usize = 64;

/// Shared between the transport and the encode loop. The encoder checks it
/// each frame and emits an IDR when it is set.
#[derive(Clone, Default)]
pub struct KeyframeSignal(Arc<AtomicBool>);

impl KeyframeSignal {
    pub fn request(&self) {
        self.0.store(true, Ordering::Relaxed);
    }

    /// Reads and clears in one go, so a request is never serviced twice.
    pub fn take(&self) -> bool {
        self.0.swap(false, Ordering::Relaxed)
    }
}

struct Handler {
    gathering_done: Arc<Notify>,
    connected: Arc<Notify>,
    /// Set the moment the connection is up, and never cleared.
    ///
    /// The flag is what makes this reliable; the `Notify` beside it only
    /// shortens the wait. `notify_waiters` wakes tasks that are *already*
    /// waiting and stores nothing for one that has not arrived yet, so a
    /// viewer who connects before the host reaches its wait is a notification
    /// that goes nowhere and a host that waits for ever. It is a race, and it
    /// is worse the faster the viewer is: a reconnect, where both ends already
    /// know the route, loses it almost every time.
    is_connected: Arc<AtomicBool>,
    keyframe: KeyframeSignal,
    /// Set once the connection reaches a state it cannot come back from.
    /// Without this the send loop keeps encoding into a socket nobody is
    /// listening on and the host is told everything is fine.
    lost: Arc<AtomicBool>,
}

#[async_trait::async_trait]
impl PeerConnectionEventHandler for Handler {
    async fn on_ice_gathering_state_change(&self, state: RTCIceGatheringState) {
        if state == RTCIceGatheringState::Complete {
            self.gathering_done.notify_waiters();
        }
    }

    async fn on_connection_state_change(&self, state: RTCPeerConnectionState) {
        match state {
            RTCPeerConnectionState::Connected => {
                // A viewer joining mid-stream has no reference frame, so the
                // very first thing they need is an IDR.
                self.keyframe.request();
                // Flag first, then wake. In this order a waiter that arrives
                // between the two sees the flag rather than missing the wake.
                self.is_connected.store(true, Ordering::Relaxed);
                self.connected.notify_waiters();
            }
            // `Disconnected` is deliberately not here: it is what a couple of
            // lost consent checks look like, and connections routinely come
            // back from it. Only these two are the end.
            RTCPeerConnectionState::Failed | RTCPeerConnectionState::Closed => {
                self.lost.store(true, Ordering::Relaxed);
            }
            _ => {}
        }
    }
}

pub struct Session<P: PeerConnection> {
    pc: P,
    /// Splits access units into RTP payloads: fragments anything over the
    /// MTU, and puts SPS and PPS in front of each keyframe as one STAP-A.
    payloader: std::sync::Mutex<H264Payloader>,
    /// Ours to hand out now that packets are built here rather than by the
    /// library. They must be consecutive per stream: a gap means "lost" to
    /// the far end, and it will ask for the missing packet for ever.
    video_seq: AtomicU16,
    audio_seq: AtomicU16,
    pacer: Pacer,
    /// These must match what the tracks were created with or the far end
    /// sees nothing.
    video_ssrc: u32,
    audio_ssrc: u32,
    gathering_done: Arc<Notify>,
    connected: Arc<Notify>,
    is_connected: Arc<AtomicBool>,
    keyframe: KeyframeSignal,
    lost: Arc<AtomicBool>,
    feedback: ViewerFeedback,
}

/// Builds a peer connection with both tracks attached. Free function rather
/// than an inherent constructor: the concrete peer-connection type is only
/// nameable as `impl PeerConnection`, which an inherent `Self`-returning
/// method cannot express.
/// Builds the ICE server list.
///
/// STUN alone is enough for most home-to-home connections, but not all: when
/// one side is behind carrier-grade NAT there is no hole to punch and the
/// media has to be relayed. That is the classic "works at my house, fails at
/// hers" failure, and it cannot be diagnosed after the fact, so TURN is
/// configured up front even though it will rarely be used.
///
/// Blocking, because fetching Cloudflare credentials is an HTTP call. Call it
/// before the runtime starts carrying media.
/// Whether a relay of last resort is configured.
///
/// Worth being able to ask, because its absence is invisible until a specific
/// kind of viewer cannot connect and nothing says why.
pub fn have_turn() -> bool {
    let explicit = std::env::var("SIDEBAND_TURN_URL").is_ok()
        && std::env::var("SIDEBAND_TURN_USER").is_ok()
        && std::env::var("SIDEBAND_TURN_PASS").is_ok();
    let cloudflare = std::env::var("SIDEBAND_CF_TURN_KEY_ID").is_ok()
        && std::env::var("SIDEBAND_CF_TURN_TOKEN").is_ok();
    explicit || cloudflare
}

/// What the relay says the route options are, if it says anything.
///
/// Asked before falling back to what this machine happens to have configured,
/// because the relay is the one piece of the setup that is already shared:
/// configure it once there and every machine pointed at it is configured too.
/// A new laptop, a reinstall, a friend running it, all of them just work.
///
/// Failure is not an error. A relay that has nothing to say leaves this
/// exactly where it was.
pub fn ice_from_relay(relay: &str) -> Option<(Vec<RTCIceServer>, bool)> {
    let url = format!("{}/api/ice", relay.trim_end_matches('/'));
    let body: serde_json::Value = ureq::get(&url).call().ok()?.body_mut().read_json().ok()?;

    let listed = body.get("iceServers")?.as_array()?;
    let mut servers = Vec::new();
    for entry in listed {
        let urls = match entry.get("urls") {
            Some(serde_json::Value::Array(a)) => {
                a.iter().filter_map(|u| u.as_str().map(str::to_owned)).collect()
            }
            Some(serde_json::Value::String(u)) => vec![u.clone()],
            _ => continue,
        };
        if urls.is_empty() {
            continue;
        }
        servers.push(RTCIceServer {
            urls,
            username: entry
                .get("username")
                .and_then(|v| v.as_str())
                .unwrap_or_default()
                .to_owned(),
            credential: entry
                .get("credential")
                .and_then(|v| v.as_str())
                .unwrap_or_default()
                .to_owned(),
        });
    }

    if servers.is_empty() {
        return None;
    }
    let turn = body.get("turn").and_then(|v| v.as_bool()).unwrap_or(false);
    Some((servers, turn))
}

pub fn ice_servers() -> Vec<RTCIceServer> {
    let mut servers = vec![RTCIceServer {
        urls: vec![
            "stun:stun.cloudflare.com:3478".to_owned(),
            "stun:stun.l.google.com:19302".to_owned(),
        ],
        ..Default::default()
    }];

    // A TURN server of your own, or any provider's.
    if let (Ok(url), Ok(user), Ok(pass)) = (
        std::env::var("SIDEBAND_TURN_URL"),
        std::env::var("SIDEBAND_TURN_USER"),
        std::env::var("SIDEBAND_TURN_PASS"),
    ) {
        servers.push(RTCIceServer {
            urls: vec![url],
            username: user,
            credential: pass,
        });
        return servers;
    }

    // Cloudflare Realtime TURN issues short-lived credentials on demand, so
    // nothing long-lived is baked into the binary.
    if let (Ok(key_id), Ok(token)) = (
        std::env::var("SIDEBAND_CF_TURN_KEY_ID"),
        std::env::var("SIDEBAND_CF_TURN_TOKEN"),
    ) {
        match fetch_cloudflare_turn(&key_id, &token) {
            Ok(server) => servers.push(server),
            // Losing TURN degrades to STUN-only rather than failing outright:
            // most connections do not need it, and a hard error here would
            // block sessions that would have worked.
            Err(e) => eprintln!("  warning: could not get TURN credentials ({e}); STUN only"),
        }
    }

    servers
}

fn fetch_cloudflare_turn(key_id: &str, token: &str) -> Result<RTCIceServer, String> {
    let url = format!(
        "https://rtc.live.cloudflare.com/v1/turn/keys/{key_id}/credentials/generate-ice-servers"
    );

    let body: serde_json::Value = ureq::post(&url)
        .header("Authorization", &format!("Bearer {token}"))
        .send_json(serde_json::json!({ "ttl": 86400 }))
        .map_err(|e| format!("request failed: {e}"))?
        .body_mut()
        .read_json()
        .map_err(|e| format!("bad response: {e}"))?;

    let entry = body
        .get("iceServers")
        .and_then(|v| if v.is_array() { v.get(0) } else { Some(v) })
        .ok_or("no iceServers in response")?;

    let urls = match entry.get("urls") {
        Some(serde_json::Value::Array(a)) => {
            a.iter().filter_map(|u| u.as_str().map(str::to_owned)).collect()
        }
        Some(serde_json::Value::String(u)) => vec![u.clone()],
        _ => return Err("no urls in iceServers".into()),
    };

    Ok(RTCIceServer {
        urls,
        username: entry.get("username").and_then(|v| v.as_str()).unwrap_or_default().to_owned(),
        credential: entry.get("credential").and_then(|v| v.as_str()).unwrap_or_default().to_owned(),
    })
}

pub async fn connect(
    ice: Vec<RTCIceServer>,
) -> Result<Session<impl PeerConnection>, String> {
    let mut media_engine = MediaEngine::default();

    // Asking for these is what makes the viewer's browser *able* to send
    // us picture-loss and NACK feedback. Negotiating them costs nothing
    // and not negotiating them cannot be fixed later in the session.
    let video_feedback = vec![
        RTCPFeedback { typ: "nack".to_owned(), parameter: "".to_owned() },
        RTCPFeedback { typ: "nack".to_owned(), parameter: "pli".to_owned() },
        RTCPFeedback { typ: "ccm".to_owned(), parameter: "fir".to_owned() },
        RTCPFeedback { typ: "goog-remb".to_owned(), parameter: "".to_owned() },
    ];

    let video_codec = RTCRtpCodecParameters {
        rtp_codec: RTCRtpCodec {
            mime_type: MIME_TYPE_H264.to_owned(),
            clock_rate: 90_000,
            channels: 0,
            sdp_fmtp_line: H264_FMTP.to_owned(),
            rtcp_feedback: video_feedback,
        },
        payload_type: VIDEO_PAYLOAD_TYPE,
        ..Default::default()
    };

    let audio_codec = RTCRtpCodecParameters {
        rtp_codec: RTCRtpCodec {
            mime_type: MIME_TYPE_OPUS.to_owned(),
            clock_rate: 48_000,
            channels: 2,
            // Tell the far end we send stereo and would like FEC honoured.
            sdp_fmtp_line: "minptime=10;useinbandfec=1;stereo=1".to_owned(),
            rtcp_feedback: vec![],
        },
        payload_type: AUDIO_PAYLOAD_TYPE,
        ..Default::default()
    };

    media_engine
        .register_codec(video_codec.clone(), RtpCodecKind::Video)
        .map_err(|e| format!("could not register H.264: {e}"))?;
    media_engine
        .register_codec(audio_codec.clone(), RtpCodecKind::Audio)
        .map_err(|e| format!("could not register Opus: {e}"))?;

    // Chosen here rather than after the connection is built, because both
    // the feedback watcher and the loss layer have to know which stream they
    // are looking at before they join the chain.
    let video_ssrc = rand::random::<u32>();
    let audio_ssrc = rand::random::<u32>();

    // Retransmission and RTCP reports, and deliberately not the default set.
    //
    // `register_default_interceptors` also advertises transport-wide
    // congestion control while registering only the *receiving* half of it,
    // so nothing here ever stamps an outgoing packet with the sequence
    // number that feedback is built from. A browser offered it and never
    // given the numbers falls back to REMB anyway; not offering it at all
    // means there is no ambiguity about which estimate is in use, and REMB
    // is the one this reads. See `bwe`.
    //
    // The loss layer goes on first, which puts it innermost, nearest the
    // socket: packets reach it after the retransmission buffer has kept a
    // copy, so one it throws away can still be asked for and resent. That is
    // what loss on a wire is, and it is the whole point of the facility.
    let registry = Registry::new().with(Loss::layer(video_ssrc));
    let registry = configure_nack(registry, &mut media_engine);
    let registry = configure_rtcp_reports(registry);

    // The viewer's estimate is built from how far apart packets arrive
    // compared with how far apart they were sent. Without this extension the
    // second half of that is a guess, and with the pacer below deliberately
    // spacing packets out, a guess that is now wrong on purpose.
    for kind in [RtpCodecKind::Video, RtpCodecKind::Audio] {
        media_engine
            .register_header_extension(
                RTCRtpHeaderExtensionCapability {
                    uri: rtc::sdp::extmap::ABS_SEND_TIME_URI.to_owned(),
                },
                kind,
                None,
            )
            .map_err(|e| format!("could not register the send-time extension: {e}"))?;
    }

    // The only place the viewer's RTCP is visible, see `bwe`. Without this
    // layer every connection looks perfect no matter what it is doing, and a
    // viewer asking for a picture it can decode is heard by nobody.
    let keyframe = KeyframeSignal::default();
    let (watcher, feedback) = FeedbackWatcher::layer(video_ssrc, keyframe.clone());
    let registry = registry.with(watcher);

    let config = RTCConfigurationBuilder::new().with_ice_servers(ice).build();

    let gathering_done = Arc::new(Notify::new());
    let connected = Arc::new(Notify::new());
    let is_connected = Arc::new(AtomicBool::new(false));
    let lost = Arc::new(AtomicBool::new(false));

    let pc = PeerConnectionBuilder::new()
        .with_configuration(config)
        .with_media_engine(media_engine)
        .with_interceptor_registry(registry)
        .with_handler(Arc::new(Handler {
            gathering_done: Arc::clone(&gathering_done),
            connected: Arc::clone(&connected),
            is_connected: Arc::clone(&is_connected),
            keyframe: keyframe.clone(),
            lost: Arc::clone(&lost),
        }))
        .with_udp_addrs(vec![local_bind()])
        .build()
        .await
        .map_err(|e| format!("could not build peer connection: {e}"))?;

    let video = Arc::new(
        TrackLocalStaticRTP::new(MediaStreamTrack::new(
            "sideband".to_owned(),
            "sideband-video".to_owned(),
            "sideband-video".to_owned(),
            RtpCodecKind::Video,
            vec![RTCRtpEncodingParameters {
                rtp_coding_parameters: RTCRtpCodingParameters {
                    ssrc: Some(video_ssrc),
                    ..Default::default()
                },
                codec: video_codec.rtp_codec.clone(),
                ..Default::default()
            }],
        )),
    );

    let audio = Arc::new(
        TrackLocalStaticRTP::new(MediaStreamTrack::new(
            "sideband".to_owned(),
            "sideband-audio".to_owned(),
            "sideband-audio".to_owned(),
            RtpCodecKind::Audio,
            vec![RTCRtpEncodingParameters {
                rtp_coding_parameters: RTCRtpCodingParameters {
                    ssrc: Some(audio_ssrc),
                    ..Default::default()
                },
                codec: audio_codec.rtp_codec.clone(),
                ..Default::default()
            }],
        )),
    );

    pc.add_track(Arc::clone(&video) as Arc<dyn TrackLocal>)
        .await
        .map_err(|e| format!("could not add video track: {e}"))?;
    pc.add_track(Arc::clone(&audio) as Arc<dyn TrackLocal>)
        .await
        .map_err(|e| format!("could not add audio track: {e}"))?;

    // Everything written to a track from here goes through the pacer, which
    // owns the tracks and is the only thing that writes to them. It stops
    // when the session is dropped and both queues close.
    let (video_tx, video_rx) = mpsc::channel::<Packet>(VIDEO_QUEUE);
    let (audio_tx, audio_rx) = mpsc::channel::<Packet>(AUDIO_QUEUE);
    let rate = Arc::new(AtomicU32::new(crate::bwe::START_BITRATE));
    tokio::spawn(pace(video_rx, audio_rx, video, audio, Arc::clone(&rate)));

    Ok(Session {
        pc,
        payloader: std::sync::Mutex::new(H264Payloader::default()),
        // Random, as RFC 3550 asks: a stream that always started at zero
        // would be an easy one to guess your way into.
        video_seq: AtomicU16::new(rand::random()),
        audio_seq: AtomicU16::new(rand::random()),
        pacer: Pacer { video: video_tx, audio: audio_tx, rate },
        video_ssrc,
        audio_ssrc,
        gathering_done,
        connected,
        is_connected,
        keyframe,
        lost,
        feedback,
    })
}

/// The send side of one connection: packets in, paced writes out.
struct Pacer {
    video: mpsc::Sender<Packet>,
    audio: mpsc::Sender<Packet>,
    /// What the rate controller last asked for, in bits per second.
    rate: Arc<AtomicU32>,
}

/// Writes packets out at a steady rate instead of all at once.
///
/// Audio is served first and never waits. It is a fortieth of the bitrate, it
/// is what a viewer notices instantly, and holding a voice packet behind a
/// keyframe buys nothing.
async fn pace(
    mut video_rx: mpsc::Receiver<Packet>,
    mut audio_rx: mpsc::Receiver<Packet>,
    video: Arc<TrackLocalStaticRTP>,
    audio: Arc<TrackLocalStaticRTP>,
    rate: Arc<AtomicU32>,
) {
    let baseline = SystemInstant::now();
    let mut budget = PACE_BURST;
    let mut last = std::time::Instant::now();

    loop {
        tokio::select! {
            biased;
            Some(packet) = audio_rx.recv() => write(&audio, packet, &baseline).await,
            Some(packet) = video_rx.recv() => {
                let cost = (packet.payload.len() + PACKET_OVERHEAD) as f64;
                loop {
                    let per_second =
                        (rate.load(Ordering::Relaxed).max(PACE_FLOOR) as f64 / 8.0) * PACE_FACTOR;
                    let now = std::time::Instant::now();
                    budget = (budget + now.duration_since(last).as_secs_f64() * per_second)
                        .min(PACE_BURST);
                    last = now;
                    if budget >= cost {
                        break;
                    }
                    // Waiting for room, and letting audio past while we wait.
                    let wait = Duration::from_secs_f64(((cost - budget) / per_second).min(0.05));
                    tokio::select! {
                        biased;
                        Some(packet) = audio_rx.recv() => write(&audio, packet, &baseline).await,
                        _ = tokio::time::sleep(wait) => {}
                    }
                }
                budget -= cost;
                write(&video, packet, &baseline).await;
            }
            else => break,
        }
    }
}

/// Turns one frame's payloads into numbered RTP packets.
///
/// Every packet of a frame carries the same timestamp, the numbers run
/// consecutively across frames, and the last packet of the frame is marked,
/// which is how the far end knows the frame is complete rather than waiting
/// for something more.
fn number(
    payloads: Vec<Bytes>,
    payload_type: PayloadType,
    ssrc: u32,
    timestamp: u32,
    seq: &AtomicU16,
) -> Vec<Packet> {
    let last = payloads.len().saturating_sub(1);
    payloads
        .into_iter()
        .enumerate()
        .map(|(n, payload)| Packet {
            header: Header {
                version: 2,
                marker: n == last,
                payload_type,
                // Wrapping is not an error: RTP sequence numbers are 16 bits
                // and are meant to go round, and every receiver handles it.
                sequence_number: seq.fetch_add(1, Ordering::Relaxed),
                timestamp,
                ssrc,
                ..Default::default()
            },
            payload,
        })
        .collect()
}

/// One packet, stamped with the moment it actually leaves.
async fn write(track: &TrackLocalStaticRTP, packet: Packet, baseline: &SystemInstant) {
    let sent = HeaderExtension::AbsSendTime(AbsSendTimeExtension::new(
        baseline.ntp(std::time::Instant::now()),
    ));
    // A failed write is the connection going away, which `lost` reports and
    // the send loop acts on; there is nothing useful to do per packet.
    let _ = track.write_rtp_with_extensions(packet, &[sent]).await;
}

/// Deliberately loses packets, to see what the viewer does about it.
///
/// `SIDEBAND_DROP` is a percentage, and while it is set that share of video
/// packets is numbered, kept for retransmission, and then dropped on the way
/// out. It sits innermost in the interceptor chain, which is what makes it a
/// fair imitation of a bad link rather than a punishment: a real lost packet
/// was sent and can be sent again when the viewer asks, and one dropped
/// before the retransmission buffer never can be.
///
/// It drops *packets*, and it has to. It used to drop whole access units
/// before they were packetised, which left no gap in the sequence numbers, so
/// the viewer's browser never knew anything was missing and its recovery
/// machinery, the part actually worth testing, never ran. What that proved
/// was that intra refresh repairs a damaged picture, which was never the
/// question being asked.
#[derive(Interceptor)]
struct Loss<P> {
    #[next]
    inner: P,
    sabotage: Sabotage,
    /// Video only. Losing speech teaches nothing here: Opus carries its own
    /// repair and a missing 20 ms is a blip, not a broken reference chain.
    video_ssrc: u32,
}

impl<P> Loss<P> {
    fn layer(video_ssrc: u32) -> impl FnOnce(P) -> Loss<P> {
        move |inner| Loss { inner, sabotage: Sabotage::from_env(), video_ssrc }
    }
}

#[interceptor]
impl<P: Interceptor> Loss<P> {
    #[overrides]
    fn handle_write(&mut self, msg: TaggedPacket) -> Result<(), Self::Error> {
        if let Wire::Rtp(ref rtp) = msg.message
            && rtp.header.ssrc == self.video_ssrc
            && self.sabotage.should_drop()
        {
            // Swallowed rather than passed on. Everything upstream believes
            // it was sent, which is exactly what the sender believes about a
            // packet a network drops.
            return Ok(());
        }
        self.inner.handle_write(msg)
    }
}

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
            eprintln!("  sabotage: dropping {percent}% of video packets");
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



impl<P: PeerConnection> Session<P> {

    /// A complete offer with every ICE candidate already embedded. This is the
    /// blob the pairing code maps to, see stage 6.
    pub async fn offer(&self) -> Result<String, String> {
        let offer = self
            .pc
            .create_offer(None)
            .await
            .map_err(|e| format!("could not create offer: {e}"))?;

        // Registering the waiter before setting the local description is not
        // enough on its own: `Notified` does not actually subscribe until it
        // is first polled, so a gather that completes during
        // set_local_description would be missed and we would wait forever.
        // `enable()` performs the subscription up front.
        let mut wait = Box::pin(self.gathering_done.notified());
        wait.as_mut().enable();

        self.pc
            .set_local_description(offer)
            .await
            .map_err(|e| format!("could not set local description: {e}"))?;

        // And a cap regardless: some candidate sets never formally reach
        // Complete, and a viewer waiting forever is worse than an offer with
        // one fewer candidate in it.
        let _ = tokio::time::timeout(GATHER_TIMEOUT, wait).await;

        let local = self
            .pc
            .local_description()
            .await
            .ok_or("no local description after gathering")?;

        Ok(local.sdp)
    }

    pub async fn accept_answer(&self, sdp: &str) -> Result<(), String> {
        let answer = RTCSessionDescription::answer(sdp.to_owned())
            .map_err(|e| format!("malformed answer SDP: {e}"))?;
        self.pc
            .set_remote_description(answer)
            .await
            .map_err(|e| format!("could not set remote description: {e}"))
    }

    /// Shuts the connection down and gives back what it is holding.
    ///
    /// Dropping is not enough. A peer connection owns a UDP socket and a live
    /// ICE agent, and neither goes away because the last handle did: the agent
    /// keeps answering connectivity checks on candidates the far end may still
    /// have, from a session nobody is using any more. Offering again without
    /// this left the old agent running beside the new one, and the second
    /// connection failed ICE every time while the first had been fine.
    pub async fn close(&self) {
        let _ = self.pc.close().await;
    }

    /// Resolves once the viewer's browser is actually connected.
    ///
    /// Checks the flag first and keeps checking, rather than trusting a single
    /// notification to arrive after this is called. See `Handler::is_connected`
    /// for the race that makes the flag the real answer here; the short waits
    /// only keep this from being a busy loop.
    pub async fn wait_connected(&self) {
        while !self.is_connected.load(Ordering::Relaxed) {
            let _ = tokio::time::timeout(
                std::time::Duration::from_millis(50),
                self.connected.notified(),
            )
            .await;
        }
    }

    pub fn keyframe_signal(&self) -> KeyframeSignal {
        self.keyframe.clone()
    }

    /// Whether the connection has reached a state it cannot recover from.
    pub fn lost(&self) -> bool {
        self.lost.load(Ordering::Relaxed)
    }

    /// Everything the viewer has reported about the video stream since this
    /// was last called. Reading clears it, see `bwe::ViewerFeedback`.
    pub fn viewer_feedback(&self) -> Feedback {
        self.feedback.take()
    }

    /// Tells the pacer what the rate controller settled on.
    pub fn set_pace_rate(&self, bits_per_second: u32) {
        self.pacer.rate.store(bits_per_second, Ordering::Relaxed);
    }

    /// Sends one access unit, timed by the shared media clock.
    ///
    /// `timestamp_us` becomes the RTP timestamp directly, and that is the
    /// whole of a fix that was needed: this used to call `write_sample`,
    /// which ignores the timestamp handed to it and builds its own by adding
    /// up the *durations* of the samples it has seen (see the packetizer in
    /// `rtc-rtp`). The duration passed was a flat 1/fps, so every slot the
    /// frame pacer skipped over a stall, and every change of frame rate, put
    /// video permanently further behind real time and behind the audio, and
    /// fed the viewer's bandwidth estimate arrival times that were not true.
    pub async fn send_video(&self, access_unit: &[u8], timestamp_us: u64) -> Result<(), String> {
        let payloads = {
            let mut payloader =
                self.payloader.lock().map_err(|_| "the payloader is poisoned")?;
            payloader
                .payload(MTU, &Bytes::copy_from_slice(access_unit))
                .map_err(|e| format!("could not packetise video: {e}"))?
        };
        if payloads.is_empty() {
            return Ok(());
        }

        // All of a frame or none of it. Half a frame is a hole the viewer has
        // to repair, and the numbers handed out would promise packets that
        // were never sent, which it would ask for until it gave up.
        if self.pacer.video.capacity() < payloads.len() {
            // A full queue is a link that cannot carry what is being sent.
            // Whatever is skipped here, the keyframe puts right.
            self.keyframe.request();
            return Ok(());
        }

        let frame = number(
            payloads,
            VIDEO_PAYLOAD_TYPE,
            self.video_ssrc,
            rtp_ticks(timestamp_us, VIDEO_CLOCK_HZ),
            &self.video_seq,
        );
        for packet in frame {
            let _ = self.pacer.video.try_send(packet);
        }
        Ok(())
    }

    /// One Opus packet, which is always one RTP packet: they are 20 ms each,
    /// far below any MTU, and nothing is gained by bundling them.
    pub async fn send_audio(&self, packet: &[u8], timestamp_us: u64) -> Result<(), String> {
        let packet = number(
            vec![Bytes::copy_from_slice(packet)],
            AUDIO_PAYLOAD_TYPE,
            self.audio_ssrc,
            rtp_ticks(timestamp_us, AUDIO_CLOCK_HZ),
            &self.audio_seq,
        )
        .remove(0);
        // Dropped rather than waited on. The queue holds more than a second
        // of speech, so a full one means the connection is gone, and blocking
        // here would stall the capture thread behind it.
        let _ = self.pacer.audio.try_send(packet);
        Ok(())
    }
}

/// Microseconds from the media clock to ticks of an RTP clock. Wrapping is
/// correct and expected, RTP timestamps are explicitly a 32-bit value that
/// rolls over, and receivers handle the wrap.
fn rtp_ticks(timestamp_us: u64, clock_hz: u64) -> u32 {
    ((timestamp_us.wrapping_mul(clock_hz)) / 1_000_000) as u32
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rtp_ticks_advance_at_the_codec_clock_rate() {
        // One 60 fps video frame is 1500 ticks of a 90 kHz clock.
        assert_eq!(rtp_ticks(0, VIDEO_CLOCK_HZ), 0);
        assert_eq!(rtp_ticks(16_666, VIDEO_CLOCK_HZ), 1499);
        assert_eq!(rtp_ticks(1_000_000, VIDEO_CLOCK_HZ), 90_000);

        // One 20 ms Opus frame is 960 ticks of a 48 kHz clock.
        assert_eq!(rtp_ticks(20_000, AUDIO_CLOCK_HZ), 960);
        assert_eq!(rtp_ticks(1_000_000, AUDIO_CLOCK_HZ), 48_000);
    }

    #[test]
    fn rtp_ticks_are_distinct_per_frame() {
        // The bug this guards: every frame stamped identically (zero) leaves
        // the decoder unable to order anything, so nothing between keyframes
        // decodes at all.
        let stamps: Vec<u32> = (0..10)
            .map(|i| rtp_ticks(i * 16_666, VIDEO_CLOCK_HZ))
            .collect();
        let mut sorted = stamps.clone();
        sorted.dedup();
        assert_eq!(sorted.len(), stamps.len(), "frames must not share a timestamp");
        assert!(stamps.windows(2).all(|w| w[1] > w[0]), "must advance monotonically");
    }

    #[test]
    fn a_frame_is_numbered_in_order_and_only_the_last_packet_is_marked() {
        let seq = AtomicU16::new(65_534);
        let frame = number(
            vec![Bytes::from_static(b"a"), Bytes::from_static(b"b"), Bytes::from_static(b"c")],
            102,
            7,
            90_000,
            &seq,
        );

        let numbers: Vec<u16> = frame.iter().map(|p| p.header.sequence_number).collect();
        // Through the wrap, which happens every eleven minutes at these rates
        // and must not produce a gap.
        assert_eq!(numbers, vec![65_534, 65_535, 0]);
        assert_eq!(
            frame.iter().map(|p| p.header.marker).collect::<Vec<_>>(),
            vec![false, false, true],
            "a receiver waits for the marker before decoding the frame"
        );
        assert!(
            frame.iter().all(|p| p.header.timestamp == 90_000 && p.header.ssrc == 7),
            "one frame is one instant"
        );
    }

    #[test]
    fn the_next_frame_carries_on_from_the_last_number() {
        let seq = AtomicU16::new(0);
        let first = number(vec![Bytes::from_static(b"a")], 102, 1, 0, &seq);
        let second = number(vec![Bytes::from_static(b"b")], 102, 1, 3000, &seq);
        assert_eq!(first[0].header.sequence_number + 1, second[0].header.sequence_number);
    }

    #[test]
    fn dropping_packets_hits_the_share_it_was_asked_for() {
        // The test facility itself: at 5%, one packet in twenty goes missing.
        let mut sabotage = Sabotage { percent: 5, counter: 0 };
        let dropped = (0..1000).filter(|_| sabotage.should_drop()).count();
        assert_eq!(dropped, 50);

        let mut none = Sabotage { percent: 0, counter: 0 };
        assert!(!none.should_drop(), "off unless asked for");
    }

    #[test]
    fn keyframe_signal_is_take_once() {
        let s = KeyframeSignal::default();
        assert!(!s.take(), "starts clear");

        s.request();
        assert!(s.take(), "request is observed");
        assert!(!s.take(), "and is not observed twice");
    }

    #[test]
    fn keyframe_requests_collapse() {
        // Several PLIs arriving between frames should cost one IDR, not one
        // per request, a burst of them is exactly what packet loss produces.
        let s = KeyframeSignal::default();
        s.request();
        s.request();
        s.request();
        assert!(s.take());
        assert!(!s.take());
    }

    #[test]
    fn keyframe_signal_shares_across_clones() {
        // The transport half and the encode half hold separate clones.
        let transport = KeyframeSignal::default();
        let encoder = transport.clone();
        transport.request();
        assert!(encoder.take(), "clone observes the request");
        assert!(!transport.take(), "and clearing is shared");
    }
}
