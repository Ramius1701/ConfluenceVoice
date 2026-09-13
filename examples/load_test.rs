//! Capacity measurement: N clients in one room, M of them speaking.
//!
//! The mixer is O(N^2) frame-adds per tick plus one Opus ENCODE per listener per
//! 20 ms, and the encode dominates. That makes the per-listener cost impossible to
//! reason about from first principles, so measure it instead of guessing before
//! deciding how many regions to switch on.
//!
//! Run ON the wolfvoice host:
//!     cargo run --release --example load_test -- https://127.0.0.1:9443 30 10
//!         (endpoint, total clients, speakers)
//!
//! Reports service CPU time consumed during a fixed speaking window, so the figure
//! is directly comparable between runs.

use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

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

#[derive(Default)]
struct Obs {
    connected: AtomicBool,
    dc_open: AtomicBool,
    gathered: AtomicBool,
    rtp: AtomicU32,
    audible: AtomicU32,
}

struct H {
    obs: Arc<Obs>,
    runtime: Arc<dyn Runtime>,
}

#[async_trait::async_trait]
impl PeerConnectionEventHandler for H {
    async fn on_ice_gathering_state_change(&self, s: RTCIceGatheringState) {
        if s == RTCIceGatheringState::Complete {
            self.obs.gathered.store(true, Ordering::SeqCst);
        }
    }
    async fn on_connection_state_change(&self, s: RTCPeerConnectionState) {
        if s == RTCPeerConnectionState::Connected {
            self.obs.connected.store(true, Ordering::SeqCst);
        }
    }
    async fn on_track(&self, track: Arc<dyn TrackRemote>) {
        let obs = self.obs.clone();
        self.runtime.spawn(Box::pin(async move {
            while let Some(e) = track.poll().await {
                if let TrackRemoteEvent::OnRtpPacket(p) = e {
                    obs.rtp.fetch_add(1, Ordering::Relaxed);
                    if p.payload.len() > 10 {
                        obs.audible.fetch_add(1, Ordering::Relaxed);
                    }
                }
            }
        }));
    }
}

struct C {
    pc: Box<dyn PeerConnection>,
    dc: Arc<dyn DataChannel>,
    track: Arc<TrackLocalStaticSample>,
    ssrc: u32,
    pt: u8,
    obs: Arc<Obs>,
}

