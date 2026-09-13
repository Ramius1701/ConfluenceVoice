//! End-to-end test: two synthetic viewers against a running wolfvoice.
//!
//! This exists because the service would otherwise ship having never answered a
//! real SDP offer. It exercises the whole path a viewer takes:
//!
//!   1. build an offer with an audio track AND an SCTP data channel,
//!   2. POST it to the JSON-RPC endpoint exactly as OpenSim's
//!      WebRtcVoiceServiceConnector does (same method name, same params shape),
//!   3. apply the answer,
//!   4. confirm the peer connection connects and THE DATA CHANNEL OPENS — that is
//!      the specific thing Janus AudioBridge cannot do and the thing Firestorm
//!      blocks on forever (llvoicewebrtc.cpp:2974-2992),
//!   5. send join + position up the channel,
//!   6. have client A emit a tone and confirm client B receives non-silent RTP,
//!      which proves the per-listener mixer is actually mixing.
//!
//! Run ON the wolfvoice host (the JSON-RPC port is firewalled to the region hosts,
//! but `iif lo accept` permits loopback):
//!     cargo run --example two_clients -- https://127.0.0.1:9443
//!
//! Certificate verification is deliberately disabled: the cert is for
//! the service hostname and we are deliberately dialling 127.0.0.1. This is a test
//! harness, never the service path.

use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};
use std::sync::Arc;
use std::time::Duration;

use rtc::interceptor::Registry as InterceptorRegistry;
use rtc::media::Sample;
use rtc::media_stream::MediaStreamTrack;
use rtc::peer_connection::configuration::interceptor_registry::register_default_interceptors;
use rtc::peer_connection::configuration::media_engine::{MediaEngine, MIME_TYPE_OPUS};
use rtc::peer_connection::configuration::RTCConfigurationBuilder;
use rtc::peer_connection::sdp::RTCSessionDescription;
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
use webrtc::runtime::{Runtime, TokioRuntime};

const SAMPLE_RATE: u32 = 48_000;
const FRAME_SAMPLES: usize = 960;

/// Per-client observations the assertions are made against.
#[derive(Default)]
struct Observed {
    connected: AtomicBool,
    data_channel_open: AtomicBool,
    gathered: AtomicBool,
    /// RTP packets received from the server (i.e. our own mix).
    rtp_packets: AtomicU32,
    /// Packets whose payload was more than a token comfort-noise frame.
    rtp_audible: AtomicU32,
    /// Data-channel messages received (roster updates).
    roster_msgs: AtomicU32,
    /// Set when a roster entry carried the "l":true departure flag.
    saw_leave: AtomicBool,
}

struct Handler {
    name: &'static str,
    obs: Arc<Observed>,
    runtime: Arc<dyn Runtime>,
}

#[async_trait::async_trait]
impl PeerConnectionEventHandler for Handler {
    async fn on_ice_gathering_state_change(&self, state: RTCIceGatheringState) {
        if state == RTCIceGatheringState::Complete {
            self.obs.gathered.store(true, Ordering::SeqCst);
        }
    }

    async fn on_connection_state_change(&self, state: RTCPeerConnectionState) {
        println!("[{}] connection state {state:?}", self.name);
        if state == RTCPeerConnectionState::Connected {
            self.obs.connected.store(true, Ordering::SeqCst);
        }
    }

    async fn on_track(&self, track: Arc<dyn TrackRemote>) {
        let obs = self.obs.clone();
        let name = self.name;
        self.runtime.spawn(Box::pin(async move {
            while let Some(evt) = track.poll().await {
                if let TrackRemoteEvent::OnRtpPacket(p) = evt {
                    obs.rtp_packets.fetch_add(1, Ordering::SeqCst);
                    // Opus silence/comfort-noise frames are a couple of bytes; a real
                    // encoded 20 ms speech frame is far larger. This distinguishes
                    // "the mixer sent us something" from "the mixer sent us audio".
                    if p.payload.len() > 10 {
                        obs.rtp_audible.fetch_add(1, Ordering::SeqCst);
                    }
                }
            }
            println!("[{name}] inbound track ended");
        }));
    }

    async fn on_data_channel(&self, _dc: Arc<dyn DataChannel>) {
        // We are the offerer, so we created the channel; nothing to do here.
    }
}

