//! ConfluenceVoice — Windows-native spatial WebRTC voice for OpenSimulator.
//!
//! Built from [wolfvoice](https://github.com/intelligentwolf/wolfvoice) by Wolf
//! Software Systems Ltd — see NOTICE. The protocol handling, mixer and session
//! logic are unchanged; what differs is packaging: a TOML config file next to the
//! exe instead of environment variables and `/etc` paths, so it runs like any other
//! Windows program.
//!
//! The region never talks WebRTC itself. OpenSim's os-webrtc-janus addon is used
//! purely as a relay: `WebRtcVoice.dll:WebRtcVoiceServiceConnector` forwards the
//! viewer's ProvisionVoiceAccountRequest / VoiceSignalingRequest capabilities to
//! this process as JSON-RPC, and we are the actual WebRTC peer for every viewer.

mod config;
mod mixer;
mod proto;
mod room;
mod session;

use bytes::Bytes;
use hyper::body::HttpBody as _;
use hyper::service::service_fn;
use hyper::{Body, Method, Request, Response, StatusCode};
use serde_json::{json, Value};
use std::collections::HashMap;
use std::net::{IpAddr, SocketAddr};
use std::sync::atomic::Ordering;
use std::sync::Arc;
use std::time::Duration;

use parking_lot::RwLock;

use config::Config;
use room::{RoomKey, Session};
use session::{Endpoint, PortPool};

/// Mixer cadence. One Opus frame per tick per listener.
const TICK: Duration = Duration::from_millis(20);

/// How long a session may sit in WebRTC's Disconnected state with no recovery before
/// it is closed outright. Confirmed live that a real drop normally reaches Failed (and
/// so gets cleaned up) on its own well under this — see session.rs's
/// on_connection_state_change — so this is a backstop for whatever that ordinary path
/// does not cover, not the usual cleanup route. Generous on purpose: closing a
/// connection that was about to recover is worse than leaving a dead one for another
/// half-minute.
const DISCONNECT_GRACE: Duration = Duration::from_secs(60);

/// Largest JSON-RPC body we will read. The biggest legitimate one is an SDP offer
/// plus a trickled ICE candidate list; a few hundred KiB is ample.
const MAX_RPC_BODY: usize = 256 * 1024;

struct App {
    sessions: room::Registry,
    endpoints: RwLock<HashMap<String, Arc<Endpoint>>>,
    ports: PortPool,
    public_ip: String,
    bind_ip: Option<String>,
    max_sessions: usize,
    runtime: Arc<dyn webrtc::runtime::Runtime>,
    /// Empty means accept from anywhere (the previous, only, behaviour).
    allowed_region_ips: Vec<IpAddr>,
    /// Built once from config and handed to every session — see config.rs's
    /// turn_urls/turn_username/turn_credential.
    ice_servers: Vec<rtc::peer_connection::configuration::RTCIceServer>,
}

impl App {
    fn endpoint(&self, id: &str) -> Option<Arc<Endpoint>> {
        self.endpoints.read().get(id).cloned()
    }

    /// Tear a session down completely: transport, registry and room membership,
    /// then tell everyone still in the room that this agent has gone.
    async fn drop_session(&self, id: &str) {
        let ep = self.endpoints.write().remove(id);
        let gone = self.sessions.remove(id);

        if let Some(ep) = ep {
            ep.close().await;
        }

        // Nothing else in the protocol expires a participant, so without this the
        // viewer keeps a departed avatar in its voice panel forever
        // (llvoicewebrtc.cpp:3212-3219 is the only removal path).
        if let Some(gone) = gone {
            // Only announce a departure if this agent has no OTHER session still in
            // the room — an agent legitimately holds one session per region it can
            // hear, and losing a neighbour connection does not mean they left.
            let remaining = self.sessions.members(&gone.room);
            let still_present = remaining.iter().any(|m| m.agent_id == gone.agent_id);
            if !still_present {
                let notice = proto::leave_json(&gone.agent_id);
                for m in remaining {
                    if let Some(ep) = self.endpoint(&m.id) {
                        if ep.data_channel_open() {
                            ep.send_roster(&notice).await;
                        }
                    }
                }
            }
        }

        log::info!("session {id} removed ({} live)", self.sessions.session_count());
    }

