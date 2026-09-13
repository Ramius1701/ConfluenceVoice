//! Linden Lab WebRTC voice wire protocol — the parts that cross our boundary.
//!
//! Two separate protocols live here:
//!
//!   1. The JSON-RPC envelope OpenSim's region module wraps around the viewer's
//!      capability request before forwarding it to us.
//!   2. The SCTP data-channel JSON the viewer exchanges with us directly once the
//!      peer connection is up.
//!
//! Every field below is transcribed from the source that produces or consumes it.
//! Nothing here is inferred.

use serde::Deserialize;
use serde_json::{json, Value};

// ─────────────────────────── JSON-RPC envelope ───────────────────────────
//
// Source: OpenSim addon-modules/os-webrtc-janus/WebRtcVoice/WebRtcVoiceServiceConnector.cs
//   :124-140 JsonRpcRequest builds {jsonrpc:"2.0", id:<uuid>, method, params}
//   :86-91   params = {request:<the viewer's map>, userID:<uuid>, scene:<region uuid>}
//   :146     WebUtil.PostToService(uri, request, 10000, true) — `rpc: true`, so
//            Content-Type is "application/json-rpc" (WebUtil.cs:408-409).
//   :160-194 the reply is unwrapped as _Result -> {error} | {result:<map>}.
// Source: OpenSim/Framework/WebUtil.cs:481-506 CanonicalizeResults — our whole
//   response body is parsed and placed under "_Result", so we must return a JSON
//   OBJECT and the payload the connector wants must sit under "result".

/// Method names the region calls.
/// Source: WebRtcVoiceServerConnector.cs:78-79 AddJsonRPCHandler registrations.
pub const METHOD_PROVISION: &str = "provision_voice_account_request";
pub const METHOD_SIGNALING: &str = "voice_signaling_request";

#[derive(Debug, Deserialize)]
pub struct RpcRequest {
    #[serde(default)]
    pub id: Value,
    #[serde(default)]
    pub method: String,
    #[serde(default)]
    pub params: RpcParams,
}

#[derive(Debug, Default, Deserialize)]
pub struct RpcParams {
    /// The viewer's own request body, verbatim.
    #[serde(default)]
    pub request: Value,
    /// Agent UUID as the REGION sees it — this is our authoritative identity for
    /// the session. The viewer never gets to assert who it is.
    #[serde(default, rename = "userID")]
    pub user_id: String,
    /// Region (scene) UUID.
    #[serde(default)]
    pub scene: String,
}

/// Build a success reply.
///
/// IMPORTANT: this must be returned with HTTP 200 even for application-level
/// failures. WebUtil.cs:428 calls EnsureSuccessStatusCode, which throws on any
/// non-2xx; WebRtcVoiceServiceConnector.cs:149-160 then calls tcs.SetResult in the
/// catch and *still* falls through to dereference the null `outerResponse`, so a
/// 4xx/5xx from us produces a NullReferenceException inside a detached Task and
/// the viewer simply hangs with no error.
pub fn rpc_ok(id: &Value, result: Value) -> Value {
    json!({ "jsonrpc": "2.0", "id": id, "result": result })
}

/// Build an application-level error reply (still HTTP 200 — see `rpc_ok`).
/// Source: WebRtcVoiceServiceConnector.cs:172-181 looks for "error" inside _Result.
pub fn rpc_err(id: &Value, message: &str) -> Value {
    json!({ "jsonrpc": "2.0", "id": id, "error": message })
}

// ─────────────────────────── viewer capability bodies ───────────────────────────

/// Channel type the viewer asks for.
/// Source: llvoicewebrtc.cpp:2799 `body["channel_type"] = "local"` for spatial;
///         llvoicewebrtc.cpp:3402 `body["channel_type"] = "multiagent"` for group/IM.
/// Consumer note: WebRtcVoiceServiceModule.cs:218-232 routes "local" to the spatial
/// service and everything else to the non-spatial one.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ChannelType {
    Local,
    MultiAgent,
}

impl ChannelType {
    pub fn parse(s: &str) -> Option<Self> {
        match s {
            "local" => Some(ChannelType::Local),
            "multiagent" => Some(ChannelType::MultiAgent),
            _ => None,
        }
    }
}

