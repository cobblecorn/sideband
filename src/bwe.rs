//! Fitting the stream to the connection it actually has.
//!
//! A fixed bitrate is a bet that the viewer's link can carry it, and losing
//! that bet does not degrade gracefully. Send 10 Mbit/s into a 3 Mbit/s link
//! and the bottleneck's queue fills within a second or two: latency climbs,
//! packets start dropping, NACK retransmissions add *more* traffic to a link
//! that already has none to spare, and the connection dies. The picture
//! freezing a few seconds in is the visible part of that.
//!
//! So the send rate has to be discovered rather than assumed. The viewer's
//! browser sends two kinds of evidence back, and they answer different
//! questions:
//!
//!   * **REMB**, the viewer watching packet arrival times and telling us, in
//!     bits per second, what it thinks the path can carry. It notices a
//!     filling queue *before* anything is lost, which is the only signal that
//!     arrives in time to prevent the damage.
//!   * **Receiver reports**, how many packets actually went missing. Slower
//!     and after the fact, but it is ground truth, and it comes back from
//!     every receiver including ones that never send REMB.
//!
//! Both arrive as RTCP, and this module reads them itself rather than through
//! the library's statistics. That is not a preference. `rtc` 0.20.4 defines
//! `process_read_rtcp_for_stats` and never calls it, and the innermost
//! interceptor in the chain discards RTCP instead of forwarding it, so
//! `get_stats` reports zero loss, zero NACKs and zero picture-loss requests no
//! matter what the viewer sends. Believing those numbers would mean believing
//! every connection is perfect, which is exactly the failure this module
//! exists to fix. `FeedbackWatcher` sits in the interceptor chain, where the
//! packets really pass, and counts them there.
//!
//! The controller is deliberately unexciting: come down fast on evidence of
//! trouble, go up slowly and only while actively being told things are calm.
//! Everything it decides is arithmetic over a `Feedback` value, so the whole
//! policy is testable without a network.

use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Instant;

use rtc::interceptor::{interceptor, Interceptor, Packet, StreamInfo, TaggedPacket};
use rtc::rtcp::payload_feedbacks::full_intra_request::FullIntraRequest;
use rtc::rtcp::payload_feedbacks::picture_loss_indication::PictureLossIndication;
use rtc::rtcp::payload_feedbacks::receiver_estimated_maximum_bitrate::ReceiverEstimatedMaximumBitrate;
use rtc::rtcp::receiver_report::ReceiverReport;
use rtc::rtcp::transport_feedbacks::transport_layer_nack::TransportLayerNack;
// The interceptor macros expand to code that names `sansio::Protocol` by
// path, so the module itself has to be in scope, importing the trait alone
// is not enough.
use rtc::sansio;
use rtc::shared::error::Error;

/// Where a session starts before anything is known about the path.
///
/// Not the maximum. Starting at the ceiling and waiting to be told to come
/// down means the first thing a slow connection experiences is the failure
/// this module exists to prevent, by the time the first receiver report
/// arrives, a second of video has already been forced into a queue that could
/// not hold it. Starting here and climbing costs a few soft seconds on a fast
/// link and costs a slow one nothing.
pub const START_BITRATE: u32 = 2_500_000;

/// The floor. Below this the picture is not worth watching, and a link that
/// cannot carry this much cannot carry a screen share at all.
pub const MIN_BITRATE: u32 = 500_000;

/// The ceiling, what a 1080p60 game source can actually use.
pub const MAX_BITRATE: u32 = 10_000_000;

/// Loss above this is congestion and the rate comes down.
///
/// Not zero: wireless links lose a percent or two at rates they carry
/// perfectly well, and NACK repairs that invisibly. Treating ordinary radio
/// loss as congestion would ratchet a good connection down to nothing.
const LOSS_CONGESTED: f64 = 0.10;

/// Below this, the path is considered calm enough to probe upward.
const LOSS_CALM: f64 = 0.02;

/// What the rate is multiplied by each second spent in the band between
/// `LOSS_CALM` and `LOSS_CONGESTED`.
///
/// Holding station in that band, which is what this used to do, assumes
/// something will come along and resolve it. Nothing will: the loss is not
/// heavy enough to trip the congestion rule and not light enough to be
/// invisible, so the session sits at a rate that is quietly shredding four or
/// five percent of its packets for as long as it lasts. Bleeding down slowly
/// finds the rate that stops losing without lurching away from a link that
/// was nearly fine.
const LOSS_BLEED: f64 = 0.95;

/// How far below the actual send rate a receiver's estimate has to sit before
/// it counts as the receiver disagreeing with us, rather than as the ordinary
/// lag of an average that trails its input.
const ESTIMATE_BELOW: f64 = 0.85;

/// How many consecutive seconds of that before the rate comes down.
///
/// Three. One is indistinguishable from the estimate lagging a burst, and at
/// a one second control interval two is still within reach of a burst that
/// straddles a report boundary. A receiver still saying the same thing on the
/// third consecutive second is not lagging, it is disagreeing.
const BELOW_BEFORE_BRAKE: u32 = 3;

/// Per-second increase once the path has proved itself.
///
/// Deliberately unhurried, because overshoot here is not self-correcting. A
/// stream that outruns its link loses packets, and loss too large for NACK to
/// repair needs a keyframe, which this pipeline cannot send (see
/// `stream::KEYFRAME_INTERVAL`). So the viewer does not recover a second
/// later, they stay frozen. Reaching a good link's ceiling twenty seconds
/// later costs a little sharpness for a little while; overshooting a weak one
/// costs the session.
const RAMP: f64 = 1.08;

/// How many consecutive calm seconds before probing upward at all. One clean
/// report right after a loss burst usually means the burst is between reports,
/// not over.
const CALM_BEFORE_RAMP: u32 = 2;

/// The most one interval may take off, whatever the reported loss. Halving
/// every second is already a fast collapse; going further turns a momentary
/// spike into a blank screen.
const MAX_DECREASE: f64 = 0.5;

/// How far a receiver's estimate must sit above what is actually going out
/// before it counts as evidence of calm in its own right.
///
/// A browser's estimate is computed from the traffic it receives and only
/// creeps above it, so the comparison has to be against the real send rate.
/// Comparing against the *target* instead is a trap that closes: hold the rate
/// down and the estimate follows it down, so it can never show the headroom
/// that would allow the rate back up. A stream that spent a while on a still
/// window ends up pinned at the floor and stays there even once the picture is
/// moving again, measured, and the reason this is a rate and not a target.
const ESTIMATE_HEADROOM: f64 = 1.05;

/// How much a receiver's estimate has to fall before it counts as the
/// receiver having detected congestion rather than simply wobbling.
const ESTIMATE_DROP: f64 = 0.98;

/// How much of its budget the encoder has to be using before a receiver's
/// estimate is worth reading at all.
///
/// The estimate is computed from the traffic that arrives, so it describes
/// whatever is being sent, and when a still window is being sent, that is the
/// window, not the link. Below this line nothing about the path is being
/// tested and no reading of the estimate is valid in either direction. Loss
/// reports still are: loss is loss at any rate.
const PUSHING: f64 = 0.5;

/// How far the target may run ahead of what is actually being sent.
///
/// A cheap picture cannot use its budget, so the link is never tested at that
/// rate and neither a quiet loss report nor a receiver estimate says anything
/// about whether it could carry it. Letting the target climb anyway banks
/// permission that was never earned, and spends it all at once the moment the
/// picture gets busy, which is the failure this whole module exists to
/// prevent.
const OVERSHOOT: f64 = 2.0;

