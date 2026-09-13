//! WebRTC transport for one viewer session.
//!
//! Responsibilities, in the order they happen:
//!   1. accept the viewer's SDP offer and answer it,
//!   2. ACCEPT the SCTP data channel it opens — without this Firestorm parks in
//!      VOICE_STATE_WAIT_FOR_DATA_CHANNEL forever (llvoicewebrtc.cpp:2974-2992),
//!   3. decode the viewer's Opus to mono PCM for the mixer,
//!   4. encode this listener's own mix back and push the roster down the channel.

use bytes::Bytes;
use parking_lot::Mutex;
use std::sync::atomic::{AtomicBool, AtomicU16, Ordering};
use std::sync::Arc;
use std::time::Duration;

use rtc::interceptor::Registry as InterceptorRegistry;
use rtc::media::Sample;
use rtc::media_stream::MediaStreamTrack;
use rtc::peer_connection::configuration::interceptor_registry::register_default_interceptors;
use rtc::peer_connection::configuration::media_engine::{MediaEngine, MIME_TYPE_OPUS};
use rtc::peer_connection::configuration::RTCConfigurationBuilder;
use rtc::peer_connection::sdp::RTCSessionDescription;
use rtc::peer_connection::transport::RTCIceCandidateInit;
use rtc::rtp_transceiver::rtp_sender::{
    RTCRtpCodec, RTCRtpCodecParameters, RTCRtpCodingParameters, RTCRtpEncodingParameters,
    RtpCodecKind,
};

use webrtc::data_channel::{DataChannel, DataChannelEvent};
use webrtc::media_stream::track_local::static_sample::TrackLocalStaticSample;
use webrtc::media_stream::track_local::TrackLocal;
use webrtc::media_stream::track_remote::{TrackRemote, TrackRemoteEvent};
use webrtc::peer_connection::{
    PeerConnection, PeerConnectionBuilder, PeerConnectionEventHandler, RTCIceGatheringState,
    RTCPeerConnectionState,
};
use webrtc::runtime::Runtime;

use crate::mixer::{FRAME_SAMPLES, SAMPLE_RATE};
use crate::proto;
use crate::room::Session;

/// Payload type we advertise for Opus if we get to choose. The answer is what
/// actually governs, and `negotiated_opus_pt` reads it back out of our own SDP.
const OPUS_PT_PREFERRED: u8 = 111;

/// How long to wait for ICE gathering before answering anyway. The region gives us
/// 10 s for the whole provision call, so this leaves ample headroom.
const GATHER_TIMEOUT: Duration = Duration::from_millis(2500);

/// Largest Opus frame we might be handed. Opus permits up to 120 ms per packet;
/// at 48 kHz mono that is 5760 samples. Sizing the decode buffer for the maximum
/// means a viewer using longer frames cannot overflow it.
const MAX_DECODE_SAMPLES: usize = 5760;

/// Allocates one UDP port per peer connection out of the firewalled media range.
///
/// The range in /etc/nftables.conf is 40000-40999, so it is also the hard ceiling
/// on concurrent sessions. Ports are handed out round-robin and never reclaimed
/// explicitly: a closed peer connection releases its socket, and by the time the
/// counter wraps a thousand sessions later the port is long free.
pub struct PortPool {
    next: AtomicU16,
    lo: u16,
    hi: u16,
}

impl PortPool {
    pub fn new(lo: u16, hi: u16) -> Self {
        PortPool {
            next: AtomicU16::new(lo),
            lo,
            hi,
        }
    }

    pub fn take(&self) -> u16 {
        let span = (self.hi - self.lo + 1) as u32;
        let n = self.next.fetch_add(1, Ordering::Relaxed);
        self.lo + ((n - self.lo) as u32 % span) as u16
    }
}

/// One viewer's live transport, paired with the mixer-facing `Session`.
pub struct Endpoint {
    pub session: Arc<Session>,
    pc: Box<dyn PeerConnection>,
    out_track: Arc<TrackLocalStaticSample>,
    out_ssrc: u32,
    out_pt: u8,
    dc: Arc<Mutex<Option<Arc<dyn DataChannel>>>>,
    encoder: Mutex<opus::Encoder>,
    /// RTP timestamp for the outbound stream, advanced by FRAME_SAMPLES per frame.
    packet_timestamp: Mutex<u32>,
}