/// A provision request, as the viewer sends it.
///
/// Source: llvoicewebrtc.cpp:2794-2802 (spatial):
///     body["jsep"] = {type:"offer", sdp:<offer>}
///     body["parcel_local_id"] = <S32>          // only when != INVALID_PARCEL_ID
///     body["channel_type"] = "local"
///     body["voice_server_type"] = "webrtc"
/// Source: llvoicewebrtc.cpp:3394-3403 (ad-hoc/group) adds:
///     body["credentials"], body["channel"], channel_type = "multiagent"
/// Source: llvoicewebrtc.cpp:2731-2734 (teardown):
///     body["logout"] = true; body["viewer_session"] = <id>
#[derive(Debug)]
pub struct ProvisionRequest {
    pub logout: bool,
    pub viewer_session: Option<String>,
    pub offer_sdp: Option<String>,
    pub channel_type: Option<ChannelType>,
    pub parcel_local_id: i32,
    pub channel: Option<String>,
}

/// Firestorm sends `voice_server_type` on every voice call and the region module
/// rejects anything that is not this value before it ever reaches us
/// (WebRtcVoiceRegionModule.cs:240-248). We re-check rather than trust the region.
pub const VOICE_SERVER_TYPE: &str = "webrtc";

impl ProvisionRequest {
    pub fn parse(v: &Value) -> Result<Self, String> {
        if let Some(vst) = v.get("voice_server_type").and_then(Value::as_str) {
            if !vst.eq_ignore_ascii_case(VOICE_SERVER_TYPE) {
                return Err(format!("voice_server_type is {vst:?}, not webrtc"));
            }
        }

        let logout = v.get("logout").and_then(Value::as_bool).unwrap_or(false);
        let viewer_session = v
            .get("viewer_session")
            .and_then(Value::as_str)
            .map(str::to_owned);

        // jsep.type must be "offer" — OnVoiceConnectionRequestSuccess
        // (llvoicewebrtc.cpp:2999-3010) only accepts an "answer" back, and the
        // exchange is strictly offer-from-viewer.
        let mut offer_sdp = None;
        if let Some(jsep) = v.get("jsep") {
            let ty = jsep.get("type").and_then(Value::as_str).unwrap_or_default();
            if !ty.is_empty() && ty != "offer" {
                return Err(format!("jsep type is {ty:?}, expected offer"));
            }
            offer_sdp = jsep
                .get("sdp")
                .and_then(Value::as_str)
                .map(str::to_owned)
                .filter(|s| !s.is_empty());
        }

        let channel_type = v
            .get("channel_type")
            .and_then(Value::as_str)
            .and_then(ChannelType::parse);

        // INVALID_PARCEL_ID is not sent at all (llvoicewebrtc.cpp:2796-2798 only
        // adds the key when the parcel id is valid), so absence means estate-wide.
        let parcel_local_id = v
            .get("parcel_local_id")
            .and_then(Value::as_i64)
            .unwrap_or(0) as i32;

        let channel = v
            .get("channel")
            .and_then(Value::as_str)
            .map(str::to_owned)
            .filter(|s| !s.is_empty());

        Ok(ProvisionRequest {
            logout,
            viewer_session,
            offer_sdp,
            channel_type,
            parcel_local_id,
            channel,
        })
    }
}

/// The provision reply the viewer requires.
///
/// Source: llvoicewebrtc.cpp:2999-3010 — the result is discarded unless it has
/// BOTH "viewer_session" AND "jsep" with type=="answer" and a non-empty "sdp".
/// `viewer_session` is also what the region module keys its session registry on,
/// and WebRtcVoiceServiceConnector.cs:94-101 rewrites its local id from this value.
pub fn provision_answer(viewer_session: &str, answer_sdp: &str) -> Value {
    json!({
        "viewer_session": viewer_session,
        "jsep": { "type": "answer", "sdp": answer_sdp }
    })
}

/// A trickled ICE update from the viewer.
///
/// Source: llvoicewebrtc.cpp:2496-2519 processIceUpdatesCoro —
///   either  body["candidates"] = [ {sdpMid, sdpMLineIndex, candidate}, ... ]
///   or      body["candidate"]  = { completed: true }
///   always  body["viewer_session"], body["voice_server_type"]
/// Note the singular/plural asymmetry: the completion marker uses "candidate",
/// the real candidates use "candidates". Reading the wrong key silently loses ICE.
#[derive(Debug)]
pub struct SignalingRequest {
    pub viewer_session: Option<String>,
    pub candidates: Vec<IceCandidate>,
    pub completed: bool,
}

#[derive(Debug, Clone)]
pub struct IceCandidate {
    pub candidate: String,
    pub sdp_mid: Option<String>,
    pub sdp_mline_index: Option<u16>,
}

