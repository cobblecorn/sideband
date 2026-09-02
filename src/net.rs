#![allow(dead_code)] // the send loop that drives this arrives with stage 6

//! Stage 5 — WebRTC transport.
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

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::Duration;

use bytes::Bytes;
use rtc::interceptor::Registry;
use rtc::media::Sample;
use rtc::shared::time::SystemInstant;
use rtc::media_stream::MediaStreamTrack;
use rtc::peer_connection::configuration::interceptor_registry::register_default_interceptors;
use rtc::peer_connection::configuration::media_engine::{
    MediaEngine, MIME_TYPE_H264, MIME_TYPE_OPUS,
};
use rtc::peer_connection::configuration::RTCConfigurationBuilder;
use rtc::peer_connection::sdp::RTCSessionDescription;
use rtc::peer_connection::transport::RTCIceServer;
use rtc::rtp_transceiver::rtp_sender::*;
use rtc::rtp_transceiver::PayloadType;
use tokio::sync::Notify;
use webrtc::media_stream::track_local::static_sample::TrackLocalStaticSample;
use webrtc::media_stream::track_local::TrackLocal;
use webrtc::peer_connection::{
    PeerConnection, PeerConnectionBuilder, PeerConnectionEventHandler, RTCIceGatheringState,
    RTCPeerConnectionState,
};

use crate::bwe::{Feedback, FeedbackWatcher, ViewerFeedback};

/// How long to wait for ICE gathering before sending what we have.
const GATHER_TIMEOUT: Duration = Duration::from_secs(5);

/// RTP clock rates. These are fixed by the codec, not chosen: H.264 is always
/// carried at 90 kHz and Opus at 48 kHz.
const VIDEO_CLOCK_HZ: u64 = 90_000;
const AUDIO_CLOCK_HZ: u64 = 48_000;

const VIDEO_PAYLOAD_TYPE: PayloadType = 102;
const AUDIO_PAYLOAD_TYPE: PayloadType = 111;

/// Baseline H.264 at level 3.1, single NAL per packet mode. This is the
/// profile every browser decodes in hardware; picking anything more exotic is
/// how you end up with a viewer whose CPU melts.
const H264_FMTP: &str = "level-asymmetry-allowed=1;packetization-mode=1;profile-level-id=42e01f";

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
    video: Arc<TrackLocalStaticSample>,
    audio: Arc<TrackLocalStaticSample>,
    /// write_sample needs these explicitly; they must match what the tracks
    /// were created with or the far end sees nothing.
    video_ssrc: u32,
    audio_ssrc: u32,
    gathering_done: Arc<Notify>,
    connected: Arc<Notify>,
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
/// hers" failure, and it cannot be diagnosed after the fact — so TURN is
/// configured up front even though it will rarely be used.
///
/// Blocking, because fetching Cloudflare credentials is an HTTP call. Call it
/// before the runtime starts carrying media.
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

    let registry = register_default_interceptors(Registry::new(), &mut media_engine)
        .map_err(|e| format!("could not register interceptors: {e}"))?;

    // Chosen here rather than after the connection is built, because the
    // feedback watcher has to know which stream it is measuring before it
    // joins the chain.
    let video_ssrc = rand::random::<u32>();
    let audio_ssrc = rand::random::<u32>();

    // The only place the viewer's RTCP is visible — see `bwe`. Without this
    // layer every connection looks perfect no matter what it is doing.
    let (watcher, feedback) = FeedbackWatcher::layer(video_ssrc);
    let registry = registry.with(watcher);

    let config = RTCConfigurationBuilder::new().with_ice_servers(ice).build();

    let gathering_done = Arc::new(Notify::new());
    let connected = Arc::new(Notify::new());
    let keyframe = KeyframeSignal::default();
    let lost = Arc::new(AtomicBool::new(false));

    let pc = PeerConnectionBuilder::new()
        .with_configuration(config)
        .with_media_engine(media_engine)
        .with_interceptor_registry(registry)
        .with_handler(Arc::new(Handler {
            gathering_done: Arc::clone(&gathering_done),
            connected: Arc::clone(&connected),
            keyframe: keyframe.clone(),
            lost: Arc::clone(&lost),
        }))
        .with_udp_addrs(vec!["0.0.0.0:0"])
        .build()
        .await
        .map_err(|e| format!("could not build peer connection: {e}"))?;

    let video = Arc::new(
        TrackLocalStaticSample::new(MediaStreamTrack::new(
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
        ))
        .map_err(|e| format!("could not create video track: {e}"))?,
    );

    let audio = Arc::new(
        TrackLocalStaticSample::new(MediaStreamTrack::new(
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
        ))
        .map_err(|e| format!("could not create audio track: {e}"))?,
    );

    pc.add_track(Arc::clone(&video) as Arc<dyn TrackLocal>)
        .await
        .map_err(|e| format!("could not add video track: {e}"))?;
    pc.add_track(Arc::clone(&audio) as Arc<dyn TrackLocal>)
        .await
        .map_err(|e| format!("could not add audio track: {e}"))?;

    Ok(Session {
        pc,
        video,
        audio,
        video_ssrc,
        audio_ssrc,
        gathering_done,
        connected,
        keyframe,
        lost,
        feedback,
    })
}