    /// ProvisionVoiceAccountRequest.
    ///
    /// Two shapes arrive here: a teardown (`logout`) and a connection request
    /// carrying an SDP offer. Source: llvoicewebrtc.cpp:2731-2734 and :2794-2802.
    async fn provision(&self, params: &proto::RpcParams) -> Result<Value, String> {
        let req = proto::ProvisionRequest::parse(&params.request)?;

        if req.logout {
            let Some(id) = req.viewer_session.clone() else {
                return Err("logout without viewer_session".into());
            };
            self.drop_session(&id).await;
            // The viewer ignores this body (breakVoiceConnectionCoro discards the
            // result at llvoicewebrtc.cpp:2748), but the region-side connector
            // still expects a map.
            return Ok(json!({ "viewer_session": id }));
        }

        let offer = req
            .offer_sdp
            .as_deref()
            .ok_or_else(|| "provision without a jsep offer".to_string())?;

        // A renegotiation for an existing session: drop the old transport first so
        // we never leave two peer connections fighting over one agent.
        if let Some(existing) = req.viewer_session.clone() {
            if self.endpoint(&existing).is_some() {
                log::info!("session {existing} re-provisioning; dropping old transport");
                self.drop_session(&existing).await;
            }
        }

        // Refuse politely rather than exhausting ports or CPU. The viewer treats a
        // failed provision as retryable, so this degrades to "voice unavailable"
        // instead of taking the service down for everyone already connected.
        if self.sessions.session_count() >= self.max_sessions {
            log::warn!(
                "refusing provision: at the {}-session ceiling (agent {})",
                self.max_sessions,
                params.user_id
            );
            return Err("voice service is at capacity".into());
        }

        // Identity comes from the REGION (params.user_id), never from the viewer's
        // own request body. That is the whole security value of routing voice
        // through the capability: the region issued the cap per-agent and told us
        // who it belongs to.
        if params.user_id.is_empty() {
            return Err("region did not supply userID".into());
        }

        let channel_type = req.channel_type.unwrap_or(proto::ChannelType::Local);
        let spatial = channel_type == proto::ChannelType::Local;
        let room = match channel_type {
            proto::ChannelType::Local => RoomKey::Spatial {
                region: params.scene.clone(),
                parcel: req.parcel_local_id,
            },
            proto::ChannelType::MultiAgent => RoomKey::MultiAgent {
                channel: req
                    .channel
                    .clone()
                    .ok_or_else(|| "multiagent request without a channel".to_string())?,
            },
        };

        let id = format!("cv-{}", uuid::Uuid::new_v4());
        let sess = Arc::new(Session::new(
            id.clone(),
            params.user_id.clone(),
            room,
            spatial,
        ));

        let port = self.ports.take();
        let ep = session::establish(
            sess.clone(),
            offer,
            &self.public_ip,
            self.bind_ip.as_deref(),
            port,
            self.runtime.clone(),
            self.ice_servers.clone(),
        )
        .await?;

        let sdp = session::answer_sdp(&ep)
            .await
            .ok_or_else(|| "no answer sdp".to_string())?;

        self.endpoints.write().insert(id.clone(), Arc::new(ep));
        self.sessions.insert(sess);

        // The PARCEL is logged deliberately: spatial rooms are keyed on region +
        // parcel, so "why can these two not hear each other" is almost always
        // answered by them being on different parcels — and that is unanswerable
        // from the log without this field.
        log::info!(
            "session {id} agent {} region {} parcel {} port {port} spatial={spatial} ({} live)",
            params.user_id,
            params.scene,
            req.parcel_local_id,
            self.sessions.session_count()
        );

        Ok(proto::provision_answer(&id, &sdp))
    }

    /// VoiceSignalingRequest — trickled ICE candidates.
    /// Source: llvoicewebrtc.cpp:2496-2519.
    async fn signaling(&self, params: &proto::RpcParams) -> Result<Value, String> {
        let req = proto::SignalingRequest::parse(&params.request);
        let Some(id) = req.viewer_session.clone() else {
            return Err("signaling without viewer_session".into());
        };
        let Some(ep) = self.endpoint(&id) else {
            // Not fatal: the viewer trickles candidates concurrently with the
            // provision call and may reference a session we already tore down.
            return Err(format!("unknown viewer_session {id}"));
        };

        for c in &req.candidates {
            if let Err(e) = ep.add_ice_candidate(c).await {
                log::debug!("session {id} candidate rejected: {e}");
            }
        }
        if req.completed {
            log::debug!("session {id} end-of-candidates");
        }
        Ok(json!({ "viewer_session": id }))
    }
}