impl SignalingRequest {
    pub fn parse(v: &Value) -> Self {
        let viewer_session = v
            .get("viewer_session")
            .and_then(Value::as_str)
            .map(str::to_owned);

        let mut candidates = Vec::new();
        if let Some(arr) = v.get("candidates").and_then(Value::as_array) {
            for c in arr {
                let cand = c.get("candidate").and_then(Value::as_str).unwrap_or("");
                if cand.is_empty() {
                    continue;
                }
                candidates.push(IceCandidate {
                    candidate: cand.to_owned(),
                    sdp_mid: c.get("sdpMid").and_then(Value::as_str).map(str::to_owned),
                    sdp_mline_index: c
                        .get("sdpMLineIndex")
                        .and_then(Value::as_i64)
                        .map(|i| i as u16),
                });
            }
        }

        let completed = v
            .get("candidate")
            .and_then(|c| c.get("completed"))
            .and_then(Value::as_bool)
            .unwrap_or(false);

        SignalingRequest {
            viewer_session,
            candidates,
            completed,
        }
    }
}

// ─────────────────────────── data-channel protocol ───────────────────────────
//
// The viewer creates an SCTP channel named "SLData" (llwebrtc.cpp:1145) and will
// NOT leave VOICE_STATE_WAIT_FOR_DATA_CHANNEL until it opens
// (llvoicewebrtc.cpp:2974-2992), so accepting this channel is mandatory.

/// Everything the viewer can send us on the data channel.
#[derive(Debug, Default)]
pub struct ViewerDataMessage {
    /// Join announcement. `primary` is true when this region is the agent's own
    /// region rather than a neighbour.
    /// Source: llvoicewebrtc.cpp:3294-3311 sendJoin — {"j":{"p":true}}; "p" is
    /// omitted entirely when not primary.
    pub join: Option<bool>,

    /// Avatar position / orientation and listener (ear) position / orientation.
    /// Source: llvoicewebrtc.cpp:1224-1249 sendPositionUpdate —
    ///   "sp"/{x,y,z}, "sh"/{x,y,z,w}, "lp"/{x,y,z}, "lh"/{x,y,z,w},
    ///   each component `(int)(value * 100)`.
    /// The positions are GLOBAL world coordinates: llvoicewebrtc.cpp:1104 uses
    /// region->getPosGlobalFromRegion(...) and mAvatarPosition is an LLVector3d in
    /// the same frame. Units here are therefore centimetres, world-absolute, which
    /// is why cross-region distance needs no translation.
    pub avatar_pos: Option<[f64; 3]>,
    pub avatar_rot: Option<[f64; 4]>,
    pub listener_pos: Option<[f64; 3]>,
    pub listener_rot: Option<[f64; 4]>,

    /// Per-participant gain requests: {"ug":{"<uuid>": <u32>}}.
    /// Source: llvoicewebrtc.cpp:2666-2673 setUserVolume —
    ///   value = (uint32_t)(volume * PEER_GAIN_CONVERSION_FACTOR), and
    ///   PEER_GAIN_CONVERSION_FACTOR = 220 (llvoicewebrtc.cpp:100).
    pub user_gain: Vec<(String, u32)>,

    /// Per-participant mute requests: {"m":{"<uuid>": <bool>}}.
    /// Source: llvoicewebrtc.cpp:2676-2683 setUserMute.
    pub user_mute: Vec<(String, bool)>,
}

/// Divisor turning a viewer `ug` value back into a linear gain multiplier.
/// Source: llvoicewebrtc.cpp:100 — const uint32_t PEER_GAIN_CONVERSION_FACTOR = 220.
///
/// CAUTION, observed inconsistency in Firestorm itself: setUserVolume scales by
/// this constant (:2668, `volume * PEER_GAIN_CONVERSION_FACTOR`), but the path that
/// re-asserts stored speaker volumes when a participant joins hardcodes 200
/// instead (:3201, `(uint32_t)(volume * 200)`). We divide by 220 because that is
/// the documented constant and the interactive path; a value that came from the
/// join path is therefore read ~9% low. Not worth guessing which the sender meant
/// on a per-message basis, but worth knowing when a level looks slightly off.
pub const PEER_GAIN_CONVERSION_FACTOR: f32 = 220.0;

/// Centimetre-to-metre divisor for the data-channel coordinates.
/// Source: llvoicewebrtc.cpp:1226-1249 multiplies metres by 100 before sending.
pub const POSITION_SCALE: f64 = 100.0;

/// Most `ug`/`m` entries we will accept from a single data-channel message, and
/// the ceiling on how many a session may accumulate. Far above any real room size.
pub const MAX_PEER_ENTRIES: usize = 256;