impl<P: PeerConnection> Session<P> {

    /// A complete offer with every ICE candidate already embedded. This is the
    /// blob the pairing code maps to — see stage 6.
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

    /// Resolves once the viewer's browser is actually connected.
    pub async fn wait_connected(&self) {
        self.connected.notified().await;
    }

    pub fn keyframe_signal(&self) -> KeyframeSignal {
        self.keyframe.clone()
    }

    /// Whether the connection has reached a state it cannot recover from.
    pub fn lost(&self) -> bool {
        self.lost.load(Ordering::Relaxed)
    }

    /// Everything the viewer has reported about the video stream since this
    /// was last called. Reading clears it — see `bwe::ViewerFeedback`.
    pub fn viewer_feedback(&self) -> Feedback {
        self.feedback.take()
    }

    /// `timestamp_us` is from the shared media clock. It becomes the RTP
    /// timestamp, and getting it right is not optional: leaving
    /// `packet_timestamp` at its default sends every frame stamped zero, and a
    /// decoder handed a stream where no frame is ordered relative to any other
    /// cannot decode anything between keyframes. It will paint each IDR, drop
    /// everything after it, and ask for a new keyframe several times a second.
    pub async fn send_video(
        &self,
        access_unit: &[u8],
        timestamp_us: u64,
        duration: Duration,
    ) -> Result<(), String> {
        self.video
            .write_sample(
                self.video_ssrc,
                VIDEO_PAYLOAD_TYPE,
                &Sample {
                    data: Bytes::copy_from_slice(access_unit),
                    timestamp: SystemInstant::now(),
                    packet_timestamp: rtp_ticks(timestamp_us, VIDEO_CLOCK_HZ),
                    duration,
                    ..Default::default()
                },
                &[],
            )
            .await
            .map_err(|e| format!("could not send video sample: {e}"))
    }

    pub async fn send_audio(
        &self,
        packet: &[u8],
        timestamp_us: u64,
        duration: Duration,
    ) -> Result<(), String> {
        self.audio
            .write_sample(
                self.audio_ssrc,
                AUDIO_PAYLOAD_TYPE,
                &Sample {
                    data: Bytes::copy_from_slice(packet),
                    timestamp: SystemInstant::now(),
                    packet_timestamp: rtp_ticks(timestamp_us, AUDIO_CLOCK_HZ),
                    duration,
                    ..Default::default()
                },
                &[],
            )
            .await
            .map_err(|e| format!("could not send audio sample: {e}"))
    }
}

/// Microseconds from the media clock to ticks of an RTP clock. Wrapping is
/// correct and expected — RTP timestamps are explicitly a 32-bit value that
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
        // per request — a burst of them is exactly what packet loss produces.
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
