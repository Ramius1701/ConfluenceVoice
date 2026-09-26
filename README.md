# ConfluenceVoice

Spatial WebRTC voice for [OpenSimulator](http://opensimulator.org) — a Windows-native
build so Firestorm and browser-based viewers on a Windows-hosted grid can hear each
other positionally, without Vivox.

ConfluenceVoice is a Windows-packaged fork of
[wolfvoice](https://github.com/intelligentwolf/wolfvoice) by Wolf Software Systems
Ltd — see [NOTICE](NOTICE) for full attribution. The protocol handling, spatial
mixer and session logic are unchanged; what's different is how it's configured and
shipped: a plain `.toml` file next to the `.exe` instead of environment variables
and `/etc` paths, so it runs like any other Windows program — download, edit one
file, double-click.

It is a **voice service backend for
[os-webrtc-janus](https://github.com/wolfsoftwaresystemsltd/os-webrtc-janus)**. That
addon does the OpenSimulator half — capabilities, provider advertisement, session
bookkeeping — and ConfluenceVoice is what you point it at instead of a Janus
gateway.

```
Firestorm 7.1.10+  /  browser viewer
        │  ProvisionVoiceAccountRequest + VoiceSignalingRequest   (LLSD caps)
        ▼
OpenSim region  +  os-webrtc-janus addon   ← REQUIRED
        │          forwards the caps onward as JSON-RPC
        ▼
   ConfluenceVoice  ── answers the SDP, terminates DTLS/SRTP/SCTP,
        │              mixes a SEPARATE stream per listener
        ▼
   viewers   (UDP media)
```

## What you need

**os-webrtc-janus is a hard requirement.** It supplies `WebRtcVoice.dll` and the
region-side capability handlers that everything here depends on. See
[docs/OPENSIM.md](docs/OPENSIM.md) for building it — that side is unaffected by
which backend (wolfvoice or ConfluenceVoice) you point it at.

**For the voice server (this program)**

- Windows, with a **public IP address** and a **DNS name** pointing at it. TLS is
  not optional: the SDP carries the server's DTLS fingerprint, so anyone able to
  rewrite plaintext signalling in flight could become the media endpoint.
- **UDP 40000–40999** open in Windows Firewall (range is configurable — see
  below). This range is also the ceiling on concurrent sessions.
- **TCP 9443** reachable *from your region hosts only*. This endpoint has no
  authentication of its own.

**For viewers** — same as upstream: Firestorm 7.1.10+ works with no configuration;
browser viewers need a client speaking the same contract
([`client/voice_llwebrtc.js`](client/voice_llwebrtc.js) is a working reference).

## Quick start

### 1. Get the exe

Build it yourself (see below), or use a release build if one's been provided to
you. Put `confluencevoice.exe` in its own folder — it will create its config file
and expects its TLS certificate next to itself.

### 2. Configure it

Run it once:

```powershell
.\confluencevoice.exe
```

The first run creates `confluencevoice.toml` beside the exe and exits, telling you
where. Open it and set at least `public_ip`:

```toml
public_ip = "203.0.113.10"   # REQUIRED — see the comment in the generated file
rpc_bind = "0.0.0.0:9443"
media_port_lo = 40000
media_port_hi = 40999
max_sessions = 900
tls_cert = "tls\\fullchain.pem"
tls_key = "tls\\privkey.pem"
```

Place your certificate and key at the `tls_cert` / `tls_key` paths (relative paths
resolve against the folder the config file is in). Run `.\confluencevoice.exe`
again.

### 3. Open the firewall

```powershell
New-NetFirewallRule -DisplayName "ConfluenceVoice RPC" -Direction Inbound -Protocol TCP -LocalPort 9443 -Action Allow
New-NetFirewallRule -DisplayName "ConfluenceVoice Media" -Direction Inbound -Protocol UDP -LocalPort 40000-40999 -Action Allow
```

Scope the RPC rule to your region hosts' IPs if your firewall profile allows it —
that port has no authentication of its own.

### 4. Point os-webrtc-janus at it, and configure the region

Same as upstream from here — see [`contrib/confluencevoice.ini`](contrib/confluencevoice.ini)
and [docs/OPENSIM.md](docs/OPENSIM.md). Both **estate** and **parcel** voice flags
must be enabled or nothing will happen and nothing will log — see
[docs/OPENSIM.md](docs/OPENSIM.md#allow-voice) for details.

**Firestorm 7.2.4+ needs one more thing:** the region must advertise
`SimulatorFeatures["stun-servers"]`, or the viewer refuses to start voice and the mic
stays greyed out. Set `StunServers` in the ini (see the template) and use an
`os-webrtc-janus` build that sends the `stun-servers` key. See
[docs/TESTING.md](docs/TESTING.md).

### 5. Check it

```powershell
curl.exe -k https://voice.example.org:9443/
# {"rooms":0,"service":"confluencevoice","sessions":0}
```

## Building from source

Needs a Rust toolchain and `cmake` (the `opus` crate compiles libopus from source).

```powershell
$env:CMAKE_POLICY_VERSION_MINIMUM = "3.5"
cargo build --release
cargo test              # unit tests, no network required
```

The `CMAKE_POLICY_VERSION_MINIMUM` variable works around a version mismatch
between the `opus` crate's vendored libopus (which declares an old
`cmake_minimum_required`) and CMake 4+, which refuses that outright. This is
CMake's own documented escape hatch, not a fragile hack — see
[the upstream PR that found it](https://github.com/intelligentwolf/wolfvoice/pull/1)
for the full explanation.

The built binary is at `target\release\confluencevoice.exe`. Copy it, together with
your `confluencevoice.toml` and `tls\` folder, wherever you want to run it from.

## Status

Working end to end with one live viewer (Firestorm 7.2.5) through an OpenSim region:
the peer connection connects, the data channel opens, and the speaking indicator
shows. **Not yet tested with two or more real participants** (hearing each other,
spatial panning). Details, problems found and fixes: [docs/TESTING.md](docs/TESTING.md).

## Testing it without a viewer

```powershell
# Correctness: two synthetic viewers, real SDP, real Opus, real mixing.
cargo run --release --example two_clients -- https://127.0.0.1:9443

# Capacity: N clients, M speaking, reports service CPU.
cargo run --release --example load_test -- https://127.0.0.1:9443 40 20
```

Run these on the voice host. Point `two_clients` at a `public_ip` that is a real,
bindable address on the machine (a loopback address won't gather a matching ICE
candidate on the client side — that's an ICE property, not specific to this
service).

## What's carried over from wolfvoice, unmodified

- `src/mixer.rs`, `src/proto.rs`, `src/room.rs`, `src/session.rs` — the spatial
  mixing math, wire protocol and session/room bookkeeping. None of it is
  Unix-specific.
- `docs/PROTOCOL.md`, `docs/CLIENT.md`, `docs/OPENSIM.md`, `client/voice_llwebrtc.js`
  — protocol reference, viewer support notes and the region-side addon, none of
  which depend on which OS runs the voice backend.
- `examples/two_clients.rs`, `examples/load_test.rs` — the end-to-end and capacity
  test harnesses.

## What's different from wolfvoice

- **Configuration**: `confluencevoice.toml` next to the exe, not
  `WOLFVOICE_PUBLIC_IP` and hardcoded `/etc/wolfvoice/tls/...` paths (see
  [`src/config.rs`](src/config.rs)).
- **No installer, no systemd unit**: this is meant to be run directly, the way any
  other Windows program is. A Windows Service wrapper (so it can start
  automatically and run without a console window) is a reasonable next step, not
  yet done.
- Session IDs are prefixed `cv-` instead of `wv-`; the health endpoint reports
  `"service": "confluencevoice"`. Everything on the wire to Firestorm/OpenSim is
  unchanged — those prefixes are purely internal/log labels.

## Known limitations

Same as upstream:

- **No TURN, and no way to add one.** Firestorm hardcodes its STUN servers to
  `stun:stunN.<grid>.secondlife.io`, which do not resolve outside Second Life.
  Media still connects in the common case, because the server advertises a
  routable host candidate and learns the viewer's address from the connectivity
  check — but a user whose network blocks outbound UDP cannot use voice at all.
- **Single instance.** If ConfluenceVoice is down, voice is down everywhere it is
  configured.
- **Group and IM voice** (`channel_type: multiagent`) is implemented and rooms are
  keyed correctly, but has had far less exercise than spatial voice.

## Credits

Built on **[wolfvoice](https://github.com/intelligentwolf/wolfvoice)** by Wolf
Software Systems Ltd for the Wolf Territories Grid, which is itself built on
**[os-webrtc-janus](https://github.com/Misterblue/os-webrtc-janus) by Robert
Adams**, which brought WebRTC voice to OpenSimulator. See [NOTICE](NOTICE) for the
full chain.

The wire protocol is Linden Lab's, as implemented in
[Firestorm](https://www.firestormviewer.org/).

## Licence

Apache License 2.0 — see [LICENSE](LICENSE).