/// Mixer loop: one pass per room per 20 ms, spread across all available cores.
async fn mixer_loop(app: Arc<App>) {
    let mut ticker = tokio::time::interval(TICK);
    ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);

    let inflight = Arc::new(std::sync::atomic::AtomicUsize::new(0));
    let mut skipped: u64 = 0;
    let mut last_warned = std::time::Instant::now();

    loop {
        ticker.tick().await;

        if inflight.load(Ordering::Acquire) != 0 {
            skipped += 1;
            // Rate-limit the complaint; at 50 ticks a second an unthrottled log
            // would itself become the bottleneck.
            if last_warned.elapsed() >= Duration::from_secs(10) {
                log::warn!(
                    "mixer overloaded: {skipped} tick(s) skipped in the last 10s \
                     ({} sessions). Audio will break up; the host needs more CPU \
                     or fewer concurrent listeners.",
                    app.sessions.session_count()
                );
                skipped = 0;
                last_warned = std::time::Instant::now();
            }
            continue;
        }

        // Close anything stuck Disconnected past the grace period before reaping —
        // see room::escalate_stale_disconnects for why Disconnected alone is not enough
        // reason to close a session, and session.rs's on_connection_state_change for
        // what normally does close one before this backstop is ever needed.
        let sessions: Vec<Arc<Session>> = app
            .endpoints
            .read()
            .values()
            .map(|ep| ep.session.clone())
            .collect();
        room::escalate_stale_disconnects(&sessions, DISCONNECT_GRACE);

        // Reap anything the transport marked dead before mixing.
        let dead: Vec<String> = app
            .endpoints
            .read()
            .values()
            .filter(|ep| ep.session.closed.load(Ordering::Relaxed))
            .map(|ep| ep.session.id.clone())
            .collect();
        for id in dead {
            app.drop_session(&id).await;
        }

        for key in app.sessions.rooms() {
            let members = app.sessions.members(&key);
            if members.len() < 2 {
                // Nobody to mix for. Still drain the jitter buffers so a lone
                // participant's audio does not pile up until someone joins.
                for m in &members {
                    while m.take_frame().is_some() {}
                    m.level.store(0, Ordering::Relaxed);
                }
                continue;
            }

            let app = app.clone();
            let inflight = inflight.clone();
            inflight.fetch_add(1, Ordering::AcqRel);
            tokio::spawn(async move {
                // Summing is cheap next to the encodes and needs the whole room's
                // frames at once, so it stays inline in the room's own task.
                let outputs = room::mix_room(&members);

                let mut handles = Vec::with_capacity(outputs.len());
                for mut out in outputs {
                    let app = app.clone();
                    handles.push(tokio::spawn(async move {
                        let Some(ep) = app.endpoint(&out.session.id) else {
                            return;
                        };
                        // Nothing can be sent before the data channel opens, and
                        // the viewer is not in VOICE_STATE_SESSION_UP until it does.
                        if !ep.data_channel_open() {
                            return;
                        }
                        // .take() rather than moving the field: ListenerOutput's Drop
                        // impl returns out.stereo to the reuse pool, and a Drop type's
                        // fields cannot be partially moved out.
                        if let Some(roster) = out.roster.take() {
                            // Joins are rare; level updates arrive every 20 ms, so only joins
                            // are logged at info.
                            if roster.contains(r#""j""#) {
                                log::info!("announce to {}: {roster}", out.session.id);
                            }
                            ep.send_roster(&roster).await;
                        }
                        if let Err(e) = ep.send_mix(&out.stereo).await {
                            log::debug!("send_mix {}: {e}", out.session.id);
                        }
                    }));
                }
                for h in handles {
                    let _ = h.await;
                }
                inflight.fetch_sub(1, Ordering::AcqRel);
            });
        }
    }
}

// ─────────────────────────── HTTP / JSON-RPC ───────────────────────────