impl Endpoint {
    /// Send this listener's mixed 20 ms stereo frame.
    pub async fn send_mix(&self, stereo: &[f32]) -> Result<(), String> {
        let mut buf = vec![0u8; 4000];
        let n = {
            let mut enc = self.encoder.lock();
            enc.encode_float(stereo, &mut buf)
                .map_err(|e| format!("opus encode: {e}"))?
        };
        buf.truncate(n);

        let ts = {
            let mut t = self.packet_timestamp.lock();
            let cur = *t;
            // Opus at 48 kHz advances the RTP clock by one tick per sample.
            *t = cur.wrapping_add(FRAME_SAMPLES as u32);
            cur
        };

        // Sample implements Default, so only the fields that carry meaning here are
        // named; that also keeps us compiling if the struct grows another field.
        let sample = Sample {
            data: Bytes::from(buf),
            timestamp: rtc::shared::time::SystemInstant::now(),
            duration: Duration::from_millis(20),
            packet_timestamp: ts,
            ..Default::default()
        };

        self.out_track
            .sample_writer(self.out_ssrc, self.out_pt)
            .write_sample(&sample)
            .await
            .map_err(|e| format!("write_sample: {e}"))
    }

    /// Push a roster update down the data channel, if it is open yet.
    pub async fn send_roster(&self, json: &str) {
        let dc = self.dc.lock().clone();
        if let Some(dc) = dc {
            if let Err(e) = dc.send_text(json).await {
                log::debug!("roster send failed for {}: {e}", self.session.id);
            }
        }
    }

    pub fn data_channel_open(&self) -> bool {
        self.dc.lock().is_some()
    }

    pub async fn add_ice_candidate(&self, c: &proto::IceCandidate) -> Result<(), String> {
        self.pc
            .add_ice_candidate(RTCIceCandidateInit {
                candidate: c.candidate.clone(),
                sdp_mid: c.sdp_mid.clone(),
                sdp_mline_index: c.sdp_mline_index,
                // The viewer sends neither of these: processIceUpdatesCoro
                // (llvoicewebrtc.cpp:2501-2505) writes only sdpMid, sdpMLineIndex
                // and candidate.
                username_fragment: None,
                url: None,
            })
            .await
            .map_err(|e| format!("add_ice_candidate: {e}"))
    }

    pub async fn close(&self) {
        if let Err(e) = self.pc.close().await {
            log::debug!("close {}: {e}", self.session.id);
        }
    }
}

/// Event handler bridging the WebRTC callbacks into our session state.
struct Handler {
    session: Arc<Session>,
    dc: Arc<Mutex<Option<Arc<dyn DataChannel>>>>,
    runtime: Arc<dyn Runtime>,
    /// Set once ICE gathering finishes, so `establish` knows the answer SDP has
    /// candidates in it. See the comment on GATHER_TIMEOUT for why this matters.
    gathered: Arc<AtomicBool>,
}

#[async_trait::async_trait]
impl PeerConnectionEventHandler for Handler {
    async fn on_ice_gathering_state_change(&self, state: RTCIceGatheringState) {
        if state == RTCIceGatheringState::Complete {
            self.gathered.store(true, Ordering::SeqCst);
        }
    }

    async fn on_connection_state_change(&self, state: RTCPeerConnectionState) {
        log::info!(
            "session {} agent {} connection state {:?}",
            self.session.id,
            self.session.agent_id,
            state
        );
        if matches!(
            state,
            RTCPeerConnectionState::Failed
                | RTCPeerConnectionState::Closed
                | RTCPeerConnectionState::Disconnected
        ) {
            self.session.closed.store(true, Ordering::Relaxed);
        }
    }