/// One synthetic viewer.
struct Client {
    pc: Box<dyn PeerConnection>,
    dc: Arc<dyn DataChannel>,
    track: Arc<TrackLocalStaticSample>,
    ssrc: u32,
    pt: u8,
    obs: Arc<Observed>,
    session: String,
}

impl Client {
    /// Shorthand so the assertions read plainly.
    fn saw_leave(&self) -> &AtomicBool {
        &self.obs.saw_leave
    }
}

fn opus_params(pt: u8) -> RTCRtpCodecParameters {
    RTCRtpCodecParameters {
        rtp_codec: RTCRtpCodec {
            mime_type: MIME_TYPE_OPUS.to_owned(),
            clock_rate: SAMPLE_RATE,
            channels: 2,
            sdp_fmtp_line: "minptime=10;useinbandfec=1".to_owned(),
            rtcp_feedback: vec![],
        },
        payload_type: pt,
    }
}

fn opus_pt_from_sdp(sdp: &str) -> Option<u8> {
    for line in sdp.lines() {
        if let Some(rest) = line.trim().strip_prefix("a=rtpmap:") {
            let mut it = rest.splitn(2, ' ');
            let pt = it.next()?;
            if it.next()?.to_ascii_lowercase().starts_with("opus/") {
                return pt.parse().ok();
            }
        }
    }
    None
}

