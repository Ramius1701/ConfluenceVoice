# The wire protocol

Everything here is transcribed from the code that produces or consumes it — Firestorm
7.2.2 (`indra/newview/llvoicewebrtc.cpp`, `llwebrtc.cpp`) and OpenSimulator plus the
os-webrtc-janus addon. Line numbers are from those versions and will drift; the field
names will not.

There are two separate protocols: the **JSON-RPC** the region wraps around a viewer's
capability call, and the **data-channel JSON** the viewer exchanges with the voice
service directly.

## 1. Region → voice service (JSON-RPC 2.0 over HTTPS)

`WebRtcVoiceServiceConnector` POSTs to your `WebRtcVoiceServerURI` with
`Content-Type: application/json-rpc`:

```json
{
  "jsonrpc": "2.0",
  "id": "<uuid>",
  "method": "provision_voice_account_request",
  "params": {
    "request": { "…the viewer's body, verbatim…" },
    "userID":  "<agent uuid>",
    "scene":   "<region uuid>"
  }
}
```

Methods are `provision_voice_account_request` and `voice_signaling_request`.

`userID` is the **authoritative identity**: the region issued the capability to that
agent and tells us who it belongs to, so a client cannot assert who it is. This is
why the service needs no credentials of its own — and why the port must be firewalled
to your region hosts.

### Replying

Reply with **HTTP 200** and a JSON object:

```json
{ "jsonrpc": "2.0", "id": "<echoed>", "result": { … } }
```

or, for an application-level failure,

```json
{ "jsonrpc": "2.0", "id": "<echoed>", "error": "human readable reason" }
```

> **Never return a non-2xx status.** OpenSim's `WebUtil.ServiceOSDRequest` calls
> `EnsureSuccessStatusCode`, which throws; the connector catches it, sets the task
> result, and then *still* dereferences the now-null response. The result is a
> NullReferenceException on a detached task and a viewer that waits forever with no
> error anywhere. A 200 carrying `{"error": …}` is the only way to report failure
> visibly.

The whole response body is parsed and placed under `_Result` by
`WebUtil.CanonicalizeResults`, and the connector then looks for `error` or `result`
inside it.

## 2. Viewer → region capabilities (LLSD XML)

The region module parses these with `OSDParser.DeserializeLLSDXml`, so a client must
send **LLSD XML**, not JSON. Replies are LLSD XML too.

### ProvisionVoiceAccountRequest — connect

```
jsep            : { type: "offer", sdp: <the offer> }
channel_type    : "local"          (spatial)  |  "multiagent"  (group/IM)
parcel_local_id : <integer>        only when the parcel id is valid
channel         : <string>         multiagent only
credentials     : <…>              multiagent only
voice_server_type: "webrtc"
```

The reply **must** contain both `viewer_session` and a `jsep` of type `answer` with a
non-empty `sdp`, or the viewer discards it and gives up:

```json
{ "viewer_session": "<opaque id you choose>",
  "jsep": { "type": "answer", "sdp": "<your answer>" } }
```

`viewer_session` is also what the region module keys its own session registry on — it
rewrites its local id from whatever you return.

> **Your answer SDP must already contain your ICE candidates.** There is no
> server→viewer trickle path: `VoiceSignalingRequest` is viewer→server only, and the
> region discards your reply to it. If you read your local description immediately
> after setting it, you get ice-ufrag and ice-pwd with no `a=candidate:` lines, and
> the viewer has your credentials but nowhere to send. Wait for ICE gathering to
> complete before answering — but stay under the region's 10-second HTTP timeout.
>
> This failure is invisible on loopback, where the server can reach the client's own
> host candidates and the client learns yours peer-reflexively. It only appears
> across NAT.

### ProvisionVoiceAccountRequest — disconnect

```
logout          : true
viewer_session  : <id>
voice_server_type: "webrtc"
```

The viewer ignores the reply, but the region still expects a map.

### VoiceSignalingRequest — trickled ICE

Either a batch of candidates:

```
candidates : [ { candidate: "<attr>", sdpMid: "0", sdpMLineIndex: 0 }, … ]
```

or an end-of-gathering marker — note the **singular** key here, where real candidates
use the plural:

```
candidate : { completed: true }
```

Both carry `viewer_session` and `voice_server_type`. The region always answers
`<llsd><undef /></llsd>` regardless of what you return.