fn opus(pt: u8) -> RTCRtpCodecParameters {
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

fn pt_from_sdp(sdp: &str) -> Option<u8> {
    for l in sdp.lines() {
        if let Some(r) = l.trim().strip_prefix("a=rtpmap:") {
            let mut it = r.splitn(2, ' ');
            let pt = it.next()?;
            if it.next()?.to_ascii_lowercase().starts_with("opus/") {
                return pt.parse().ok();
            }
        }
    }
    None
}

async fn build(i: usize, endpoint: &str, scene: &str, rt: Arc<dyn Runtime>) -> anyhow::Result<C> {
    let codec = opus(111);
    let mut me = MediaEngine::default();
    me.register_codec(codec.clone(), RtpCodecKind::Audio)?;
    let reg = register_default_interceptors(InterceptorRegistry::new(), &mut me)?;
    let obs = Arc::new(Obs::default());
    let handler = Arc::new(H { obs: obs.clone(), runtime: rt.clone() });

    let ssrc = 0x4000_0000u32 + i as u32;
    let track = Arc::new(TrackLocalStaticSample::new(MediaStreamTrack::new(
        format!("load-{i}"),
        format!("load-track-{i}"),
        "load".into(),
        RtpCodecKind::Audio,
        vec![RTCRtpEncodingParameters {
            rtp_coding_parameters: RTCRtpCodingParameters { ssrc: Some(ssrc), ..Default::default() },
            codec: codec.rtp_codec.clone(),
            ..Default::default()
        }],
    ))?);

    let pc = PeerConnectionBuilder::new()
        .with_configuration(RTCConfigurationBuilder::new().build())
        .with_media_engine(me)
        .with_interceptor_registry(reg)
        .with_handler(handler)
        .with_runtime(rt)
        .with_udp_addrs(vec!["0.0.0.0:0".to_string()])
        .build()
        .await?;

    let local: Arc<dyn TrackLocal> = track.clone();
    pc.add_track(local).await?;
    let dc = pc.create_data_channel("SLData", None).await?;

    let offer = pc.create_offer(None).await?;
    pc.set_local_description(offer).await?;
    for _ in 0..80 {
        if obs.gathered.load(Ordering::SeqCst) { break; }
        tokio::time::sleep(Duration::from_millis(25)).await;
    }
    let ld = pc.local_description().await.ok_or_else(|| anyhow::anyhow!("no local desc"))?;

    // Distinct agent id per client, since the server keys participants on it.
    let agent = format!("{:08x}-1111-1111-1111-111111111111", 0x10AD_0000u32 + i as u32);
    let body = serde_json::json!({
        "jsonrpc":"2.0","id":format!("load-{i}"),
        "method":"provision_voice_account_request",
        "params":{"request":{"jsep":{"type":"offer","sdp":ld.sdp},
            "channel_type":"local","voice_server_type":"webrtc","parcel_local_id":1},
            "userID":agent,"scene":scene}
    });
    let http = reqwest::Client::builder().danger_accept_invalid_certs(true).build()?;
    let resp: serde_json::Value = http.post(endpoint)
        .header("Content-Type","application/json-rpc")
        .json(&body).send().await?.json().await?;
    let result = resp.get("result").ok_or_else(|| anyhow::anyhow!("no result: {resp}"))?;
    let sdp = result.get("jsep").and_then(|j| j.get("sdp")).and_then(|v| v.as_str())
        .ok_or_else(|| anyhow::anyhow!("no answer sdp"))?.to_string();
    let pt = pt_from_sdp(&sdp).unwrap_or(111);
    pc.set_remote_description(RTCSessionDescription::answer(sdp)?).await?;

    {
        let obs2 = obs.clone();
        let dc2 = dc.clone();
        tokio::spawn(async move {
            while let Some(e) = dc2.poll().await {
                match e {
                    DataChannelEvent::OnOpen => obs2.dc_open.store(true, Ordering::SeqCst),
                    DataChannelEvent::OnClose => break,
                    _ => {}
                }
            }
        });
    }

    Ok(C { pc: Box::new(pc), dc, track, ssrc, pt, obs })
}

/// Service CPU time (user+sys) in seconds, read from /proc.
fn service_cpu_secs() -> Option<f64> {
    let out = std::process::Command::new("pgrep").args(["-x", "wolfvoice"]).output().ok()?;
    let pid = String::from_utf8_lossy(&out.stdout).split_whitespace().next()?.to_string();
    let stat = std::fs::read_to_string(format!("/proc/{pid}/stat")).ok()?;
    // utime is field 14, stime 15 (1-indexed), after the comm field in parentheses.
    let tail = stat.rsplit(')').next()?;
    let f: Vec<&str> = tail.split_whitespace().collect();
    let utime: f64 = f.get(11)?.parse().ok()?;
    let stime: f64 = f.get(12)?.parse().ok()?;
    let hz = 100.0; // CONFIG_HZ on Debian
    Some((utime + stime) / hz)
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let mut a = std::env::args().skip(1);
    let endpoint = a.next().unwrap_or_else(|| "https://127.0.0.1:9443".into());
    let total: usize = a.next().and_then(|v| v.parse().ok()).unwrap_or(20);
    let speakers: usize = a.next().and_then(|v| v.parse().ok()).unwrap_or(total / 2);
    let scene = std::env::var("WOLFVOICE_TEST_REGION")
        .unwrap_or_else(|_| "00000000-0000-0000-0000-000000000001".to_string());
    let scene = scene.as_str();
    let rt: Arc<dyn Runtime> = Arc::new(TokioRuntime);

    println!("building {total} clients ({speakers} speaking)...");
    let mut cs = Vec::new();
    for i in 0..total {
        match build(i, &endpoint, scene, rt.clone()).await {
            Ok(c) => cs.push(c),
            Err(e) => { println!("client {i} failed: {e}"); break; }
        }
    }

    // Wait for everyone to connect.
    for _ in 0..150 {
        if cs.iter().all(|c| c.obs.connected.load(Ordering::SeqCst) && c.obs.dc_open.load(Ordering::SeqCst)) { break; }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    let up = cs.iter().filter(|c| c.obs.connected.load(Ordering::SeqCst)).count();
    let dcs = cs.iter().filter(|c| c.obs.dc_open.load(Ordering::SeqCst)).count();
    println!("connected {up}/{}, data channels {dcs}/{}", cs.len(), cs.len());

    // Spread them 5 m apart in a line so distance gain stays at unity.
    for (i, c) in cs.iter().enumerate() {
        let x = 1000.0 + (i as f64) * 5.0;
        let _ = c.dc.send_text(r#"{"j":{"p":true}}"#).await;
        let msg = serde_json::json!({
            "sp":{"x":(x*100.0) as i64,"y":100000,"z":2500},
            "sh":{"x":0,"y":0,"z":0,"w":100},
            "lp":{"x":(x*100.0) as i64,"y":100000,"z":2500},
            "lh":{"x":0,"y":0,"z":0,"w":100}
        });
        let _ = c.dc.send_text(&msg.to_string()).await;
    }
    tokio::time::sleep(Duration::from_millis(500)).await;

    let cpu0 = service_cpu_secs();
    let t0 = Instant::now();
    const SECONDS: usize = 10;

    println!("{speakers} clients speaking for {SECONDS}s...");
    let mut tasks = Vec::new();
    for c in cs.iter().take(speakers) {
        let track = c.track.clone();
        let (ssrc, pt) = (c.ssrc, c.pt);
        tasks.push(tokio::spawn(async move {
            let mut enc = opus::Encoder::new(SAMPLE_RATE, opus::Channels::Mono, opus::Application::Voip).unwrap();
            let mut phase = 0.0f32;
            let step = 2.0 * std::f32::consts::PI * 300.0 / SAMPLE_RATE as f32;
            let mut ts = 0u32;
            for _ in 0..(SECONDS * 50) {
                let mut pcm = vec![0f32; FRAME_SAMPLES];
                for s in pcm.iter_mut() { *s = phase.sin() * 0.3; phase += step; }
                let mut out = vec![0u8; 4000];
                let n = enc.encode_float(&pcm, &mut out).unwrap();
                out.truncate(n);
                let sample = Sample {
                    data: bytes::Bytes::from(out),
                    timestamp: rtc::shared::time::SystemInstant::now(),
                    duration: Duration::from_millis(20),
                    packet_timestamp: ts,
                    ..Default::default()
                };
                ts = ts.wrapping_add(FRAME_SAMPLES as u32);
                let _ = track.sample_writer(ssrc, pt).write_sample(&sample).await;
                tokio::time::sleep(Duration::from_millis(20)).await;
            }
        }));
    }
    for t in tasks { let _ = t.await; }

    let elapsed = t0.elapsed().as_secs_f64();
    let cpu1 = service_cpu_secs();

    let total_rtp: u32 = cs.iter().map(|c| c.obs.rtp.load(Ordering::Relaxed)).sum();
    let total_aud: u32 = cs.iter().map(|c| c.obs.audible.load(Ordering::Relaxed)).sum();

    println!("\n──── RESULTS ────");
    println!("clients up          : {up}");
    println!("speakers            : {speakers}");
    println!("wall time           : {elapsed:.1}s");
    if let (Some(a), Some(b)) = (cpu0, cpu1) {
        let cpu = b - a;
        println!("service CPU         : {cpu:.2}s  ({:.0}% of one core, {:.1}% of 12 cores)",
                 100.0 * cpu / elapsed, 100.0 * cpu / elapsed / 12.0);
        if up > 0 {
            println!("CPU per listener    : {:.1} ms/s", 1000.0 * cpu / elapsed / up as f64);
        }
    } else {
        println!("service CPU         : could not read /proc (run this on the wolfvoice host)");
    }
    // Each listener should receive ~50 packets/s for the whole window.
    let expect = (up as f64) * 50.0 * elapsed;
    println!("rtp received        : {total_rtp} (expected ~{expect:.0}, {:.0}%)",
             100.0 * total_rtp as f64 / expect.max(1.0));
    println!("audible frames      : {total_aud}");

    for c in cs { c.pc.close().await.ok(); }
    Ok(())
}
