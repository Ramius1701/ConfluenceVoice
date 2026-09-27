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
    /// Empty means accept from anywhere (the previous, only, behaviour). An RwLock
    /// rather than a plain Vec so the admin page's /reload can swap this live —
    /// see admin_handle — without restarting and dropping every connected session.
    allowed_region_ips: RwLock<Vec<IpAddr>>,
    /// Handed to every new session at provision time — see config.rs's
    /// turn_urls/turn_username/turn_credential. Also reloadable via /reload; see
    /// allowed_region_ips above for why this is an RwLock and not a plain Vec.
    ice_servers: RwLock<Vec<rtc::peer_connection::configuration::RTCIceServer>>,
    started_at: std::time::Instant,
    /// Cumulative mixer ticks skipped to an overload, since startup. mixer_loop's own
    /// 10s log warning tracks a separate, resetting count for its own rate-limiting —
    /// this one only ever grows, so the status endpoint can show a real lifetime total.
    total_ticks_skipped: std::sync::atomic::AtomicU64,
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
        // Read the guard's contents out to an owned Vec on its own statement, rather
        // than inline in the call below: a guard borrowed inline as part of a larger
        // expression lives until the end of that statement, which would otherwise
        // hold this non-Send RwLockReadGuard across the .await and make this whole
        // async fn's future not Send.
        let ice_servers = self.ice_servers.read().clone();
        let ep = session::establish(
            sess.clone(),
            offer,
            &self.public_ip,
            self.bind_ip.as_deref(),
            port,
            self.runtime.clone(),
            ice_servers,
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
            app.total_ticks_skipped.fetch_add(1, Ordering::Relaxed);
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
        // Reachable only from wherever allowed_region_ips (if set) permits — the same
        // TCP-accept-time check covers this endpoint too, not just POST — so exposing
        // configuration shape (not secrets or the list contents themselves) here is no
        // worse than console access already implies.
        let body = json!({
            "service": "confluencevoice",
            "version": env!("CARGO_PKG_VERSION"),
            "uptime_secs": app.started_at.elapsed().as_secs(),
            "sessions": app.sessions.session_count(),
            "max_sessions": app.max_sessions,
            "rooms": app.sessions.rooms().len(),
            "mixer_ticks_skipped_total": app.total_ticks_skipped.load(Ordering::Relaxed),
            "ip_allow_list_active": !app.allowed_region_ips.read().is_empty(),
            "turn_configured": !app.ice_servers.read().is_empty(),
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

/// Binds with retries when the port is briefly still held by a just-exited process —
/// specifically the race in admin_handle's /restart: the new instance can start trying
/// to bind before the old one has fully released its sockets. Not needed on a genuine
/// first-ever startup, where it just succeeds on the first attempt; harmless there too.
/// The same "wait for the port to actually go quiet" principle used by hand all
/// session for the real OpenSim region restarts, just automated here.
async fn bind_with_retry(addr: SocketAddr, what: &str) -> std::io::Result<tokio::net::TcpListener> {
    let deadline = tokio::time::Instant::now() + Duration::from_secs(10);
    loop {
        match tokio::net::TcpListener::bind(addr).await {
            Ok(l) => return Ok(l),
            Err(e) if e.kind() == std::io::ErrorKind::AddrInUse && tokio::time::Instant::now() < deadline => {
                log::warn!("{what} bind on {addr} still in use (previous instance still exiting?), retrying: {e}");
                tokio::time::sleep(Duration::from_millis(300)).await;
            }
            Err(e) => return Err(e),
        }
    }
}

// ─────────────────────────── Admin page ───────────────────────────
//
// Plain HTTP (no TLS), separate from the voice port and separate from
// allowed_region_ips. There is no login — reachability IS the authorization
// boundary, which is only safe because config.rs defaults admin_bind to
// 127.0.0.1. Stop and Restart are the two operations that make sense for a
// program to offer about itself: a genuinely stopped process cannot serve a
// "start" request, since nothing would be listening to receive it — that needs
// an external supervisor instead (see the on-hold Windows Service wrapper).

/// Serves the admin page until the process exits. Errors here (a bad bind address,
/// the port already in use) disable the admin page rather than taking down voice —
/// this is a convenience on top of the real service, not a dependency of it.
async fn admin_server(app: Arc<App>, bind: String) {
    let addr: SocketAddr = match bind.parse() {
        Ok(a) => a,
        Err(e) => {
            log::error!("admin_bind {bind:?} is not a valid address: {e}; admin page disabled");
            return;
        }
    };
    let listener = match bind_with_retry(addr, "admin page").await {
        Ok(l) => l,
        Err(e) => {
            log::error!("could not bind admin page on {addr}: {e}; admin page disabled");
            return;
        }
    };
    if !addr.ip().is_loopback() {
        log::warn!(
            "admin page bound to {addr}, which is NOT loopback-only — anyone who can reach \
             this address can stop or restart voice for everyone, with no login. Restrict \
             this at the firewall, or use an SSH tunnel/VPN instead of widening it directly."
        );
    }
    log::info!("admin page on http://{addr}/");

    loop {
        let (stream, peer) = match listener.accept().await {
            Ok(v) => v,
            Err(e) => {
                log::warn!("admin accept: {e}");
                continue;
            }
        };
        let app = app.clone();
        tokio::spawn(async move {
            let svc = service_fn(move |req| admin_handle(app.clone(), req));
            if let Err(e) = hyper::server::conn::Http::new()
                .http1_only(true)
                .serve_connection(stream, svc)
                .await
            {
                log::debug!("admin connection from {peer}: {e}");
            }
        });
    }
}

async fn admin_handle(app: Arc<App>, req: Request<Body>) -> Result<Response<Body>, hyper::Error> {
    let resp = match (req.method(), req.uri().path()) {
        (&Method::GET, "/") => admin_status_page(&app),
        (&Method::POST, "/stop") => {
            log::warn!("admin: stop requested");
            admin_exit_shortly();
            admin_plain_response("Stopping. This page will stop responding shortly.")
        }
        (&Method::POST, "/restart") => {
            log::warn!("admin: restart requested");
            match std::env::current_exe() {
                Ok(exe) => match std::process::Command::new(&exe).spawn() {
                    Ok(child) => log::info!("admin: spawned new instance, pid {}", child.id()),
                    Err(e) => log::error!("admin: failed to spawn new instance: {e}; not exiting"),
                },
                Err(e) => log::error!("admin: current_exe failed: {e}; not exiting"),
            }
            admin_exit_shortly();
            admin_plain_response("Restarting. Reload this page in a few seconds.")
        }
        (&Method::POST, "/kick") => match query_param(req.uri(), "id") {
            Some(id) => {
                log::warn!("admin: kick requested for session {id}");
                app.drop_session(&id).await;
                admin_redirect_to_root()
            }
            None => Response::builder()
                .status(StatusCode::BAD_REQUEST)
                .body(Body::from("missing id"))
                .unwrap(),
        },
        (&Method::POST, "/reload") => match Config::load() {
            Ok(cfg) => {
                let ip_count = cfg.allowed_region_ips.len();
                let turn_on = !cfg.turn_urls.is_empty();
                *app.allowed_region_ips.write() = cfg.allowed_region_ips;
                *app.ice_servers.write() = build_ice_servers(&cfg.turn_urls, cfg.turn_username, cfg.turn_credential);
                log::warn!(
                    "admin: config reloaded — allow-list {} address(es), TURN {}",
                    ip_count,
                    if turn_on { "configured" } else { "not configured" }
                );
                admin_plain_response(
                    "Reloaded confluencevoice.toml. Only the IP allow-list and TURN settings \
                     were applied live — everything else needs a restart to take effect.",
                )
            }
            Err(e) => {
                log::error!("admin: reload failed, nothing was changed: {e}");
                admin_plain_response(&format!(
                    "Reload failed, nothing was changed:<br><pre>{}</pre>",
                    html_escape(&e)
                ))
            }
        },
        _ => Response::builder()
            .status(StatusCode::NOT_FOUND)
            .body(Body::empty())
            .unwrap(),
    };
    Ok(resp)
}

/// Reads one query-string parameter from a request URI. Our own session ids
/// (`cv-<uuid>`) never need percent-decoding, so this stays deliberately simple.
fn query_param(uri: &hyper::Uri, key: &str) -> Option<String> {
    uri.query()?.split('&').find_map(|pair| {
        let (k, v) = pair.split_once('=')?;
        (k == key).then(|| v.to_string())
    })
}

fn admin_redirect_to_root() -> Response<Body> {
    Response::builder()
        .status(StatusCode::SEE_OTHER)
        .header("location", "/")
        .body(Body::empty())
        .unwrap()
}

/// Exits the process shortly after this call returns, not immediately: the caller
/// still needs to hand its HTTP response back to hyper so the browser actually sees
/// a confirmation, rather than the connection just dying mid-request.
fn admin_exit_shortly() {
    tokio::spawn(async {
        tokio::time::sleep(Duration::from_millis(300)).await;
        std::process::exit(0);
    });
}

/// Builds the ICE-server list handed to viewers from a loaded config's TURN fields.
/// Shared between startup and admin_handle's /reload so both apply the same rule:
/// no turn_urls means no relay offered at all.
fn build_ice_servers(
    turn_urls: &[String],
    turn_username: Option<String>,
    turn_credential: Option<String>,
) -> Vec<rtc::peer_connection::configuration::RTCIceServer> {
    if turn_urls.is_empty() {
        Vec::new()
    } else {
        vec![rtc::peer_connection::configuration::RTCIceServer {
            urls: turn_urls.to_vec(),
            username: turn_username.unwrap_or_default(),
            credential: turn_credential.unwrap_or_default(),
        }]
    }
}

/// Escapes the handful of characters that matter in HTML text content. Used for
/// anything on the admin page that ultimately came from the network (agent ids,
/// room names, log message text) rather than a literal we wrote ourselves.
fn html_escape(s: &str) -> String {
    s.replace('&', "&amp;").replace('<', "&lt;").replace('>', "&gt;")
}

/// First 8 characters of the UUID half of a session id, for a compact admin-page
/// column — the full id is still what /kick's query string carries.
fn short_id(id: &str) -> &str {
    id.strip_prefix("cv-").and_then(|s| s.get(..8)).unwrap_or(id)
}

/// Renders the live session list for the admin page, one row per participant with
/// a Kick button that calls /kick — see admin_handle. Not gated on anything the
/// mixer needs; room::Registry::all exists purely for this.
fn sessions_table_html(app: &App) -> String {
    let mut sessions = app.sessions.all();
    sessions.sort_by_key(|s| s.created_at);
    if sessions.is_empty() {
        return "<p>No active sessions.</p>".to_string();
    }
    let mut html = String::from(
        "<table><tr><th>Agent</th><th>Session</th><th>Room</th><th>Type</th>\
         <th>Joined</th><th>Channel</th><th>Connected</th><th></th></tr>",
    );
    for s in &sessions {
        html.push_str(&format!(
            "<tr><td>{agent}</td><td><code>{sid}</code></td><td>{room}</td><td>{kind}</td>\
             <td>{joined}</td><td>{dc}</td><td>{secs} s</td><td>\
             <form method=\"post\" action=\"/kick?id={id}\" \
             onsubmit=\"return confirm('Disconnect {agent}? They will need to reconnect.');\">\
             <button type=\"submit\" class=\"danger\">Kick</button></form></td></tr>",
            agent = html_escape(&s.agent_id),
            sid = short_id(&s.id),
            room = html_escape(&s.room.to_string()),
            kind = if s.spatial { "spatial" } else { "multiagent" },
            joined = if s.joined.load(Ordering::Relaxed) { "yes" } else { "no" },
            dc = if s.dc_open.load(Ordering::Relaxed) { "yes" } else { "no" },
            secs = s.created_at.elapsed().as_secs(),
            id = s.id,
        ));
    }
    html.push_str("</table>");
    html
}

/// Renders the recent-warnings-and-errors panel — see LOG_RING / RingLogger below.
fn log_tail_html() -> String {
    let lines = recent_log_lines();
    if lines.is_empty() {
        return "<p>No warnings or errors since startup.</p>".to_string();
    }
    let body = lines
        .iter()
        .map(|l| html_escape(l))
        .collect::<Vec<_>>()
        .join("\n");
    format!(
        "<pre style=\"max-height:16em; overflow:auto; background:#f4f4f4; \
         padding:0.6em; border:1px solid #ccc; white-space:pre-wrap; \
         font-size:0.85em;\">{body}</pre>"
    )
}

fn admin_plain_response(msg: &str) -> Response<Body> {
    Response::builder()
        .status(StatusCode::OK)
        .header("content-type", "text/html; charset=utf-8")
        .body(Body::from(format!(
            "<!doctype html><meta charset=utf-8><body style=font-family:sans-serif>{msg} \
             <a href=/>Back</a></body>"
        )))
        .unwrap()
}

fn admin_status_page(app: &App) -> Response<Body> {
    let html = format!(
        r#"<!doctype html>
<html><head><meta charset="utf-8"><title>ConfluenceVoice admin</title>
<style>
body {{ font-family: system-ui, sans-serif; max-width: 40em; margin: 2em auto; padding: 0 1em; }}
table {{ border-collapse: collapse; width: 100%; margin-bottom: 1.5em; }}
th, td {{ text-align: left; padding: 0.3em 0.6em; border-bottom: 1px solid #ccc; }}
form {{ display: inline-block; margin-right: 0.6em; }}
button {{ padding: 0.5em 1.2em; font-size: 1em; cursor: pointer; }}
.danger {{ background: #c0392b; color: white; border: 1px solid #a03024; }}
h2 {{ font-size: 1.1em; margin-top: 1.5em; }}
</style></head>
<body>
<h1>ConfluenceVoice</h1>
<table>
<tr><th>Version</th><td>{version}</td></tr>
<tr><th>Uptime</th><td>{uptime_secs} s</td></tr>
<tr><th>Sessions</th><td>{sessions} / {max_sessions}</td></tr>
<tr><th>Rooms</th><td>{rooms}</td></tr>
<tr><th>Mixer ticks skipped (lifetime)</th><td>{ticks_skipped}</td></tr>
<tr><th>IP allow-list active</th><td>{allow_list}</td></tr>
<tr><th>TURN configured</th><td>{turn}</td></tr>
</table>

<h2>Sessions</h2>
{sessions_table}

<h2>Recent warnings / errors</h2>
{log_tail}

<h2>Controls</h2>
<form method="post" action="/reload" onsubmit="return confirm('Reload confluencevoice.toml now? Only the IP allow-list and TURN settings are applied live; everything else needs a restart.');">
<button type="submit">Reload config</button>
</form>
<form method="post" action="/restart" onsubmit="return confirm('Restart ConfluenceVoice now? Everyone connected will need to fully relog to get voice back — confirmed 2026-09-27 that reconnecting on its own, and the usual voice on/off toggle, do NOT recover it. See docs/TESTING.md.');">
<button type="submit">Restart</button>
</form>
<form method="post" action="/stop" onsubmit="return confirm('Stop ConfluenceVoice now? Voice stays down until it is started again by hand — this page cannot start it back up.');">
<button type="submit" class="danger">Stop</button>
</form>
</body></html>"#,
        version = env!("CARGO_PKG_VERSION"),
        uptime_secs = app.started_at.elapsed().as_secs(),
        sessions = app.sessions.session_count(),
        max_sessions = app.max_sessions,
        rooms = app.sessions.rooms().len(),
        ticks_skipped = app.total_ticks_skipped.load(Ordering::Relaxed),
        allow_list = if app.allowed_region_ips.read().is_empty() { "no" } else { "yes" },
        turn = if app.ice_servers.read().is_empty() { "no" } else { "yes" },
        sessions_table = sessions_table_html(app),
        log_tail = log_tail_html(),
    );
    Response::builder()
        .status(StatusCode::OK)
        .header("content-type", "text/html; charset=utf-8")
        .body(Body::from(html))
        .unwrap()
}

// ─────────────────────────── Log ring buffer ───────────────────────────
//
// Captures WARN/ERROR lines in memory so the admin page can show recent trouble
// without opening a log file over RDP. Does not replace normal logging — every
// record still goes to env_logger's own target (stderr, or wherever
// RUST_LOG/redirection points it) exactly as before; this just also keeps a
// short-lived copy of the ones worth surfacing.

const LOG_RING_CAP: usize = 100;

static LOG_RING: parking_lot::Mutex<std::collections::VecDeque<String>> =
    parking_lot::Mutex::new(std::collections::VecDeque::new());

fn push_log_line(line: String) {
    let mut ring = LOG_RING.lock();
    if ring.len() >= LOG_RING_CAP {
        ring.pop_front();
    }
    ring.push_back(line);
}

fn recent_log_lines() -> Vec<String> {
    LOG_RING.lock().iter().cloned().collect()
}

/// Wraps env_logger's own Logger so every record still reaches it unchanged, while
/// WARN/ERROR records are also captured into LOG_RING with a timestamp relative to
/// process start (so it lines up with the "Uptime" figure shown on the same page).
struct RingLogger {
    inner: env_logger::Logger,
    started_at: std::time::Instant,
}

impl log::Log for RingLogger {
    fn enabled(&self, metadata: &log::Metadata) -> bool {
        self.inner.enabled(metadata)
    }

    fn log(&self, record: &log::Record) {
        if self.inner.enabled(record.metadata()) && record.level() <= log::Level::Warn {
            push_log_line(format!(
                "[{:>8.1}s] {:<5} {}",
                self.started_at.elapsed().as_secs_f64(),
                record.level(),
                record.args()
            ));
        }
        self.inner.log(record);
    }

    fn flush(&self) {
        self.inner.flush();
    }
}

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    let started_at = std::time::Instant::now();
    let inner_logger =
        env_logger::Builder::from_env(env_logger::Env::default().default_filter_or("info")).build();
    log::set_max_level(inner_logger.filter());
    log::set_boxed_logger(Box::new(RingLogger { inner: inner_logger, started_at }))
        .expect("logger set exactly once, at the very start of main");

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
        allowed_region_ips: RwLock::new(cfg.allowed_region_ips.clone()),
        ice_servers: RwLock::new(build_ice_servers(
            &cfg.turn_urls,
            cfg.turn_username.clone(),
            cfg.turn_credential.clone(),
        )),
        started_at,
        total_ticks_skipped: std::sync::atomic::AtomicU64::new(0),
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

    if let Some(admin_bind) = cfg.admin_bind.clone() {
        tokio::spawn(admin_server(app.clone(), admin_bind));
    } else {
        log::info!("admin page disabled (admin_bind is empty in the config file)");
    }

    let tls = Arc::new(load_tls(&cfg.tls_cert, &cfg.tls_key)?);
    let acceptor = tokio_rustls::TlsAcceptor::from(tls);
    let addr: SocketAddr = cfg.rpc_bind.parse()?;
    let listener = bind_with_retry(addr, "JSON-RPC port")
        .await
        .map_err(|e| format!("{addr}: {e}"))?;

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
        {
            let allow = app.allowed_region_ips.read();
            if !allow.is_empty() && !allow.contains(&peer.ip()) {
                log::warn!("rejecting connection from {peer}: not in allowed_region_ips");
                continue;
            }
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