async fn build_client(
    name: &'static str,
    agent_id: &str,
    scene: &str,
    endpoint: &str,
    runtime: Arc<dyn Runtime>,
) -> anyhow::Result<Client> {
    let codec = opus_params(111);
    let mut media_engine = MediaEngine::default();
    media_engine.register_codec(codec.clone(), RtpCodecKind::Audio)?;
    let registry = register_default_interceptors(InterceptorRegistry::new(), &mut media_engine)?;

    let obs = Arc::new(Observed::default());
    let dc_slot = Arc::new(parking_lot::Mutex::new(None));
    let handler = Arc::new(Handler {
        name,
        obs: obs.clone(),
        runtime: runtime.clone(),
    });

    let ssrc: u32 = 0x5EED_0000 ^ (name.len() as u32) ^ (agent_id.len() as u32) << 8;
    let track = Arc::new(TrackLocalStaticSample::new(MediaStreamTrack::new(
        format!("test-{name}"),
        format!("test-track-{name}"),
        "test".to_owned(),
        RtpCodecKind::Audio,
        vec![RTCRtpEncodingParameters {
            rtp_coding_parameters: RTCRtpCodingParameters {
                ssrc: Some(ssrc),
                ..Default::default()
            },
            codec: codec.rtp_codec.clone(),
            ..Default::default()
        }],
    ))?);

    let pc = PeerConnectionBuilder::new()
        .with_configuration(RTCConfigurationBuilder::new().build())
        .with_media_engine(media_engine)
        .with_interceptor_registry(registry)
        .with_handler(handler)
        .with_runtime(runtime.clone())
        .with_udp_addrs(vec!["0.0.0.0:0".to_string()])
        .build()
        .await?;

    let local: Arc<dyn TrackLocal> = track.clone();
    pc.add_track(local).await?;

    // Create the data channel BEFORE the offer, exactly as the viewer does, so the
    // m=application section is present in the offer we send.
    let dc = pc.create_data_channel("SLData", None).await?;

    let offer = pc.create_offer(None).await?;
    pc.set_local_description(offer).await?;

    // Wait for gathering to finish so the offer carries its candidates and we do not
    // need to trickle. Firestorm trickles; this shortcut keeps the harness small
    // without changing what the server has to do with the SDP.
    for _ in 0..100 {
        if obs.gathered.load(Ordering::SeqCst) {
            break;
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }

    let local_desc = pc
        .local_description()
        .await
        .ok_or_else(|| anyhow::anyhow!("no local description"))?;

    // Exactly the envelope WebRtcVoiceServiceConnector.cs:124-140 sends.
    let body = serde_json::json!({
        "jsonrpc": "2.0",
        "id": format!("test-{name}"),
        "method": "provision_voice_account_request",
        "params": {
            "request": {
                "jsep": { "type": "offer", "sdp": local_desc.sdp },
                "channel_type": "local",
                "voice_server_type": "webrtc",
                "parcel_local_id": 0
            },
            "userID": agent_id,
            "scene": scene
        }
    });

    let http = reqwest::Client::builder()
        .danger_accept_invalid_certs(true)
        .build()?;
    let resp: serde_json::Value = http
        .post(endpoint)
        .header("Content-Type", "application/json-rpc")
        .json(&body)
        .send()
        .await?
        .json()
        .await?;

    if let Some(err) = resp.get("error") {
        anyhow::bail!("provision failed: {err}");
    }
    let result = resp
        .get("result")
        .ok_or_else(|| anyhow::anyhow!("no result in {resp}"))?;
    let session = result
        .get("viewer_session")
        .and_then(|v| v.as_str())
        .ok_or_else(|| anyhow::anyhow!("no viewer_session"))?
        .to_string();
    let answer_sdp = result
        .get("jsep")
        .and_then(|j| j.get("sdp"))
        .and_then(|v| v.as_str())
        .ok_or_else(|| anyhow::anyhow!("no answer sdp"))?
        .to_string();

    let pt = opus_pt_from_sdp(&answer_sdp).unwrap_or(111);
    pc.set_remote_description(RTCSessionDescription::answer(answer_sdp)?)
        .await?;

    // Watch the data channel for open + roster traffic.
    {
        let obs2 = obs.clone();
        let dc2 = dc.clone();
        let nm = name;
        runtime.spawn(Box::pin(async move {
            while let Some(evt) = dc2.poll().await {
                match evt {
                    DataChannelEvent::OnOpen => {
                        println!("[{nm}] DATA CHANNEL OPEN");
                        obs2.data_channel_open.store(true, Ordering::SeqCst);
                    }
                    DataChannelEvent::OnMessage(m) => {
                        obs2.roster_msgs.fetch_add(1, Ordering::SeqCst);
                        if let Ok(s) = std::str::from_utf8(&m.data) {
                            println!("[{nm}] roster: {s}");
                            if s.contains("\"l\":true") {
                                obs2.saw_leave.store(true, Ordering::SeqCst);
                            }
                        }
                    }
                    DataChannelEvent::OnClose => break,
                    _ => {}
                }
            }
        }));
    }
    *dc_slot.lock() = Some(dc.clone());

    Ok(Client {
        pc: Box::new(pc),
        dc,
        track,
        ssrc,
        pt,
        obs,
        session,
    })
}

impl Client {
    /// Send join and a position, as the viewer does once the channel opens.
    async fn announce(&self, pos: [f64; 3]) -> anyhow::Result<()> {
        self.dc.send_text(r#"{"j":{"p":true}}"#).await?;
        let s = |v: f64| (v * 100.0) as i64;
        let msg = serde_json::json!({
            "sp": {"x": s(pos[0]), "y": s(pos[1]), "z": s(pos[2])},
            "sh": {"x": 0, "y": 0, "z": 0, "w": 100},
            "lp": {"x": s(pos[0]), "y": s(pos[1]), "z": s(pos[2])},
            "lh": {"x": 0, "y": 0, "z": 0, "w": 100},
        });
        self.dc.send_text(&msg.to_string()).await?;
        Ok(())
    }

    /// Emit `frames` 20 ms frames of a 440 Hz tone.
    async fn speak(&self, frames: usize) -> anyhow::Result<()> {
        let mut enc = opus::Encoder::new(SAMPLE_RATE, opus::Channels::Mono, opus::Application::Voip)?;
        let mut phase = 0.0f32;
        let step = 2.0 * std::f32::consts::PI * 440.0 / SAMPLE_RATE as f32;
        let mut ts: u32 = 0;
        for _ in 0..frames {
            let mut pcm = vec![0f32; FRAME_SAMPLES];
            for s in pcm.iter_mut() {
                *s = phase.sin() * 0.5;
                phase += step;
            }
            let mut out = vec![0u8; 4000];
            let n = enc.encode_float(&pcm, &mut out)?;
            out.truncate(n);
            let sample = Sample {
                data: bytes::Bytes::from(out),
                timestamp: rtc::shared::time::SystemInstant::now(),
                duration: Duration::from_millis(20),
                packet_timestamp: ts,
                ..Default::default()
            };
            ts = ts.wrapping_add(FRAME_SAMPLES as u32);
            self.track
                .sample_writer(self.ssrc, self.pt)
                .write_sample(&sample)
                .await?;
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
        Ok(())
    }
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let endpoint = std::env::args()
        .nth(1)
        .unwrap_or_else(|| "https://127.0.0.1:9443".to_string());
    let scene = std::env::var("WOLFVOICE_TEST_REGION")
        .unwrap_or_else(|_| "00000000-0000-0000-0000-000000000001".to_string());
    let scene = scene.as_str();
    let runtime: Arc<dyn Runtime> = Arc::new(TokioRuntime);

    println!("== provisioning two clients against {endpoint}");
    let a = build_client(
        "A",
        "aaaaaaaa-1111-1111-1111-111111111111",
        scene,
        &endpoint,
        runtime.clone(),
    )
    .await?;
    let b = build_client(
        "B",
        "bbbbbbbb-2222-2222-2222-222222222222",
        scene,
        &endpoint,
        runtime.clone(),
    )
    .await?;
    println!("A session {}\nB session {}", a.session, b.session);

    // Wait for both to connect and open their data channels.
    let mut ok = false;
    for _ in 0..120 {
        if a.obs.connected.load(Ordering::SeqCst)
            && b.obs.connected.load(Ordering::SeqCst)
            && a.obs.data_channel_open.load(Ordering::SeqCst)
            && b.obs.data_channel_open.load(Ordering::SeqCst)
        {
            ok = true;
            break;
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }

    println!("\n== results");
    println!(
        "A connected={} dc_open={}",
        a.obs.connected.load(Ordering::SeqCst),
        a.obs.data_channel_open.load(Ordering::SeqCst)
    );
    println!(
        "B connected={} dc_open={}",
        b.obs.connected.load(Ordering::SeqCst),
        b.obs.data_channel_open.load(Ordering::SeqCst)
    );
    if !ok {
        anyhow::bail!("clients did not both connect with an open data channel");
    }

    // Stand 5 m apart so the distance gain is unity (inside REF_DISTANCE).
    a.announce([1000.0, 1000.0, 25.0]).await?;
    b.announce([1005.0, 1000.0, 25.0]).await?;
    tokio::time::sleep(Duration::from_millis(300)).await;

    println!("\n== A speaks for 1s; B should receive audible mix");
    let before = b.obs.rtp_audible.load(Ordering::SeqCst);
    a.speak(50).await?;
    tokio::time::sleep(Duration::from_millis(500)).await;
    let after = b.obs.rtp_audible.load(Ordering::SeqCst);

    println!(
        "B rtp_packets={} audible_frames={} (delta {}) roster_msgs={}",
        b.obs.rtp_packets.load(Ordering::SeqCst),
        after,
        after - before,
        b.obs.roster_msgs.load(Ordering::SeqCst)
    );
    println!(
        "A rtp_packets={} roster_msgs={}",
        a.obs.rtp_packets.load(Ordering::SeqCst),
        a.obs.roster_msgs.load(Ordering::SeqCst)
    );

    let verdict_audio = after > before;
    let verdict_roster = b.obs.roster_msgs.load(Ordering::SeqCst) > 0;

    // Departure: log A out the way the viewer does (llvoicewebrtc.cpp:2731-2734) and
    // confirm B is told, since nothing else in the protocol expires a participant.
    println!("\n== A logs out; B should receive an \"l\":true notice");
    b.saw_leave().store(false, Ordering::SeqCst);
    let logout = serde_json::json!({
        "jsonrpc": "2.0",
        "id": "test-logout",
        "method": "provision_voice_account_request",
        "params": {
            "request": {
                "logout": true,
                "viewer_session": a.session,
                "voice_server_type": "webrtc"
            },
            "userID": "aaaaaaaa-1111-1111-1111-111111111111",
            "scene": scene
        }
    });
    let http = reqwest::Client::builder()
        .danger_accept_invalid_certs(true)
        .build()?;
    http.post(&endpoint)
        .header("Content-Type", "application/json-rpc")
        .json(&logout)
        .send()
        .await?;

    let mut verdict_leave = false;
    for _ in 0..40 {
        if b.saw_leave().load(Ordering::SeqCst) {
            verdict_leave = true;
            break;
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }

    println!(
        "\nAUDIO_MIXED={} ROSTER_DELIVERED={} LEAVE_NOTIFIED={}",
        verdict_audio, verdict_roster, verdict_leave
    );

    a.pc.close().await.ok();
    b.pc.close().await.ok();

    if !verdict_audio {
        anyhow::bail!("B never received audible mixed audio while A was speaking");
    }
    if !verdict_leave {
        anyhow::bail!("B was never told that A left");
    }
    println!("\nPASS");
    Ok(())
}