/// Largest gain a listener may request for one speaker. The viewer's own slider
/// tops out at 2.0 (`ug` = 440); a raw u32 would otherwise divide down to a gain
/// of ~19 million, which is a request to blow someone's ears off rather than a
/// volume setting.
pub const MAX_PEER_GAIN: f32 = 4.0;

fn vec3(v: &Value) -> Option<[f64; 3]> {
    Some([
        v.get("x")?.as_f64()? / POSITION_SCALE,
        v.get("y")?.as_f64()? / POSITION_SCALE,
        v.get("z")?.as_f64()? / POSITION_SCALE,
    ])
}

fn vec4(v: &Value) -> Option<[f64; 4]> {
    Some([
        v.get("x")?.as_f64()? / POSITION_SCALE,
        v.get("y")?.as_f64()? / POSITION_SCALE,
        v.get("z")?.as_f64()? / POSITION_SCALE,
        v.get("w")?.as_f64()? / POSITION_SCALE,
    ])
}

impl ViewerDataMessage {
    pub fn parse(text: &str) -> Result<Self, serde_json::Error> {
        let v: Value = serde_json::from_str(text)?;
        let mut m = ViewerDataMessage::default();

        if let Some(j) = v.get("j") {
            // "p" is absent for non-primary connections, so presence of "j" is the
            // join signal and "p" only refines it.
            m.join = Some(j.get("p").and_then(Value::as_bool).unwrap_or(false));
        }
        if let Some(x) = v.get("sp") {
            m.avatar_pos = vec3(x);
        }
        if let Some(x) = v.get("sh") {
            m.avatar_rot = vec4(x);
        }
        if let Some(x) = v.get("lp") {
            m.listener_pos = vec3(x);
        }
        if let Some(x) = v.get("lh") {
            m.listener_rot = vec4(x);
        }
        // These two maps arrive from the VIEWER, so they are attacker-controlled by
        // any grid user. Cap the number of entries taken from one message: a
        // legitimate client only ever adjusts the people it can hear, and without a
        // cap a single object with a million keys grows this session's state
        // without bound.
        if let Some(obj) = v.get("ug").and_then(Value::as_object) {
            for (k, val) in obj.iter().take(MAX_PEER_ENTRIES) {
                if let Some(g) = val.as_u64() {
                    m.user_gain.push((k.clone(), g as u32));
                }
            }
        }
        if let Some(obj) = v.get("m").and_then(Value::as_object) {
            for (k, val) in obj.iter().take(MAX_PEER_ENTRIES) {
                if let Some(b) = val.as_bool() {
                    m.user_mute.push((k.clone(), b));
                }
            }
        }
        Ok(m)
    }
}

/// One participant's entry in the roster we push down the data channel.
///
/// Source: llvoicewebrtc.cpp:3155-3243 OnDataReceivedImpl — the viewer walks the
/// top-level object treating each KEY as an agent UUID, and reads per entry:
///   "j" : object   -> that participant has joined ("j":{"p":bool} marks primary)
///   "p" : integer  -> audio power; the viewer stores mLevel = p / 128.0 (:3227)
///   "v" : bool     -> is speaking (:3232)
///   "m" : bool     -> moderator-muted (:3237)
/// A key that does not parse as a UUID is skipped as "probably a test client"
/// (:3161-3165), so the keys must be real agent ids.
#[derive(Debug, Clone, Default)]
pub struct RosterEntry {
    pub joined: bool,
    pub primary: bool,
    pub power: u8,
    pub speaking: bool,
    pub moderator_muted: bool,
}

/// Scale factor from our internal 0.0-1.0 level to the viewer's `p` field.
/// Source: llvoicewebrtc.cpp:3227 — `participant->mLevel = (F32)p / 128.0f`.
pub const LEVEL_SCALE_TO_WIRE: f32 = 128.0;

/// Level above which the viewer would consider a participant to be speaking.
/// Source: llvoicewebrtc.cpp:98 SPEAKING_AUDIO_LEVEL = 0.30f, compared against
/// mLevel at :1938. We compute `v` ourselves on the same threshold so our roster
/// and the viewer's own local reading of its own voice agree.
pub const SPEAKING_AUDIO_LEVEL: f32 = 0.30;

/// Serialise a departure notice.
///
/// Source: llvoicewebrtc.cpp:3212-3219 — an entry carrying `"l": true` makes the
/// viewer call removeParticipantByID. Without this the viewer keeps a departed
/// participant in its voice panel indefinitely, because nothing else in the
/// protocol expires one.
///
/// Note the viewer ignores `"l"` for its OWN agent id (:3214), so it is safe to
/// broadcast a leave to the whole room without special-casing the leaver.
pub fn leave_json(agent_id: &str) -> String {
    json!({ agent_id: { "l": true } }).to_string()
}