async fn handle(app: Arc<App>, req: Request<Body>) -> Result<Response<Body>, hyper::Error> {
    // A tiny health endpoint, reachable only from the allow-listed region hosts.
    if req.method() == Method::GET {
        let body = json!({
            "service": "confluencevoice",
            "sessions": app.sessions.session_count(),
            "rooms": app.sessions.rooms().len(),
        });
        return Ok(Response::builder()
            .status(StatusCode::OK)
            .header("content-type", "application/json")
            .body(Body::from(body.to_string()))
            .unwrap());
    }

    if req.method() != Method::POST {
        return Ok(Response::builder()
            .status(StatusCode::METHOD_NOT_ALLOWED)
            .body(Body::empty())
            .unwrap());
    }

    // Bounded read. `to_bytes` would buffer the entire body with no limit, so a
    // single large POST could drive the process' memory unbounded — a trivial
    // denial of service for anyone who can reach the port. The largest legitimate
    // body is an SDP offer plus a trickled candidate list.
    let mut body = req.into_body();
    let mut whole = bytes::BytesMut::new();
    while let Some(chunk) = body.data().await {
        let chunk = chunk?;
        if whole.len() + chunk.len() > MAX_RPC_BODY {
            log::warn!(
                "rejecting oversize JSON-RPC body (>{} bytes) — possible abuse",
                MAX_RPC_BODY
            );
            return Ok(json_200(&proto::rpc_err(&Value::Null, "request body too large")));
        }
        whole.extend_from_slice(&chunk);
    }
    Ok(rpc_response(app, whole.freeze()).await)
}

/// Always answers 200 with a JSON object.
///
/// This is not sloppiness, it is required. OpenSim's connector calls
/// EnsureSuccessStatusCode (WebUtil.cs:428) which throws on any non-2xx, and then
/// WebRtcVoiceServiceConnector.cs:149-160 sets the task result inside the catch and
/// STILL falls through to dereference the now-null response — a NullReferenceException
/// on a detached task, with the viewer left waiting forever. A 200 carrying
/// {"error": ...} is the only way to report a failure that the region can see.
async fn rpc_response(app: Arc<App>, body: Bytes) -> Response<Body> {
    let parsed: Result<proto::RpcRequest, _> = serde_json::from_slice(&body);
    let (id, method, params) = match parsed {
        Ok(r) => (r.id, r.method, r.params),
        Err(e) => {
            return json_200(&proto::rpc_err(&Value::Null, &format!("bad json-rpc: {e}")));
        }
    };

    let result = match method.as_str() {
        proto::METHOD_PROVISION => app.provision(&params).await,
        proto::METHOD_SIGNALING => app.signaling(&params).await,
        other => Err(format!("unknown method {other:?}")),
    };

    match result {
        Ok(v) => json_200(&proto::rpc_ok(&id, v)),
        Err(e) => {
            log::warn!("{method} failed: {e}");
            json_200(&proto::rpc_err(&id, &e))
        }
    }
}

fn json_200(v: &Value) -> Response<Body> {
    Response::builder()
        .status(StatusCode::OK)
        .header("content-type", "application/json")
        .body(Body::from(v.to_string()))
        .unwrap()
}