/// How fast the record of what a link has actually carried fades.
///
/// Applied per second while sending, so what a busy source proved stays true
/// for roughly half a minute of a quiet one. Long enough to swap to a chat
/// window and back without re-earning the rate; short enough that a stream
/// left on something still does not keep a claim it can no longer support.
const PROVEN_DECAY: f64 = 0.97;

/// How much the ramp accelerates for each calm second beyond the first two,
/// and where it stops.
///
/// A flat rate has to be timid enough for the worst case, which makes climbing
/// out of a low rate take half a minute. Accelerating is safe here because the
/// overshoot cap bounds any single step to what the link has actually carried,
/// and because a single second of trouble resets it to gentle.
const RAMP_ACCELERATION: f64 = 0.04;
const RAMP_MAX: f64 = 1.20;

/// What to leave for audio when treating a receiver estimate as a ceiling.
/// REMB covers everything on the transport, and the number we control is the
/// video encoder's, spending the whole estimate on video would starve the
/// audio it also has to cover. 128 kbit/s of Opus plus RTP overhead.
const AUDIO_ALLOWANCE: u32 = 160_000;

/// Frame-rate ladder, with the band between each rung's thresholds
/// deliberately wide.
///
/// At a low bitrate, 60 fps spends the budget on sixty near-identical pictures
/// and has nothing left to make any of them sharp; half as many frames with
/// twice the bits each is the better trade for watching someone play. So a
/// session that opens at the cautious starting rate opens at 30 fps too, and
/// earns 60 back once there are bits to justify it.
///
/// The gap between dropping and climbing is hysteresis: a target hovering near
/// a boundary must not flip the frame rate back and forth every second.
const FPS_LADDER: &[Rung] = &[
    Rung { fps: 60, drop_below: 3_500_000, climb_above: u32::MAX },
    Rung { fps: 30, drop_below: 1_200_000, climb_above: 5_000_000 },
    Rung { fps: 20, drop_below: 0, climb_above: 2_000_000 },
];

/// How far the picture is shrunk before encoding.
///
/// Bitrate and frame rate are fitted to the link continuously, and this is not.
/// It is chosen once and then held for the rest of the session, which is a
/// deliberate departure from how everything else here works.
///
/// The reason is that a viewer notices this in a way they notice nothing else.
/// A bitrate change is invisible, a frame rate change is nearly so, and a
/// resolution change resizes the picture in front of them. Chasing the link
/// with it, which is what the first version of this did, produced a window
/// that grew and shrank as the rate wandered across a threshold, and that is
/// worse than simply being at the wrong size: a slightly soft picture is
/// something you stop noticing after a minute, and one that keeps resizing is
/// something you never stop noticing.
///
/// Divisors are powers of two because the scaler downsamples by generating
/// mipmaps, and mip levels are exactly the powers of two.
fn divisor_for(bitrate: u32) -> u32 {
    match bitrate {
        b if b >= 3_000_000 => 1,
        b if b >= 1_000_000 => 2,
        _ => 4,
    }
}

/// Intervals spent measuring before the size is first chosen.
///
/// The session opens at full size, because the alternative is guessing before
/// any evidence exists, and a guess that starts small on a fast link is a
/// picture that is needlessly soft for as long as it lasts.
const SETTLE_INTERVALS: u32 = 10;

/// How long every second in a row has to agree before the size moves again.
///
/// This is the whole mechanism, and it is not hysteresis. Hysteresis compares
/// one number against two thresholds, which is fine when the number is steady
/// and useless here, because the rate climbs and brakes every second by design
/// and a link anywhere near a threshold crosses it constantly. That is what
/// made the picture grow and shrink.
///
/// So the question asked is not "where is the rate now" but "what size has
/// every one of the last thirty seconds supported". A rate wandering across a
/// boundary answers that with a disagreement, and nothing moves. Only a link
/// that has genuinely changed can make thirty consecutive seconds agree.
const AGREE_INTERVALS: usize = 30;

/// The least time between one size change and the next.
///
/// A backstop on top of the agreement window. Even if the connection really is
/// swinging between two sustained states, the viewer sees at most one change a
/// minute rather than a picture that keeps rearranging itself.
const COOLDOWN_INTERVALS: u32 = 60;

struct Rung {
    fps: u32,
    /// Step down to the next rung below this bitrate.
    drop_below: u32,
    /// Step up to the rung above at or beyond this bitrate.
    climb_above: u32,
}

// ---------------------------------------------------------------------------
// Reading what the viewer sends back
// ---------------------------------------------------------------------------

/// Everything the viewer's RTCP has said since it was last read.
///
/// Shared between the transport, which parses the packets, and the controller,
/// which acts on them. Every field is read-and-clear: a report is evidence
/// about the interval it arrived in, and counting one twice would mean holding
/// a brake the viewer has already released.
#[derive(Clone)]
pub struct ViewerFeedback(Arc<Counters>);

struct Counters {
    /// Lowest REMB this interval, in bits per second. Zero means none arrived.
    estimate: AtomicU32,
    /// Worst reported loss this interval, as a fraction times 10 000, or
    /// `NO_REPORT`.
    loss: AtomicU32,
    picture_loss: AtomicU32,
    nacks: AtomicU32,
    bytes_sent: AtomicU32,
    /// When the counters were last read, so the rate can be worked out from
    /// the interval that actually elapsed rather than the one assumed.
    since: Mutex<Instant>,
}

/// "No receiver report since the last read", which is a different thing from
/// no loss and must never be confused with it.
const NO_REPORT: u32 = u32::MAX;

/// Fixed-point scale for the stored loss fraction. Four decimal places is far
/// finer than the eight bits a receiver report actually carries.
const LOSS_SCALE: f64 = 10_000.0;

impl Default for ViewerFeedback {
    fn default() -> Self {
        Self::new()
    }
}

impl ViewerFeedback {
    fn new() -> Self {
        Self(Arc::new(Counters {
            estimate: AtomicU32::new(0),
            loss: AtomicU32::new(NO_REPORT),
            picture_loss: AtomicU32::new(0),
            nacks: AtomicU32::new(0),
            bytes_sent: AtomicU32::new(0),
            since: Mutex::new(Instant::now()),
        }))
    }

    /// Records one bandwidth estimate. If several arrive within an interval the
    /// lowest wins, the cautious reading is the safe one.
    fn note_estimate(&self, bits_per_second: u32) {
        // Zero is the sentinel for "nothing reported", so a genuine zero
        // estimate is stored as one bit per second rather than vanishing.
        let bps = bits_per_second.max(1);
        let _ = self.0.estimate.fetch_update(Ordering::Relaxed, Ordering::Relaxed, |current| {
            Some(if current == 0 { bps } else { current.min(bps) })
        });
    }

    /// Records one receiver report's loss fraction. Worst of the interval
    /// wins, for the same reason the lowest estimate does.
    fn note_loss(&self, fraction: f64) {
        let scaled = (fraction.clamp(0.0, 1.0) * LOSS_SCALE) as u32;
        let _ = self.0.loss.fetch_update(Ordering::Relaxed, Ordering::Relaxed, |current| {
            Some(if current == NO_REPORT { scaled } else { current.max(scaled) })
        });
    }

    fn note_picture_loss(&self) {
        self.0.picture_loss.fetch_add(1, Ordering::Relaxed);
    }

    fn note_nack(&self) {
        self.0.nacks.fetch_add(1, Ordering::Relaxed);
    }