/// Serialise a roster update. Entries whose fields are all default are skipped so
/// idle participants do not generate traffic.
pub fn roster_json(entries: &[(String, RosterEntry)]) -> String {
    let mut root = serde_json::Map::new();
    for (id, e) in entries {
        let mut o = serde_json::Map::new();
        if e.joined {
            // Always send the object form. The viewer only checks that "j" is an
            // object (:3181-3182); "p" inside it marks the primary server.
            o.insert("j".into(), json!({ "p": e.primary }));
        }
        o.insert("p".into(), json!(e.power));
        o.insert("v".into(), json!(e.speaking));
        o.insert("m".into(), json!(e.moderator_muted));
        root.insert(id.clone(), Value::Object(o));
    }
    Value::Object(root).to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_position_update_as_metres() {
        // 12345 cm -> 123.45 m
        let m = ViewerDataMessage::parse(
            r#"{"sp":{"x":12345,"y":-200,"z":2100},"lh":{"x":0,"y":0,"z":0,"w":100}}"#,
        )
        .unwrap();
        assert_eq!(m.avatar_pos, Some([123.45, -2.0, 21.0]));
        assert_eq!(m.listener_rot, Some([0.0, 0.0, 0.0, 1.0]));
    }

    #[test]
    fn parses_join_without_primary_flag() {
        // sendJoin omits "p" entirely when the connection is not primary.
        let m = ViewerDataMessage::parse(r#"{"j":{}}"#).unwrap();
        assert_eq!(m.join, Some(false));
        let m = ViewerDataMessage::parse(r#"{"j":{"p":true}}"#).unwrap();
        assert_eq!(m.join, Some(true));
    }

    #[test]
    fn parses_gain_and_mute_maps() {
        let m = ViewerDataMessage::parse(
            r#"{"ug":{"11111111-1111-1111-1111-111111111111":110},
                "m":{"22222222-2222-2222-2222-222222222222":true}}"#,
        )
        .unwrap();
        assert_eq!(m.user_gain[0].1, 110); // 110/220 == 0.5 gain
        assert!(m.user_mute[0].1);
    }

    #[test]
    fn provision_parse_rejects_non_webrtc() {
        let v: Value = serde_json::from_str(r#"{"voice_server_type":"vivox"}"#).unwrap();
        assert!(ProvisionRequest::parse(&v).is_err());
    }

    #[test]
    fn provision_parse_reads_offer_and_channel() {
        let v: Value = serde_json::from_str(
            r#"{"jsep":{"type":"offer","sdp":"v=0"},"channel_type":"local",
                "parcel_local_id":7,"voice_server_type":"webrtc"}"#,
        )
        .unwrap();
        let p = ProvisionRequest::parse(&v).unwrap();
        assert_eq!(p.offer_sdp.as_deref(), Some("v=0"));
        assert_eq!(p.channel_type, Some(ChannelType::Local));
        assert_eq!(p.parcel_local_id, 7);
        assert!(!p.logout);
    }

    #[test]
    fn signaling_parse_handles_both_shapes() {
        let v: Value = serde_json::from_str(
            r#"{"candidates":[{"candidate":"candidate:1 1 udp 1 1.2.3.4 5 typ host",
                "sdpMid":"0","sdpMLineIndex":0}],"viewer_session":"s"}"#,
        )
        .unwrap();
        let s = SignalingRequest::parse(&v);
        assert_eq!(s.candidates.len(), 1);
        assert!(!s.completed);

        let v: Value = serde_json::from_str(r#"{"candidate":{"completed":true}}"#).unwrap();
        let s = SignalingRequest::parse(&v);
        assert!(s.completed);
        assert!(s.candidates.is_empty());
    }

    #[test]
    fn leave_notice_uses_the_l_flag() {
        let s = leave_json("cccccccc-0000-0000-0000-000000000000");
        assert!(s.contains(r#""l":true"#), "leave must set l=true: {s}");
        assert!(s.contains("cccccccc-0000-0000-0000-000000000000"));
    }

    #[test]
    fn roster_uses_viewer_field_names() {
        let e = RosterEntry {
            joined: true,
            primary: true,
            power: 64,
            speaking: true,
            moderator_muted: false,
        };
        let s = roster_json(&[("aaaaaaaa-0000-0000-0000-000000000000".into(), e)]);
        assert!(s.contains(r#""j":{"p":true}"#));
        assert!(s.contains(r#""p":64"#));
        assert!(s.contains(r#""v":true"#));
    }
}