fn load_tls(cert_path: &std::path::Path, key_path: &std::path::Path) -> Result<rustls::ServerConfig, String> {
    let certs = {
        let f = std::fs::File::open(cert_path)
            .map_err(|e| format!("{}: {e}", cert_path.display()))?;
        let mut r = std::io::BufReader::new(f);
        rustls_pemfile::certs(&mut r)
            .map_err(|e| format!("{}: {e}", cert_path.display()))?
            .into_iter()
            .map(rustls::Certificate)
            .collect::<Vec<_>>()
    };
    if certs.is_empty() {
        return Err(format!("{} contained no certificates", cert_path.display()));
    }

    let key = {
        let f = std::fs::File::open(key_path)
            .map_err(|e| format!("{}: {e}", key_path.display()))?;
        let mut r = std::io::BufReader::new(f);
        // certbot issues ECDSA keys by default in v4, which land in the PKCS#8
        // section, but accept an RSA key too so a key-type change cannot brick us.
        let mut keys = rustls_pemfile::pkcs8_private_keys(&mut r)
            .map_err(|e| format!("{}: {e}", key_path.display()))?;
        if keys.is_empty() {
            let f = std::fs::File::open(key_path)
                .map_err(|e| format!("{}: {e}", key_path.display()))?;
            let mut r = std::io::BufReader::new(f);
            keys = rustls_pemfile::rsa_private_keys(&mut r)
                .map_err(|e| format!("{}: {e}", key_path.display()))?;
        }
        if keys.is_empty() {
            let f = std::fs::File::open(key_path)
                .map_err(|e| format!("{}: {e}", key_path.display()))?;
            let mut r = std::io::BufReader::new(f);
            keys = rustls_pemfile::ec_private_keys(&mut r)
                .map_err(|e| format!("{}: {e}", key_path.display()))?;
        }
        rustls::PrivateKey(
            keys.into_iter()
                .next()
                .ok_or_else(|| format!("{} contained no usable private key", key_path.display()))?,
        )
    };

    rustls::ServerConfig::builder()
        .with_safe_defaults()
        .with_no_client_auth()
        .with_single_cert(certs, key)
        .map_err(|e| format!("tls config: {e}"))
}

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    env_logger::Builder::from_env(env_logger::Env::default().default_filter_or("info")).init();

    let cfg = match Config::load() {
        Ok(c) => c,
        Err(e) => {
            eprintln!("{e}");
            std::process::exit(2);
        }
    };

    let app = Arc::new(App {
        sessions: room::Registry::default(),
        endpoints: RwLock::new(HashMap::new()),
        ports: PortPool::new(cfg.media_port_lo, cfg.media_port_hi),
        public_ip: cfg.public_ip.clone(),
        bind_ip: cfg.bind_ip.clone(),
        max_sessions: cfg.max_sessions,
        runtime: Arc::new(webrtc::runtime::TokioRuntime),
        allowed_region_ips: cfg.allowed_region_ips.clone(),
        ice_servers: if cfg.turn_urls.is_empty() {
            Vec::new()
        } else {
            vec![rtc::peer_connection::configuration::RTCIceServer {
                urls: cfg.turn_urls.clone(),
                username: cfg.turn_username.clone().unwrap_or_default(),
                credential: cfg.turn_credential.clone().unwrap_or_default(),
                ..Default::default()
            }]
        },
    });

    if cfg.allowed_region_ips.is_empty() {
        log::warn!(
            "allowed_region_ips is not set: the JSON-RPC port accepts connections from \
             anywhere. Set it to your region hosts' addresses in confluencevoice.toml \
             once you know them — this port has no other authentication."
        );
    } else {
        log::info!(
            "JSON-RPC port restricted to {} address(es): {:?}",
            cfg.allowed_region_ips.len(),
            cfg.allowed_region_ips
        );
    }
    if !cfg.turn_urls.is_empty() {
        log::info!("TURN relay configured: {:?}", cfg.turn_urls);
    }

    tokio::spawn(mixer_loop(app.clone()));

    let tls = Arc::new(load_tls(&cfg.tls_cert, &cfg.tls_key)?);
    let acceptor = tokio_rustls::TlsAcceptor::from(tls);
    let addr: SocketAddr = cfg.rpc_bind.parse()?;
    let listener = tokio::net::TcpListener::bind(addr).await?;

    match &cfg.bind_ip {
        Some(bind) => log::info!(
            "confluencevoice listening on {addr} (TLS), media {}-{}/udp bound on {bind}, advertised as {}",
            cfg.media_port_lo,
            cfg.media_port_hi,
            cfg.public_ip
        ),
        None => log::info!(
            "confluencevoice listening on {addr} (TLS), media {}-{}/udp on {}",
            cfg.media_port_lo,
            cfg.media_port_hi,
            cfg.public_ip
        ),
    }

    loop {
        let (stream, peer) = match listener.accept().await {
            Ok(v) => v,
            Err(e) => {
                log::warn!("accept: {e}");
                continue;
            }
        };
        if !app.allowed_region_ips.is_empty() && !app.allowed_region_ips.contains(&peer.ip()) {
            log::warn!("rejecting connection from {peer}: not in allowed_region_ips");
            continue;
        }

        let acceptor = acceptor.clone();
        let app = app.clone();
        tokio::spawn(async move {
            let tls_stream = match acceptor.accept(stream).await {
                Ok(s) => s,
                Err(e) => {
                    log::debug!("tls handshake from {peer}: {e}");
                    return;
                }
            };
            let svc = service_fn(move |req| handle(app.clone(), req));
            if let Err(e) = hyper::server::conn::Http::new()
                .http1_only(true)
                .serve_connection(tls_stream, svc)
                .await
            {
                log::debug!("connection from {peer}: {e}");
            }
        });
    }
}