    fn note_packet_sent(&self, bytes: u32) {
        self.0.bytes_sent.fetch_add(bytes, Ordering::Relaxed);
    }

    /// Everything since the last call, clearing as it goes.
    pub fn take(&self) -> Feedback {
        let bytes = self.0.bytes_sent.swap(0, Ordering::Relaxed);
        let elapsed = match self.0.since.lock() {
            Ok(mut since) => {
                let elapsed = since.elapsed();
                *since = Instant::now();
                elapsed.as_secs_f64()
            }
            Err(_) => 0.0,
        };
        // A zero-length window would divide to infinity; reporting no rate is
        // the honest answer, and the controller treats it as "not measured".
        let sent_bitrate = if elapsed > 0.05 {
            (bytes as f64 * 8.0 / elapsed) as u32
        } else {
            0
        };

        Feedback {
            sent_bitrate,
            loss: match self.0.loss.swap(NO_REPORT, Ordering::Relaxed) {
                NO_REPORT => None,
                scaled => Some(scaled as f64 / LOSS_SCALE),
            },
            receiver_estimate: match self.0.estimate.swap(0, Ordering::Relaxed) {
                0 => None,
                bps => Some(bps),
            },
            picture_loss: self.0.picture_loss.swap(0, Ordering::Relaxed),
            nacks: self.0.nacks.swap(0, Ordering::Relaxed),
        }
    }
}

/// Counts what the viewer sends back, and what we send it.
///
/// It lives in the interceptor chain because that is the only place either
/// direction is visible: outgoing RTP passes through on its way to the wire,
/// and incoming RTCP passes through and then stops, the chain's innermost
/// layer drops it rather than handing it up. Nothing downstream can tell this
/// layer is here.
#[derive(Interceptor)]
pub struct FeedbackWatcher<P> {
    #[next]
    inner: P,
    feedback: ViewerFeedback,
    /// Only the video stream is measured. Audio loss matters less, Opus
    /// carries in-band FEC, and a lost 20 ms packet is a blip rather than a
    /// broken reference chain, and folding the two together would let a
    /// healthy audio stream mask a video stream in trouble.
    video_ssrc: u32,
}

impl<P> FeedbackWatcher<P> {
    /// A factory for `Registry::with`, together with the handle to read from.
    pub fn layer(video_ssrc: u32) -> (impl FnOnce(P) -> FeedbackWatcher<P>, ViewerFeedback) {
        let feedback = ViewerFeedback::new();
        let handle = feedback.clone();
        (move |inner| FeedbackWatcher { inner, feedback, video_ssrc }, handle)
    }
}

#[interceptor]
impl<P: Interceptor> FeedbackWatcher<P> {
    #[overrides]
    fn handle_read(&mut self, msg: TaggedPacket) -> Result<(), Self::Error> {
        if let Packet::Rtcp(ref packets) = msg.message {
            for packet in packets {
                self.observe(packet.as_any());
            }
        }

        self.inner.handle_read(msg)
    }

    #[overrides]
    fn handle_write(&mut self, msg: TaggedPacket) -> Result<(), Self::Error> {
        // Original transmissions only. Retransmissions are produced further
        // down the chain and never pass through here, which is what makes this
        // the right denominator for a loss fraction.
        if let Packet::Rtp(ref rtp) = msg.message {
            if rtp.header.ssrc == self.video_ssrc {
                // Twelve bytes is the fixed RTP header. Close enough for a
                // ratio against the encoder's target, and it avoids
                // marshalling every packet a second time to find out.
                self.feedback.note_packet_sent(rtp.payload.len() as u32 + 12);
            }
        }

        self.inner.handle_write(msg)
    }
}

impl<P> FeedbackWatcher<P> {
    fn observe(&self, packet: &dyn std::any::Any) {
        if let Some(remb) = packet.downcast_ref::<ReceiverEstimatedMaximumBitrate>() {
            // The wire format is a mantissa and an exponent, so the parsed
            // value is a float that can be enormous, or NaN, or negative
            // through nothing but a malformed packet.
            let bps = remb.bitrate;
            if bps.is_finite() && bps > 0.0 {
                self.feedback.note_estimate(bps.min(u32::MAX as f32) as u32);
            }
        }

        if let Some(rr) = packet.downcast_ref::<ReceiverReport>() {
            for block in &rr.reports {
                if block.ssrc == self.video_ssrc {
                    // Eight bits of fixed-point fraction, per RFC 3550, over
                    // the interval since the receiver's previous report.
                    self.feedback.note_loss(block.fraction_lost as f64 / 256.0);
                }
            }
        }

        if packet
            .downcast_ref::<PictureLossIndication>()
            .is_some_and(|pli| pli.media_ssrc == self.video_ssrc)
        {
            self.feedback.note_picture_loss();
        }

        // A full intra request is a picture-loss request by another name, and
        // says the same thing about the state of the decoder.
        if packet
            .downcast_ref::<FullIntraRequest>()
            .is_some_and(|fir| fir.fir.iter().any(|e| e.ssrc == self.video_ssrc))
        {
            self.feedback.note_picture_loss();
        }

        if packet
            .downcast_ref::<TransportLayerNack>()
            .is_some_and(|nack| nack.media_ssrc == self.video_ssrc)
        {
            self.feedback.note_nack();
        }
    }
}

// ---------------------------------------------------------------------------
// Deciding what to do about it
// ---------------------------------------------------------------------------

/// What the viewer told us about the last interval.
#[derive(Debug, Clone, Copy, Default)]
pub struct Feedback {
    /// The worst loss fraction reported, 0.0-1.0.
    ///
    /// `None` when no receiver report arrived, which is not the same as zero
    /// loss and must not be read as good news.
    pub loss: Option<f64>,
    /// The viewer's estimate of what the path can carry, in bits per second.
    pub receiver_estimate: Option<u32>,
    /// Picture-loss requests. A decoder asking to be rescued is not a decoder
    /// with spare capacity.
    pub picture_loss: u32,
    /// Retransmission requests. Not acted on, a NACK is loss that was
    /// repaired, and the loss report already covers it, but worth seeing.
    pub nacks: u32,
    /// What actually went out over the interval, in bits per second.
    ///
    /// The measured rate, not the target: a still picture uses a fraction of
    /// its budget, and every judgement about the path has to be made against
    /// the traffic that tested it. Zero when nothing was sent, paused, or
    /// between sources.
    pub sent_bitrate: u32,
}

/// What the encoder should be doing.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Target {
    pub bitrate: u32,
    pub fps: u32,
    /// How much to shrink each dimension before encoding. One is untouched.
    /// Fixed for the session once settled, see `divisor_for`.
    pub divisor: u32,
}