## 3. The data channel

The viewer opens an SCTP data channel labelled **`SLData`** as part of its offer, and
**will not consider voice usable until that channel opens** — it parks in
`VOICE_STATE_WAIT_FOR_DATA_CHANNEL` indefinitely. Accepting it is mandatory. This is
precisely what Janus AudioBridge cannot do.

All messages are JSON text.

### Viewer → server

**Join.** Sent once the channel opens. `p` marks this as the agent's *primary*
region rather than a neighbour; it is omitted entirely when not primary.

```json
{ "j": { "p": true } }
```

**Position.** Sent on change, throttled to about 10 Hz.

```json
{ "sp": {"x":…,"y":…,"z":…},          // avatar position
  "sh": {"x":…,"y":…,"z":…,"w":…},    // avatar orientation
  "lp": {"x":…,"y":…,"z":…},          // listener (ear) position
  "lh": {"x":…,"y":…,"z":…,"w":…} }   // listener orientation
```

All components are integers, **metres × 100** — so centimetres — in **global world
coordinates**. The viewer builds them from `region->getPosGlobalFromRegion(...)`,
which adds the region's south-west corner (`x = handle >> 32`,
`y = handle & 0xFFFFFFFF`). Cross-region distance therefore needs no translation.

Quaternions are `(x, y, z, w)`, also scaled by 100.

**Per-user gain**, where the value is `volume × 220`:

```json
{ "ug": { "<agent uuid>": 220 } }
```

**Per-user mute:**

```json
{ "m": { "<agent uuid>": true } }
```

> Firestorm is internally inconsistent here: `setUserVolume` scales by
> `PEER_GAIN_CONVERSION_FACTOR` (220), but the path that re-asserts stored volumes
> when a participant joins hardcodes `volume * 200`. We divide by 220, so a
> join-path value reads about 9 % low.

### Server → viewer

One object whose **keys are agent UUIDs**. A key that does not parse as a UUID is
skipped by the viewer as "probably a test client".

```json
{ "<agent uuid>": {
    "j": { "p": true },   // this participant has joined
    "p": 96,              // audio power; the viewer reads mLevel = p / 128.0
    "v": true,            // is speaking
    "m": false,           // moderator-muted
    "l": true             // this participant has LEFT
} }
```

`"l": true` is the **only** way a participant is ever removed. Nothing else expires
one, so a server that never sends it leaves departed avatars in the voice panel
forever. The viewer ignores `l` for its own agent id, so it is safe to broadcast.

A participant is only *added* when a join arrives and it is either the primary
connection or a non-spatial channel — which is how cross-region voice avoids
duplicating a participant announced by several neighbouring servers.

## 4. Spatialisation

Positions, gain and mute all travel *to* the server, and the server returns one
already-mixed stream. That is the whole reason a per-listener mix is required: A and
B in the same room need different audio, because each is at a different place.

wolfvoice's distance model follows OpenSim's own spatial-voice channel defaults so
that a browser client using WebAudio locally and this server sound the same:

- reference distance **10 m** — no attenuation closer than this
  (OpenSim's `CHAN_CLAMPING_DISTANCE_DEFAULT`, "distance before attenuation applies")
- maximum distance **60 m** — silent at or beyond
  (`CHAN_MAX_RANGE_DEFAULT`, "distance at which channel is silent")
- linear rolloff, factor 1.0

Rolloff 1.0 rather than Vivox's default of 2.0 because in a linear model a rolloff of
2 reaches silence at `ref + (max-ref)/2` = 35 m, contradicting a documented maximum
range of 60.

Separately, the viewer clamps its **ear** to within 50 m of its avatar so a
long camera cannot be used to eavesdrop. **Re-apply that clamp server-side** — the
comment in the viewer source says as much, and a modified client simply would not
bother.

## 5. Provider selection

The viewer decides which voice system to use from one field in the region's
`SimulatorFeatures`:

```
VoiceServerType = "webrtc"   ->  WebRTC client
VoiceServerType = "vivox"    ->  Vivox client
absent or empty              ->  Vivox client
```

There is **no viewer-side setting** for this — nothing for a user to tick. In a mixed
grid the viewer also checks each neighbouring region's own value before opening a
connection to it, so regions can be migrated one at a time without the viewer
thrashing.