    /// The viewer's microphone. Decode to mono and hand frames to the mixer.
    async fn on_track(&self, track: Arc<dyn TrackRemote>) {
        if track.kind().await != RtpCodecKind::Audio {
            return;
        }
        let session = self.session.clone();
        self.runtime.spawn(Box::pin(async move {
            // One decoder per speaker: Opus is stateful, so frames from different
            // senders must never share a decoder.
            let mut decoder = match opus::Decoder::new(SAMPLE_RATE, opus::Channels::Mono) {
                Ok(d) => d,
                Err(e) => {
                    log::error!("opus decoder for {}: {e}", session.id);
                    return;
                }
            };
            let mut pcm = vec![0f32; MAX_DECODE_SAMPLES];

            while let Some(evt) = track.poll().await {
                let TrackRemoteEvent::OnRtpPacket(packet) = evt else {
                    continue;
                };
                if packet.payload.is_empty() {
                    continue; // comfort noise / padding
                }
                // fec=false: we want the real frame, not a reconstruction.
                match decoder.decode_float(&packet.payload, &mut pcm, false) {
                    Ok(n) => {
                        // Split into exact 20 ms frames so the mixer's grid stays
                        // aligned even if the sender uses 40 or 60 ms packets.
                        let mut off = 0;
                        while off + FRAME_SAMPLES <= n {
                            session.push_frame(pcm[off..off + FRAME_SAMPLES].to_vec());
                            off += FRAME_SAMPLES;
                        }
                        if off < n {
                            // Tail shorter than a frame: pad rather than drop, so a
                            // 10 ms sender still produces continuous audio.
                            let mut tail = pcm[off..n].to_vec();
                            tail.resize(FRAME_SAMPLES, 0.0);
                            session.push_frame(tail);
                        }
                    }
                    Err(e) => log::debug!("opus decode for {}: {e}", session.id),
                }
            }
            log::info!("inbound track ended for session {}", session.id);
        }));
    }

    /// The viewer's "SLData" channel. Accepting this is mandatory.
    async fn on_data_channel(&self, data_channel: Arc<dyn DataChannel>) {
        let label = data_channel.label().await.unwrap_or_default();
        log::info!("session {} data channel {:?}", self.session.id, label);

        *self.dc.lock() = Some(data_channel.clone());

        let session = self.session.clone();
        let slot = self.dc.clone();
        self.runtime.spawn(Box::pin(async move {
            while let Some(evt) = data_channel.poll().await {
                match evt {
                    DataChannelEvent::OnMessage(msg) => {
                        // The viewer always sends text JSON here
                        // (llvoicewebrtc.cpp:2672 sendData(..., false)).
                        if !msg.is_string {
                            continue;
                        }
                        let Ok(text) = std::str::from_utf8(&msg.data) else {
                            continue;
                        };
                        match proto::ViewerDataMessage::parse(text) {
                            Ok(m) => session.apply_data(&m),
                            Err(e) => log::debug!("bad data-channel json: {e}: {text}"),
                        }
                    }
                    DataChannelEvent::OnClose | DataChannelEvent::OnError => break,
                    _ => {}
                }
            }
            *slot.lock() = None;
            log::info!("data channel closed for session {}", session.id);
        }));
    }
}

/// Read the payload type the answer actually assigns to Opus.
///
/// The offerer chooses payload numbers, so we must send with the number the viewer
/// expects rather than our own preference. Source of truth is our own answer SDP:
/// RFC 4566 s6 `a=rtpmap:<payload type> <encoding name>/<clock rate>`.
fn negotiated_opus_pt(sdp: &str) -> Option<u8> {
    for line in sdp.lines() {
        let line = line.trim();
        let Some(rest) = line.strip_prefix("a=rtpmap:") else {
            continue;
        };
        let (pt, codec) = rest.split_once(' ')?;
        if codec.to_ascii_lowercase().starts_with("opus/") {
            return pt.parse().ok();
        }
    }
    None
}