/// Rate control, biased towards coming down.
pub struct Controller {
    bitrate: u32,
    /// Position on `FPS_LADDER`, not a frame rate. Holding the index means a
    /// `max_fps` that is not itself a rung caps what is reported without
    /// freezing the ladder, which is what matching on the value would do,
    /// since no rung would ever equal the current frame rate again.
    rung: usize,
    max_fps: u32,
    calm: u32,
    /// The most this link has been seen to actually carry lately, fading as
    /// it ages. What the target is allowed to be built on.
    proven: u32,
    /// The previous receiver estimate, because the direction it moved is what
    /// carries the meaning, see `update`.
    last_estimate: Option<u32>,
    /// The most this session may ever ask for.
    ///
    /// `MAX_BITRATE` unless `SIDEBAND_MAX_BITRATE` says otherwise, in kbit/s.
    /// A deliberate lever rather than a tuning knob: the controller finds the
    /// link's capacity on its own, but "how much of my upload is this allowed
    /// to take" is a question about the rest of the house, not about the link,
    /// and nothing measurable from here can answer it.
    ceiling: u32,
    /// The picture size in force, and how many intervals have been seen.
    divisor: u32,
    intervals: u32,
    /// Intervals since the size last moved, so it cannot move again straight
    /// away however convincing the evidence looks.
    since_change: u32,
    /// Intervals since the link last gave any sign of being the limit.
    ///
    /// The distinction this exists for: a low target rate does not mean a slow
    /// connection. The rate is capped by what the encoder actually spent, and
    /// a still window spends almost nothing, so a browser sitting on a page
    /// looks exactly like a struggling link. Measured on a LAN that could
    /// carry ten times as much, a static window settled at 2.5 Mbit/s and the
    /// picture was shrunk for no reason at all.
    ///
    /// So shrinking asks a second question first: has anything actually gone
    /// wrong. Loss, a receiver asking for less, a picture-loss request. On a
    /// healthy connection the answer is no however cheap the content is, and
    /// the picture is left alone.
    strain: u32,
    /// The recent target rates, oldest first. Bounded at `AGREE_INTERVALS`,
    /// which is what makes "every second agreed" a question this can answer.
    recent: std::collections::VecDeque<u32>,
    /// A size chosen by hand, which is obeyed from the first frame and never
    /// reconsidered. For anyone who would rather pick than be adapted to.
    fixed: Option<u32>,
    /// Consecutive seconds the receiver's estimate has sat below what is
    /// actually being sent. The counterpart to `last_estimate`: one catches an
    /// estimate falling, this one catches an estimate that has already fallen
    /// and stayed there.
    below: u32,
}

impl Controller {
    pub fn new(start: u32, max_fps: u32) -> Self {
        let bitrate = start.clamp(MIN_BITRATE, MAX_BITRATE);
        Self {
            bitrate,
            rung: opening_rung(bitrate),
            max_fps,
            calm: 0,
            proven: 0,
            last_estimate: None,
            below: 0,
            ceiling: env_ceiling(),
            divisor: env_scale().unwrap_or(1),
            intervals: 0,
            since_change: 0,
            strain: u32::MAX,
            recent: std::collections::VecDeque::with_capacity(AGREE_INTERVALS),
            fixed: env_scale(),
        }
    }

    pub fn target(&self) -> Target {
        Target {
            bitrate: self.bitrate,
            fps: FPS_LADDER[self.rung].fps.min(self.max_fps),
            divisor: self.divisor,
        }
    }

    /// Folds one interval's evidence in and returns the new target.
    pub fn update(&mut self, fb: &Feedback) -> Target {
        let mut calm = false;
        let mut trouble = false;

        // What the link was actually asked to carry. Everything below is
        // judged against this rather than against the target, because a
        // budget the encoder did not spend tested nothing.
        let sending = fb.sent_bitrate;
        if sending > 0 {
            self.proven = scale_raw(self.proven, PROVEN_DECAY).max(sending);
        }

        // Two things have to hold before the receiver's estimate is worth
        // acting on: that the encoder was actually using its budget, and that
        // the estimate moved *down*. Either one alone produces false alarms
        // that a real session ran into.
        //
        // What the estimate means is in the direction it moved, not in where
        // it sits.
        //
        // A browser lowers this figure only when it has detected queueing; the
        // rest of the time it simply creeps along above whatever it is being
        // sent, from a smoothed average. So an estimate that happens to sit
        // below the last second's output is the ordinary consequence of the
        // encoder producing a busy second, braking on that is a false alarm,
        // and a costly one: measured on a loopback connection with no possible
        // congestion, it clawed the rate back down every time the picture got
        // interesting.
        //
        // And a falling estimate is not enough on its own either, because it
        // falls just as readily when the *sending* rate falls. Switching to a
        // still window did exactly that, and drove the same session to the
        // floor, which is what `pushing` is there to prevent.
        //
        // Skipped entirely when nothing was sent: an estimate of a stream that
        // was not flowing describes nothing.
        let pushing = sending as f64 >= self.bitrate as f64 * PUSHING;
        if let (Some(estimate), true) = (fb.receiver_estimate, pushing) {
            let fell = self
                .last_estimate
                .is_some_and(|previous| (estimate as f64) < previous as f64 * ESTIMATE_DROP);
            self.last_estimate = Some(estimate);

            let for_video = estimate.saturating_sub(AUDIO_ALLOWANCE);
            let video_sent = sending.saturating_sub(AUDIO_ALLOWANCE).max(1);

            // Two ways to be told the path is worth less than we are
            // spending, and both are needed.
            //
            // A *falling* estimate is the early warning: the receiver has just
            // seen its queue building. It is obeyed on the spot.
            //
            // A *settled* one is the case this used to miss entirely, and it
            // is the one that ends sessions. An estimate only has to fall
            // once; after that it sits there, low and steady, and every
            // second the direction test asks "has it fallen since last time?"
            // the answer is no. So the old rule neither braked nor climbed,
            // and the stream held a rate the receiver had already said, and
            // was still saying, the path could not carry. On a link half the
            // size of the target that is a picture that freezes within
            // seconds and never comes back.
            // A rising estimate is a receiver catching up with us, which is
            // exactly what a healthy one does while the rate climbs, so it
            // never counts against us however far below it currently sits.
            let rising = self.last_estimate.is_some_and(|previous| estimate > previous);
            if !rising && for_video < scale_raw(video_sent, ESTIMATE_BELOW) {
                self.below += 1;
            } else {
                self.below = 0;
            }

            let settled_low = self.below >= BELOW_BEFORE_BRAKE;

            if (fell || settled_low) && for_video < self.bitrate {
                self.bitrate = for_video.max(MIN_BITRATE);
                trouble = true;
            } else if !fell && for_video as f64 >= video_sent as f64 * ESTIMATE_HEADROOM {
                calm = true;
            }
        } else {
            // No estimate, or the encoder was not spending its budget, so
            // there is nothing to compare and the run starts again.
            //
            // Without this reset the count was not consecutive at all, it
            // merely accumulated across whichever scattered seconds happened
            // to qualify, and the next busy second cashed them all in at once.
            // Measured on loopback with zero loss, zero NACKs and zero picture
            // loss: a mostly still window, a handful of quiet seconds, then
            // one 2 Mbit/s burst, and the rate collapsed from 2.5 Mbit/s to
            // the floor. Three seconds has to mean three in a row.
            self.below = 0;
        }

        match fb.loss {
            Some(loss) if loss > LOSS_CONGESTED => {
                self.bitrate = scale(self.bitrate, (1.0 - 0.5 * loss).max(MAX_DECREASE));
                trouble = true;
            }
            // Enough loss to be doing damage, not enough to look like
            // congestion. NACK repairs some of it, and the retransmissions are
            // themselves extra traffic on a link already dropping packets, so
            // holding station here is not the neutral choice it looks like.
            // Come down gently until it stops.
            Some(loss) if loss > LOSS_CALM => {
                self.bitrate = scale(self.bitrate, LOSS_BLEED);
                trouble = true;
            }
            Some(_) => calm = true,
            // No report arrived. Silence is not evidence of calm.
            None => {}
        }

        if fb.picture_loss > 0 {
            trouble = true;
        }

        if trouble {
            self.calm = 0;
        } else if calm {
            self.calm += 1;
        }

        // Climbing needs fresh evidence, not merely a good history. A link
        // that has gone quiet, no reports, no estimates, nothing coming back
        // at all, is precisely where sending more is most likely to be the
        // wrong move, and a run of calm seconds before it must not authorise
        // that.
        if calm && self.calm >= CALM_BEFORE_RAMP {
            self.bitrate = scale(self.bitrate, self.ramp());
        }

        // And whatever the evidence said, the target stays within reach of
        // what this link has actually been carrying. This is the cap that
        // makes the accelerating ramp above safe: a single step can never ask
        // for more than roughly double what already worked.
        if self.proven > 0 {
            let ceiling = scale(self.proven, OVERSHOOT).max(START_BITRATE);
            self.bitrate = self.bitrate.min(ceiling);
        }

        // Applied last, so it caps whatever every rule above arrived at.
        self.bitrate = self.bitrate.min(self.ceiling).max(MIN_BITRATE);

        self.rung = next_rung(self.rung, self.bitrate);

        // Recorded before the size decision reads it. `trouble` is every
        // reason the controller had to hold back this interval, which is
        // exactly the evidence that the link, rather than the content, is
        // what is setting the rate.
        self.strain = if trouble { 0 } else { self.strain.saturating_add(1) };

        self.settle_size();
        self.target()
    }