/// Bring up a peer connection for a viewer session and answer its offer.
pub async fn establish(
    session: Arc<Session>,
    offer_sdp: &str,
    public_ip: &str,
    port: u16,
    runtime: Arc<dyn Runtime>,
) -> Result<Endpoint, String> {
    // Opus only — this is a voice service and offering anything else just invites
    // the viewer to negotiate something we cannot mix.
    let opus = RTCRtpCodecParameters {
        rtp_codec: RTCRtpCodec {
            mime_type: MIME_TYPE_OPUS.to_owned(),
            clock_rate: SAMPLE_RATE,
            channels: 2,
            sdp_fmtp_line: "minptime=10;useinbandfec=1".to_owned(),
            rtcp_feedback: vec![],
        },
        payload_type: OPUS_PT_PREFERRED,
    };

    let mut media_engine = MediaEngine::default();
    media_engine
        .register_codec(opus.clone(), RtpCodecKind::Audio)
        .map_err(|e| format!("register_codec: {e}"))?;

    let registry = register_default_interceptors(InterceptorRegistry::new(), &mut media_engine)
        .map_err(|e| format!("interceptors: {e}"))?;

    // No ICE servers: we are the answerer on a directly-attached public address,
    // so our host candidates are already routable. The VIEWER has no working STUN
    // either — llvoicewebrtc.cpp:2883 hardcodes stun%d.<grid>.secondlife.io, which
    // does not resolve off Second Life — so connectivity relies on the viewer
    // reaching our host candidate and us learning its peer-reflexive address.
    let config = RTCConfigurationBuilder::new().build();

    let dc_slot: Arc<Mutex<Option<Arc<dyn DataChannel>>>> = Arc::new(Mutex::new(None));
    let gathered = Arc::new(AtomicBool::new(false));
    let handler = Arc::new(Handler {
        session: session.clone(),
        dc: dc_slot.clone(),
        runtime: runtime.clone(),
        gathered: gathered.clone(),
    });

    let out_ssrc: u32 = rand_u32();
    let out_track = Arc::new(
        TrackLocalStaticSample::new(MediaStreamTrack::new(
            format!("wolfvoice-{}", session.id),
            format!("wolfvoice-mix-{}", session.id),
            "wolfvoice-mix".to_owned(),
            RtpCodecKind::Audio,
            vec![RTCRtpEncodingParameters {
                rtp_coding_parameters: RTCRtpCodingParameters {
                    ssrc: Some(out_ssrc),
                    ..Default::default()
                },
                codec: opus.rtp_codec.clone(),
                ..Default::default()
            }],
        ))
        .map_err(|e| format!("TrackLocalStaticSample: {e}"))?,
    );

    let pc = PeerConnectionBuilder::new()
        .with_configuration(config)
        .with_media_engine(media_engine)
        .with_interceptor_registry(registry)
        .with_handler(handler)
        .with_runtime(runtime)
        .with_udp_addrs(vec![format!("{public_ip}:{port}")])
        .build()
        .await
        .map_err(|e| format!("peer connection build: {e}"))?;

    let track: Arc<dyn TrackLocal> = out_track.clone();
    pc.add_track(track)
        .await
        .map_err(|e| format!("add_track: {e}"))?;

    let offer = RTCSessionDescription::offer(offer_sdp.to_owned())
        .map_err(|e| format!("parse offer: {e}"))?;
    pc.set_remote_description(offer)
        .await
        .map_err(|e| format!("set_remote_description: {e}"))?;

    let answer = pc
        .create_answer(None)
        .await
        .map_err(|e| format!("create_answer: {e}"))?;
    pc.set_local_description(answer)
        .await
        .map_err(|e| format!("set_local_description: {e}"))?;

    // WAIT for ICE gathering before reading the answer back.
    //
    // This is not an optimisation, it is required. We are the ANSWERER, and the LL
    // protocol gives the server no way to trickle candidates to the viewer: the
    // viewer POSTs its own candidates to VoiceSignalingRequest and the region
    // discards our reply (WebRtcVoiceRegionModule.cs:320 always answers
    // `<llsd><undef /></llsd>`). So our candidates can only reach the viewer inside
    // the answer SDP, and `local_description()` read immediately after
    // set_local_description contains ice-ufrag/ice-pwd but NO a=candidate lines.
    //
    // Without this the viewer has our credentials and no address. It appeared to
    // work in a loopback test only because the server could reach the client's own
    // host candidates and the client then learned ours peer-reflexively; across the
    // internet, a viewer behind NAT offers only private candidates we cannot reach,
    // so nothing ever connects.
    //
    // The region's HTTP call times out at 10 s (WebUtil.PostToService(..., 10000, true)),
    // so this must stay well under that. If gathering has not finished we return what
    // we have rather than failing the provision outright — a partial candidate list is
    // still better than none.
    let deadline = tokio::time::Instant::now() + GATHER_TIMEOUT;
    while !gathered.load(Ordering::SeqCst) && tokio::time::Instant::now() < deadline {
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    if !gathered.load(Ordering::SeqCst) {
        log::warn!(
            "session {}: ICE gathering did not complete within {:?}; answering with a \
             partial candidate list",
            session.id,
            GATHER_TIMEOUT
        );
    }

    let local = pc
        .local_description()
        .await
        .ok_or_else(|| "no local description after set_local_description".to_string())?;

    // A candidate-free answer is a guaranteed connection failure, so say so loudly
    // rather than letting it look like a viewer problem later.
    if !local.sdp.contains("a=candidate:") {
        log::error!(
            "session {}: answer SDP carries NO ICE candidates — the viewer will have \
             nowhere to send media. Check that {} is the right public address and that \
             the media port range is bindable.",
            session.id,
            public_ip
        );
    }

    let out_pt = negotiated_opus_pt(&local.sdp).unwrap_or(OPUS_PT_PREFERRED);

    let encoder = opus::Encoder::new(
        SAMPLE_RATE,
        opus::Channels::Stereo,
        // Voip rather than Audio: this is speech, and Voip biases the encoder
        // towards intelligibility at low rates.
        opus::Application::Voip,
    )
    .map_err(|e| format!("opus encoder: {e}"))?;

    Ok(Endpoint {
        session,
        pc: Box::new(pc),
        out_track,
        out_ssrc,
        out_pt,
        dc: dc_slot,
        encoder: Mutex::new(encoder),
        packet_timestamp: Mutex::new(rand_u32()),
    })
}

/// Return the SDP we answered with, for handing back to the viewer.
pub async fn answer_sdp(ep: &Endpoint) -> Option<String> {
    ep.pc.local_description().await.map(|d| d.sdp)
}

/// Small non-cryptographic random source for SSRCs and RTP start timestamps.
/// Both are opaque identifiers, not secrets.
fn rand_u32() -> u32 {
    use std::time::{SystemTime, UNIX_EPOCH};
    static COUNTER: std::sync::atomic::AtomicU32 = std::sync::atomic::AtomicU32::new(0);
    let n = COUNTER.fetch_add(0x9E37_79B9, Ordering::Relaxed);
    let t = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.subsec_nanos())
        .unwrap_or(0);
    // Avoid 0, which some stacks treat as unset.
    (n ^ t.rotate_left(13)) | 1
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reads_negotiated_opus_payload_type() {
        let sdp = "v=0\r\n\
                   m=audio 9 UDP/TLS/RTP/SAVPF 111\r\n\
                   a=rtpmap:111 opus/48000/2\r\n\
                   a=fmtp:111 minptime=10\r\n";
        assert_eq!(negotiated_opus_pt(sdp), Some(111));
    }

    #[test]
    fn ignores_non_opus_rtpmaps() {
        let sdp = "a=rtpmap:0 PCMU/8000\r\na=rtpmap:96 VP8/90000\r\n";
        assert_eq!(negotiated_opus_pt(sdp), None);
    }

    #[test]
    fn payload_type_lookup_is_case_insensitive() {
        // Some stacks write "OPUS/48000/2".
        let sdp = "a=rtpmap:96 OPUS/48000/2\r\n";
        assert_eq!(negotiated_opus_pt(sdp), Some(96));
    }

    #[test]
    fn port_pool_stays_inside_the_firewalled_range() {
        let pool = PortPool::new(40000, 40002);
        let mut seen = Vec::new();
        for _ in 0..7 {
            let p = pool.take();
            assert!((40000..=40002).contains(&p), "port {p} escaped the range");
            seen.push(p);
        }
        // It must actually cycle rather than return one port forever.
        assert!(seen.iter().any(|p| *p != seen[0]));
    }

    #[test]
    fn ssrc_is_never_zero() {
        for _ in 0..100 {
            assert_ne!(rand_u32(), 0);
        }
    }
}