    /// Moves the picture size, but only on evidence a wandering rate cannot
    /// produce.
    ///
    /// The first decision comes at `SETTLE_INTERVALS`, from whatever the link
    /// turned out to support. After that it can still improve, and still fall
    /// back, but only when every second of the agreement window says the same
    /// thing, and never twice inside the cooldown.
    fn settle_size(&mut self) {
        // Automatic sizing is gone.
        //
        // It was a good idea that cost more than it was worth. Fitting the
        // resolution to the link genuinely buys sharpness on a slow one, and
        // every version of the rule that decided when to change it produced a
        // picture that changed size in front of the viewer. Settling once was
        // not enough, because a link that improves should be allowed to help;
        // allowing that back was not enough either, because the evidence for
        // "it improved" is never clean enough to be sure from.
        //
        // Whatever the theory says, a viewer watching their window resize is
        // worse off than one watching a slightly soft picture, and they said so
        // repeatedly. So the size is whatever `SIDEBAND_SCALE` says and never
        // moves, and the default is not to touch it at all.
        return;

        #[allow(unreachable_code)]
        {
        self.intervals += 1;
        self.since_change += 1;

        if self.recent.len() == AGREE_INTERVALS {
            self.recent.pop_front();
        }
        self.recent.push_back(self.bitrate);

        // The opening decision, taken on whatever is known by then rather than
        // waiting for a full window, because ten seconds of a picture at the
        // wrong size is the thing being fixed.
        if self.intervals == SETTLE_INTERVALS {
            let wanted = divisor_for(self.bitrate);
            if wanted <= self.divisor || self.strained() {
                self.set_divisor(wanted);
            }
            return;
        }
        if self.intervals < SETTLE_INTERVALS || self.since_change < COOLDOWN_INTERVALS {
            return;
        }
        if self.recent.len() < AGREE_INTERVALS {
            return;
        }

        let worst = self.recent.iter().copied().min().unwrap_or(self.bitrate);
        let best = self.recent.iter().copied().max().unwrap_or(self.bitrate);

        // What the *worst* second of the window would support. If even that is
        // a bigger picture than the one being sent, then every second in the
        // window agreed, and the connection has genuinely improved.
        let earned = divisor_for(worst);
        if earned < self.divisor {
            self.set_divisor(self.divisor / 2);
            return;
        }

        // And the other way: if even the *best* second could not support the
        // size currently being sent, nothing in the window agreed with it.
        // Only when the link is what is holding the rate down, though, see
        // `strain`: cheap content is not a reason to shrink anything.
        if divisor_for(best) > self.divisor && self.strained() {
            self.set_divisor(self.divisor * 2);
        }
        }
    }

    /// Whether the link has shown itself to be the limit recently enough to
    /// justify sending a smaller picture.
    fn strained(&self) -> bool {
        self.strain < AGREE_INTERVALS as u32
    }

    fn set_divisor(&mut self, divisor: u32) {
        let divisor = divisor.clamp(1, 4);
        if divisor != self.divisor {
            self.divisor = divisor;
            self.since_change = 0;
        }
    }

    /// How hard to push this second. Gentle at first and bolder the longer
    /// nothing has gone wrong, so climbing out of a rate that a still picture
    /// left behind takes seconds rather than half a minute.
    fn ramp(&self) -> f64 {
        let extra = self.calm.saturating_sub(CALM_BEFORE_RAMP) as f64;
        (RAMP + extra * RAMP_ACCELERATION).min(RAMP_MAX)
    }
}

/// A hand-set ceiling, in kbit/s, or the built in one.
///
/// Read once per session rather than per interval: a value that changed under
/// a running stream would be a rate control input nobody could reproduce.
fn env_ceiling() -> u32 {
    std::env::var("SIDEBAND_MAX_BITRATE")
        .ok()
        .and_then(|v| v.trim().parse::<u32>().ok())
        .map(|kbit| kbit.saturating_mul(1000).clamp(MIN_BITRATE, MAX_BITRATE))
        .unwrap_or(MAX_BITRATE)
}

/// A hand-picked picture size, or nothing.
///
/// `SIDEBAND_SCALE` of 1, 2 or 4, meaning full, half or quarter. Set, it is
/// obeyed from the first frame and the size never changes at all, not even
/// once. Anything else is ignored rather than guessed at.
fn env_scale() -> Option<u32> {
    match std::env::var("SIDEBAND_SCALE").ok()?.trim() {
        "1" => Some(1),
        "2" => Some(2),
        "4" => Some(4),
        _ => None,
    }
}

/// Scales without clamping to the stream's bitrate limits, for figures that
/// are evidence rather than targets.
fn scale_raw(value: u32, factor: f64) -> u32 {
    (value as f64 * factor).round() as u32
}

fn scale(bitrate: u32, factor: f64) -> u32 {
    let scaled = (bitrate as f64 * factor).round();
    (scaled as u32).clamp(MIN_BITRATE, MAX_BITRATE)
}

/// The highest rung the opening bitrate can support, so a session starts at
/// the frame rate it can afford rather than dropping to it a second after the
/// viewer connects.
fn opening_rung(bitrate: u32) -> usize {
    FPS_LADDER
        .iter()
        .position(|rung| bitrate >= rung.drop_below)
        .unwrap_or(FPS_LADDER.len() - 1)
}

/// One step along the ladder, at most, so quality degrades in a way a viewer
/// can follow rather than lurching between extremes.
fn next_rung(at: usize, bitrate: u32) -> usize {
    let rung = &FPS_LADDER[at];

    if bitrate < rung.drop_below && at + 1 < FPS_LADDER.len() {
        return at + 1;
    }
    if bitrate >= rung.climb_above && at > 0 {
        return at - 1;
    }
    at
}

/// Reads the ladder the way the tests talk about it, in frame rates.
#[cfg(test)]
fn next_fps(current: u32, bitrate: u32) -> u32 {
    let at = FPS_LADDER
        .iter()
        .position(|rung| rung.fps == current)
        .expect("test frame rates come from the ladder");
    FPS_LADDER[next_rung(at, bitrate)].fps
}

/// The current target, shared with the thread that owns the encoder.
///
/// The controller runs on the send side and the encoder lives on a capture
/// thread, so the target crosses a thread boundary every second. Two atomics
/// are enough: the encoder reads whichever pair it sees, and a torn read costs
/// at most one frame encoded at the previous frame rate.
#[derive(Clone)]
pub struct Quality(Arc<Shared>);

struct Shared {
    bitrate: AtomicU32,
    fps: AtomicU32,
    divisor: AtomicU32,
}

impl Quality {
    pub fn new(target: Target) -> Self {
        Self(Arc::new(Shared {
            bitrate: AtomicU32::new(target.bitrate),
            fps: AtomicU32::new(target.fps),
            divisor: AtomicU32::new(target.divisor),
        }))
    }

    pub fn set(&self, target: Target) {
        self.0.bitrate.store(target.bitrate, Ordering::Relaxed);
        self.0.fps.store(target.fps, Ordering::Relaxed);
        self.0.divisor.store(target.divisor, Ordering::Relaxed);
    }

    pub fn get(&self) -> Target {
        Target {
            bitrate: self.0.bitrate.load(Ordering::Relaxed),
            fps: self.0.fps.load(Ordering::Relaxed),
            divisor: self.0.divisor.load(Ordering::Relaxed),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A clean second in which the encoder used its whole budget, what a busy
    /// source on a healthy link looks like.
    fn calm_at(sending: u32) -> Feedback {
        Feedback { loss: Some(0.0), sent_bitrate: sending, ..Default::default() }
    }

    fn losing(fraction: f64, sending: u32) -> Feedback {
        Feedback { loss: Some(fraction), sent_bitrate: sending, ..Default::default() }
    }

    fn estimating(bps: u32, sending: u32) -> Feedback {
        Feedback { receiver_estimate: Some(bps), sent_bitrate: sending, ..Default::default() }
    }

    /// Runs the controller through clean seconds with an encoder that keeps up
    /// with whatever budget it is given.
    fn keeping_up(c: &mut Controller, seconds: usize) -> Target {
        let mut target = c.target();
        for _ in 0..seconds {
            target = c.update(&calm_at(target.bitrate));
        }
        target
    }

    #[test]
    fn a_session_opens_at_the_frame_rate_its_bitrate_can_afford() {
        // 2.5 Mbit/s cannot carry 60 fps well, so the stream opens at 30
        // rather than opening at 60 and dropping a second later.
        let c = Controller::new(START_BITRATE, 60);
        assert_eq!(c.target(), Target { bitrate: START_BITRATE, fps: 30, divisor: 1 });

        // And a source capped at 30 is never asked for 60.
        assert_eq!(Controller::new(MAX_BITRATE, 30).target().fps, 30);
    }

    #[test]
    fn a_frame_rate_cap_off_the_ladder_still_lets_it_move() {
        // 45 is not a rung. Reporting min(rung, cap) must not stop the ladder
        // from stepping down when the bitrate collapses, which is what
        // matching rungs by frame-rate value would have done.
        let mut c = Controller::new(MAX_BITRATE, 45);
        assert_eq!(c.target().fps, 45);

        for _ in 0..6 {
            c.update(&losing(0.8, MAX_BITRATE));
        }
        assert_eq!(c.target().fps, 20, "the ladder still descends under the cap");
    }

    #[test]
    fn a_clean_path_climbs_but_not_immediately() {
        let mut c = Controller::new(START_BITRATE, 60);

        // One calm second is not enough on its own, a burst of loss often has
        // a quiet report in the middle of it.
        assert_eq!(c.update(&calm_at(START_BITRATE)).bitrate, START_BITRATE);
        assert!(
            c.update(&calm_at(START_BITRATE)).bitrate > START_BITRATE,
            "sustained calm probes upward"
        );
    }

    #[test]
    fn a_clean_path_reaches_the_ceiling_and_stops() {
        let mut c = Controller::new(START_BITRATE, 60);
        assert_eq!(
            keeping_up(&mut c, 60),
            Target { bitrate: MAX_BITRATE, fps: 60, divisor: 1 }
        );
    }

    #[test]
    fn heavy_loss_backs_off_fast() {
        let mut c = Controller::new(MAX_BITRATE, 60);
        assert!(c.update(&losing(0.4, MAX_BITRATE)).bitrate < MAX_BITRATE);

        // Four seconds of heavy loss must have taken the rate down by more
        // than half, this is the way out of the failure, and it has to be
        // quicker than the failure itself.
        for _ in 0..3 {
            let sending = c.target().bitrate;
            c.update(&losing(0.4, sending));
        }
        assert!(c.target().bitrate < MAX_BITRATE / 2, "got {}", c.target().bitrate);
    }

    #[test]
    fn a_single_interval_never_takes_more_than_half() {
        // Total loss should not empty the budget in one step: a momentary
        // spike would blank the picture rather than soften it.
        let mut c = Controller::new(8_000_000, 60);
        assert_eq!(c.update(&losing(1.0, 8_000_000)).bitrate, 4_000_000);
    }

    #[test]
    fn the_floor_holds() {
        let mut c = Controller::new(MIN_BITRATE, 30);
        for _ in 0..20 {
            c.update(&losing(0.9, MIN_BITRATE));
        }
        assert_eq!(c.target().bitrate, MIN_BITRATE);
    }

    #[test]
    fn mild_loss_bleeds_down_gently_and_recovers() {
        // This band used to hold station, on the reasoning that a few percent
        // is what wireless does at a rate it carries perfectly well and NACK
        // repairs it invisibly. Half of that is true. The half that is not:
        // holding assumes something will come along and resolve it, and
        // nothing will, so the session settles at a rate that quietly sheds
        // four or five percent of its packets for as long as it lasts, with
        // the retransmissions adding load to a link already dropping things.
        // A pipeline that cannot send a keyframe does not get to sit there.
        let mut c = Controller::new(4_000_000, 60);
        for _ in 0..10 {
            c.update(&losing(0.05, 4_000_000));
        }
        let bled = c.target().bitrate;
        assert!(bled < 4_000_000, "should have come down, stayed at {bled}");
        assert!(bled > 2_000_000, "gently, not a collapse, got {bled}");

        // And it is a bleed, not a ratchet: once the loss stops, the rate
        // climbs back rather than being stuck where the bad minute left it.
        for _ in 0..10 {
            c.update(&losing(0.0, bled));
        }
        assert!(
            c.target().bitrate > bled,
            "should climb again once the path is clean, got {}",
            c.target().bitrate
        );
    }

    #[test]
    fn silence_is_not_taken_for_calm() {
        // The bug this guards: reading "no report" as "no loss" would let the
        // rate climb precisely when the link is too congested to get anything
        // back at all.
        let mut c = Controller::new(START_BITRATE, 60);
        for _ in 0..10 {
            c.update(&Feedback { sent_bitrate: START_BITRATE, ..Default::default() });
        }
        assert_eq!(c.target().bitrate, START_BITRATE);
    }

    #[test]
    fn a_link_going_quiet_stops_a_climb_already_under_way() {
        let mut c = Controller::new(START_BITRATE, 60);
        let climbed = keeping_up(&mut c, 4).bitrate;
        assert!(climbed > START_BITRATE);

        // Reports stop arriving. A good history is not licence to keep pushing
        // into a link that has gone silent.
        for _ in 0..5 {
            c.update(&Feedback { sent_bitrate: climbed, ..Default::default() });
        }
        assert_eq!(c.target().bitrate, climbed);
    }

    #[test]
    fn a_receiver_estimate_brakes_when_it_falls_and_not_before() {
        let mut c = Controller::new(6_000_000, 60);

        // One figure on its own carries no direction, and direction is the
        // whole of its meaning, nothing happens on the strength of it.
        assert_eq!(c.update(&estimating(6_000_000, 6_000_000)).bitrate, 6_000_000);

        // Now it falls. The receiver has seen its queue building and said what
        // it thinks the path is worth, and that is obeyed at once, minus the
        // audio the figure also has to cover.
        let braked = c.update(&estimating(2_000_000, 6_000_000)).bitrate;
        assert_eq!(braked, 2_000_000 - AUDIO_ALLOWANCE);

        // And a wildly optimistic figure is not permission to jump back; the
        // climb is still one ramp step at a time.
        let after = c.update(&estimating(50_000_000, braked)).bitrate;
        assert!(after <= scale(braked, RAMP_MAX));
    }

    #[test]
    fn a_burst_the_estimate_has_not_caught_up_with_yet_does_not_brake() {
        // The regression this guards, measured on a loopback connection that
        // could not possibly have been congested: a browser's estimate is a
        // smoothed average of what it has been receiving, so it sits below any
        // second in which the encoder produced a burst. Reading that as
        // congestion clawed the rate back down every time the picture got
        // interesting.
        let mut c = Controller::new(2_000_000, 60);
        c.update(&estimating(900_000, 400_000));

        // A burst, with the estimate climbing after it. Rising is the tell: a
        // receiver catching up is not a receiver objecting.
        for estimate in [1_000_000, 1_150_000] {
            c.update(&estimating(estimate, 1_400_000));
        }
        assert!(c.target().bitrate >= 2_000_000, "got {}", c.target().bitrate);
    }

    #[test]
    fn scattered_low_seconds_do_not_add_up_to_a_brake() {
        // Measured on loopback, with zero loss and zero picture loss: a mostly
        // still window spends most of its seconds below the threshold that
        // makes an estimate worth reading at all, and if those seconds still
        // counted towards the brake, the next busy second cashed in a run that
        // was never consecutive. The rate fell from 2.5 Mbit/s to the floor on
        // a connection that could not possibly have been congested.
        let mut c = Controller::new(2_500_000, 60);

        for _ in 0..6 {
            // Below, but not pushing: nothing was being asked of the link.
            c.update(&estimating(480_000, 100_000));
        }
        // One busy second. It must be judged on its own, not on the six.
        c.update(&estimating(480_000, 2_000_000));

        assert!(
            c.target().bitrate >= 2_000_000,
            "a still window then one burst is not congestion, got {}",
            c.target().bitrate
        );
    }

    #[test]
    fn an_estimate_that_has_settled_below_the_send_rate_brakes() {
        // The counterpart, and the failure that ended real sessions. An
        // estimate only has to fall once. After that it sits there, low and
        // flat, and a rule that asks "did it fall since last time?" answers no
        // for ever, so the stream holds a rate the receiver is still saying
        // the path cannot carry. The viewer freezes within seconds and never
        // comes back, because this pipeline has no working keyframe to repair
        // them with.
        let mut c = Controller::new(6_000_000, 60);
        for _ in 0..4 {
            c.update(&estimating(2_000_000, 6_000_000));
        }
        assert!(
            c.target().bitrate <= 2_000_000,
            "should have come down to what the receiver reported, got {}",
            c.target().bitrate
        );
    }

    #[test]
    fn an_estimate_falling_because_the_picture_got_cheaper_is_not_congestion() {
        // The second false alarm, and the one that swapping sources produces
        // every time: a receiver's estimate follows the traffic, so switching
        // to a still window makes it fall exactly as it would under
        // congestion. Measured, it took a real session to the floor and left
        // it there.
        let mut c = Controller::new(START_BITRATE, 60);
        c.update(&estimating(2_000_000, 2_400_000));

        for _ in 0..8 {
            // Cheap picture; the estimate slides down after it.
            c.update(&estimating(220_000, 260_000));
        }
        assert!(
            c.target().bitrate >= START_BITRATE,
            "a still window must not be read as a slow link, got {}",
            c.target().bitrate
        );
    }

    #[test]
    fn an_estimate_is_judged_against_what_was_actually_sent() {
        // The trap this guards, and the reason the comparison is not against
        // the target: a still picture uses a fraction of its budget, so an
        // estimate that tracks the picture reads far below the target and
        // brakes, every second, for as long as the picture stays still, until
        // the stream sits at the floor and cannot get back up even once there
        // is something to show. Measured on a real session before this.
        let mut c = Controller::new(6_000_000, 60);

        // 800 kbit/s of a static window, with the viewer's estimate
        // comfortably above it: nothing is wrong here.
        for _ in 0..5 {
            c.update(&estimating(900_000, 800_000));
        }
        assert!(
            c.target().bitrate > MIN_BITRATE,
            "a cheap picture must not be read as a slow link, got {}",
            c.target().bitrate
        );
    }

    #[test]
    fn an_estimate_only_matching_the_send_rate_is_not_calm() {
        // A browser's estimate is computed from the traffic it receives, so a
        // figure that merely matches it says nothing about spare capacity.
        let mut c = Controller::new(4_000_000, 60);
        for _ in 0..5 {
            c.update(&estimating(4_000_000, 4_000_000));
        }
        assert_eq!(c.target().bitrate, 4_000_000);
    }

    #[test]
    fn the_target_stays_within_reach_of_what_the_link_has_carried() {
        // Clean reports while the encoder uses a fraction of its budget are
        // not evidence the link could carry the rest. Without this cap the
        // target climbs to the ceiling on a still picture and spends the whole
        // lot the instant it starts moving.
        let mut c = Controller::new(START_BITRATE, 60);
        for _ in 0..40 {
            c.update(&calm_at(700_000));
        }
        assert!(
            c.target().bitrate <= scale(700_000, OVERSHOOT).max(START_BITRATE),
            "got {}",
            c.target().bitrate
        );
    }

    #[test]
    fn a_cheap_source_never_drags_the_rate_below_the_opening_one() {
        // The cap has a floor of the opening rate, so the worst a still window
        // can do is put the stream back where a fresh session would start,
        // never below it.
        let mut c = Controller::new(MAX_BITRATE, 60);
        for _ in 0..60 {
            c.update(&calm_at(200_000));
        }
        assert_eq!(c.target().bitrate, START_BITRATE);
    }

    #[test]
    fn what_a_busy_source_proved_survives_a_spell_on_a_quiet_one() {
        // Swapping to a chat window and back is the case: the link was
        // measured a moment ago and has not changed, so the rate should still
        // be there rather than having to be earned again from scratch.
        let mut c = Controller::new(START_BITRATE, 60);
        let earned = keeping_up(&mut c, 30).bitrate;
        assert!(earned > 5_000_000, "got {earned}");

        for _ in 0..5 {
            c.update(&calm_at(600_000));
        }
        assert!(
            c.target().bitrate > earned / 2,
            "five quiet seconds should not undo it, got {}",
            c.target().bitrate
        );
    }

    #[test]
    fn climbing_gets_bolder_the_longer_nothing_goes_wrong() {
        // A flat ramp timid enough for the worst case takes half a minute to
        // climb out of a low rate. Each further calm second pushes harder,
        // bounded, and any trouble puts it back to gentle.
        let mut c = Controller::new(MIN_BITRATE, 60);
        c.update(&calm_at(MIN_BITRATE));
        let before = c.target().bitrate;
        let early = c.update(&calm_at(before)).bitrate as f64 / before as f64;

        let mut last = c.target().bitrate;
        for _ in 0..6 {
            last = c.update(&calm_at(last)).bitrate;
        }
        let late = c.update(&calm_at(last)).bitrate as f64 / last as f64;

        assert!(late > early, "{late} should be bolder than {early}");
        assert!(late <= RAMP_MAX + 0.001, "and still bounded, got {late}");
    }

    #[test]
    fn one_bad_second_makes_the_ramp_gentle_again() {
        let mut c = Controller::new(MIN_BITRATE, 60);
        keeping_up(&mut c, 10);

        let sending = c.target().bitrate;
        c.update(&losing(0.5, sending));

        // Back to needing two calm seconds before anything moves at all.
        let after_loss = c.target().bitrate;
        c.update(&calm_at(after_loss));
        assert_eq!(c.target().bitrate, after_loss, "one calm second is not enough again");
    }

    #[test]
    fn picture_loss_stops_a_climb() {
        let mut c = Controller::new(START_BITRATE, 60);
        let climbed = keeping_up(&mut c, 2).bitrate;

        let struggling = Feedback {
            loss: Some(0.0),
            picture_loss: 3,
            sent_bitrate: climbed,
            ..Default::default()
        };
        assert_eq!(c.update(&struggling).bitrate, climbed, "a failing decoder gets no more bits");
    }

    #[test]
    fn frame_rate_steps_down_and_back_up_with_hysteresis() {
        // Down at 3.5 Mbit/s, but not back up until 5, a target sitting
        // between the two must not flip every second.
        assert_eq!(next_fps(60, 4_000_000), 60);
        assert_eq!(next_fps(60, 3_000_000), 30);
        assert_eq!(next_fps(30, 4_000_000), 30, "inside the band, stays put");
        assert_eq!(next_fps(30, 5_000_000), 60);

        assert_eq!(next_fps(30, 1_000_000), 20);
        assert_eq!(next_fps(20, 1_500_000), 20);
        assert_eq!(next_fps(20, 2_000_000), 30);
    }

    #[test]
    fn a_hand_set_ceiling_caps_everything_above_it() {
        // The lever, and the thing that must not happen: a ceiling low enough
        // to be useful must still leave a working stream, not clamp its way
        // to zero.
        let mut c = Controller::new(START_BITRATE, 60);
        c.ceiling = 900_000;
        for _ in 0..30 {
            c.update(&losing(0.0, c.target().bitrate));
        }
        assert!(c.target().bitrate <= 900_000, "got {}", c.target().bitrate);
        assert!(c.target().bitrate >= MIN_BITRATE, "got {}", c.target().bitrate);
    }

    #[test]
    fn a_hand_picked_size_is_never_touched() {
        let mut c = Controller::new(START_BITRATE, 60);
        c.fixed = Some(2);
        c.divisor = 2;
        for i in 0..500 {
            c.bitrate = if i % 3 == 0 { 400_000 } else { 9_000_000 };
            c.settle_size();
        }
        assert_eq!(c.divisor, 2);
    }

    #[test]
    fn the_picture_size_never_moves_on_its_own() {
        // Automatic sizing is gone, and this is what says so. Every rule for
        // deciding when to change resolution produced a picture that changed
        // size in front of the viewer, and a viewer watching their window
        // resize is worse off than one watching a slightly soft picture.
        let mut c = Controller::new(START_BITRATE, 60);
        for i in 0..500 {
            c.bitrate = if i % 2 == 0 { 400_000 } else { 9_000_000 };
            c.update(&losing(if i % 3 == 0 { 0.4 } else { 0.0 }, c.bitrate));
            assert_eq!(c.target().divisor, 1, "the size moved at interval {i}");
        }
    }

    #[test]
    fn a_session_opens_at_full_size() {
        // Before there is evidence, guessing small would leave a fast link
        // needlessly soft for as long as the guess lasted.
        let c = Controller::new(START_BITRATE, 60);
        assert_eq!(c.target().divisor, 1);
    }

    #[test]
    fn the_size_thresholds_are_the_ones_that_matter() {
        // Half a megabit at full size is about twelve thousandths of a bit per
        // pixel, which is the mush this exists to prevent.
        assert_eq!(divisor_for(500_000), 4);
        assert_eq!(divisor_for(1_500_000), 2);
        assert_eq!(divisor_for(5_000_000), 1);
    }

    #[test]
    fn frame_rate_moves_one_rung_at_a_time() {
        // A collapse to the floor should not jump 60 to 20 in a single step;
        // each interval takes one rung, so the picture degrades in a way a
        // viewer can follow.
        let mut c = Controller::new(MAX_BITRATE, 60);

        let mut seen = vec![c.target().fps];
        for _ in 0..6 {
            let sending = c.target().bitrate;
            let fps = c.update(&losing(0.8, sending)).fps;
            if *seen.last().unwrap() != fps {
                seen.push(fps);
            }
        }
        assert_eq!(seen, vec![60, 30, 20]);
    }

    #[test]
    fn the_worst_report_and_the_lowest_estimate_in_an_interval_win() {
        let f = ViewerFeedback::new();
        assert_eq!(f.take().loss, None, "nothing reported yet");

        f.note_loss(0.01);
        f.note_loss(0.30);
        f.note_loss(0.02);
        f.note_estimate(4_000_000);
        f.note_estimate(1_500_000);
        f.note_estimate(3_000_000);

        let taken = f.take();
        assert_eq!(taken.loss, Some(0.3));
        assert_eq!(taken.receiver_estimate, Some(1_500_000));

        // And reading clears, so one bad second is not counted twice.
        let after = f.take();
        assert_eq!(after.loss, None);
        assert_eq!(after.receiver_estimate, None);
    }

    #[test]
    fn a_clean_report_is_distinguishable_from_no_report() {
        // The controller treats these two completely differently, so they must
        // not collapse into each other.
        let f = ViewerFeedback::new();
        assert_eq!(f.take().loss, None);

        f.note_loss(0.0);
        assert_eq!(f.take().loss, Some(0.0));
    }

    #[test]
    fn counts_accumulate_and_reset_together() {
        let f = ViewerFeedback::new();
        f.note_picture_loss();
        f.note_picture_loss();
        f.note_nack();
        for _ in 0..5 {
            f.note_packet_sent(1200);
        }

        let taken = f.take();
        assert_eq!((taken.picture_loss, taken.nacks), (2, 1));

        let after = f.take();
        assert_eq!((after.picture_loss, after.nacks), (0, 0));
        assert_eq!(after.sent_bitrate, 0, "and nothing was sent in the second window");
    }

    #[test]
    fn the_send_rate_is_measured_over_the_window_that_elapsed() {
        let f = ViewerFeedback::new();
        // A first read establishes the window; it is too short to measure.
        assert_eq!(f.take().sent_bitrate, 0);

        for _ in 0..100 {
            f.note_packet_sent(1_000);
        }
        std::thread::sleep(std::time::Duration::from_millis(120));

        // 100 KB over roughly an eighth of a second is several Mbit/s. The
        // exact figure depends on the sleep, so this checks the order of
        // magnitude rather than pretending to be precise about wall time.
        let rate = f.take().sent_bitrate;
        assert!((2_000_000..=10_000_000).contains(&rate), "got {rate}");
    }

    #[test]
    fn quality_round_trips_across_the_thread_boundary() {
        let q = Quality::new(Target { bitrate: START_BITRATE, fps: 60, divisor: 1 });
        let reader = q.clone();
        q.set(Target { bitrate: 1_000_000, fps: 30, divisor: 2 });
        assert_eq!(reader.get(), Target { bitrate: 1_000_000, fps: 30, divisor: 2 });
    }
}