# ConfluenceVoice

Spatial WebRTC voice for [OpenSimulator](http://opensimulator.org) — a **Windows-native**
voice server so Firestorm and browser-based viewers can hear each other positionally,
without Vivox, on **any OpenSim grid**.

## Who this is for

Vivox has stopped taking new signups for its free OpenSim voice service and has
announced its end ([OSGrid's notice](https://www.osgrid.online/news/vivox-voice/)).
Grids that already have Vivox set up can still get voice from it today, but nobody
outside Vivox knows for how long, so operators need a replacement they control before
it stops. The existing WebRTC voice backend for OpenSim
([wolfvoice](https://github.com/intelligentwolf/wolfvoice)) is built for Linux, with
Docker or WSL as the route for everyone else. ConfluenceVoice gives Windows grid
operators another option: a single `confluencevoice.exe` that runs directly on Windows,
configured by one `.toml` file, with no Linux VM, container or WSL layer to install and
maintain.

It is not tied to any particular grid or OpenSim distribution. It works with any
OpenSimulator that has the `os-webrtc-janus` addon (upstream OpenSim and OpenSim-NGC
include it; older trees can add it). The name comes from the Confluence grid where it
was developed and first tested.

**Free and self-hosted** is the actual difference from the alternatives, not just a
preference. wolfvoice is the same idea but needs Linux, Docker or WSL. ThinkVox is a
hosted service: free for 10 concurrent users, then priced per tier ($19/mo for 50,
$49/mo for 200, $149/mo for 1,000, per their site). Vivox needs no server of your own
either, but it's the thing running out — see above. ConfluenceVoice costs only your own
hardware and bandwidth, with no account, no per-user pricing and no external service in
the voice path, at the cost of running and maintaining it yourself.

**Best on a grid where every region speaks WebRTC.** That's the configuration this
project tests against, and where the checklist in [docs/TESTING.md](docs/TESTING.md) is
being filled in. A grid that mixes WebRTC with Vivox or ThinkVox regions works too, but
hits a real Firestorm quirk — see "Known limitations" below — that a single-voice-system
grid never encounters.

ConfluenceVoice is an independent project, derived from
[wolfvoice](https://github.com/intelligentwolf/wolfvoice) by Wolf Software Systems
Ltd, which is used as a donor of code and ideas — this is not a GitHub fork and
shares no git history with it. See [NOTICE](NOTICE) for full attribution. The
protocol handling, spatial mixer and session logic are carried over unchanged; what's
different is how it's configured and shipped: a plain `.toml` file next to the `.exe`
instead of environment variables and `/etc` paths, so it runs like any other Windows
program — download, edit one file, double-click.

It is a **voice service backend for
[os-webrtc-janus](https://github.com/Misterblue/os-webrtc-janus)** (original by
Robert Adams; Wolf Software Systems maintains a fork at
[intelligentwolf/os-webrtc-janus](https://github.com/intelligentwolf/os-webrtc-janus)). That
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
CMake's own documented escape hatch, not a fragile hack. Without it the build stops
with "Compatibility with CMake < 3.5 has been removed from CMake". Use a recent CMake
(4.x) so it also recognises current Visual Studio versions.

The built binary is at `target\release\confluencevoice.exe`. Copy it, together with
your `confluencevoice.toml` and `tls\` folder, wherever you want to run it from.

## Status

Working end to end with one live viewer (Firestorm 7.2.5) through an OpenSim region:
the peer connection connects, the data channel opens, and the speaking indicator
shows. **Not yet tested with two or more real participants** (hearing each other,
spatial panning). Details, problems found and fixes: [docs/TESTING.md](docs/TESTING.md).

## Roadmap

Next, in order:

1. **Multi-person testing.** Two or more real participants for spatial voice (hearing
   each other, panning, distance, other people's speaking dots), then group voice and
   person-to-person calls. Progress is tracked in the checklist in
   [docs/TESTING.md](docs/TESTING.md); a release should only claim what is ticked there.
2. **A shared secret for the voice port.** TCP 9443 now supports an IP allow-list
   (`allowed_region_ips` in `confluencevoice.toml`, checked before the TLS handshake) —
   done. A shared-secret scheme on top of that is not: the region-side connector
   (`os-webrtc-janus`'s `WebRtcVoiceServiceConnector.cs`) has no way to send one today, so
   this needs a matching region-side change first, not just a ConfluenceVoice one.
3. ~~TURN relay support~~ **Done.** `turn_urls`/`turn_username`/`turn_credential` in
   `confluencevoice.toml` are passed to viewers as a relay of last resort. Verified that a
   configured TURN server doesn't break normal connections; **not yet verified against a
   real TURN server actually relaying media** for a viewer that needs one.

After that:

4. **Easier setup for operators.** Get the `stun-servers` fix into upstream
   `os-webrtc-janus` (Firestorm 7.2.4+ needs it), a Windows guide to getting a free
   TLS certificate, a firewall setup script (PowerShell commands exist in this README;
   not yet a standalone script), automatic public-IP detection, and a Vivox-to-WebRTC
   migration guide for grids that mix both.
5. **Reliability.** Done: a status endpoint (`GET /`) reporting version, uptime, session
   counts, cumulative skipped-tick count, and whether auth/TURN are configured; config
   validation that rejects bad IP entries and incomplete TURN setup at startup with a
   clear message; and a local admin page (`admin_bind` in `confluencevoice.toml`,
   loopback-only by default) with a live status view and Stop/Restart controls. Still
   open: a TLS certificate expiry warning on the status/admin page (so a lapsed cert
   doesn't silently take voice down), log to a file with rotation, certificate reload
   without a restart, and a documented Task Scheduler or NSSM recipe for starting on
   boot.
6. **Distribution and trust.** Done: automated Windows builds and tests on every push
   ([ci.yml](.github/workflows/ci.yml)), and a release workflow
   ([release.yml](.github/workflows/release.yml)) that builds, packages and publishes a
   GitHub Release from a version tag. Still open: code signing so SmartScreen does not
   warn (needs a certificate), and a winget or Scoop package.
7. **Moderator mute.** Estate and group moderators muting another person, a feature
   Vivox had. The protocol field exists; the server currently always reports it as
   `false`.

On hold: a Windows Service wrapper (start automatically, no console window). For
reference when this resumes: wolfvoice's `contrib/wolfvoice.service` restarts on crash
(`Restart=always`, 2s delay), bounds memory to 2G "to bound a runaway rather than size
normal use," and sandboxes the process heavily (`ProtectSystem=strict` and similar) —
worth carrying the same intent into whatever Windows equivalent gets built, not just the
"starts automatically" part.

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
  automatically and run without a console window) is not implemented and is on hold
  for now; run it from a console, or start it yourself with Task Scheduler.
- Session IDs are prefixed `cv-` instead of `wv-`; the health endpoint reports
  `"service": "confluencevoice"`. Everything on the wire to Firestorm/OpenSim is
  unchanged — those prefixes are purely internal/log labels.

## Known limitations

**Mixed grids.** On a grid that runs WebRTC regions next to Vivox or ThinkVox regions,
Firestorm's Vivox client can stop for the rest of the session after visiting a WebRTC
region, so voice fails in the Vivox regions until voice is toggled off and on in
Preferences → Sound & Media → Voice (no relog needed). This is a Firestorm behaviour,
not something ConfluenceVoice can fix on its own — a region-side workaround (letting the
region's Vivox and WebRTC modules answer requests together) has been tried on one grid's
`os-webrtc-janus` build, not upstreamed here yet. A grid running WebRTC only, with no
Vivox or ThinkVox regions, does not hit this. Details in
[docs/TESTING.md](docs/TESTING.md).

- **TURN is supported but unverified against a real relay.** `confluencevoice.toml` can
  point viewers at a TURN server (see Roadmap above) — unlike upstream wolfvoice, which
  has no way to add one. Without one configured, the situation is the same as upstream:
  Firestorm hardcodes its own STUN servers to `stun:stunN.<grid>.secondlife.io`, which
  don't resolve outside Second Life, so media still connects in the common case (the
  server advertises a routable host candidate and learns the viewer's address from the
  connectivity check), but a viewer whose network blocks outbound UDP entirely gets no
  voice.
- **Single instance.** If ConfluenceVoice is down, voice is down everywhere it is
  configured.
- **Group and IM voice** (`channel_type: multiagent`) is implemented and rooms are
  keyed correctly, but has had far less exercise than spatial voice.

## Alternatives

Other ways to get voice on an OpenSim grid now that the built-in Vivox connector no
longer works for most operators. Facts below are as of September 2026; check each
project's own pages before relying on them.

- **[wolfvoice](https://github.com/intelligentwolf/wolfvoice)** — the project
  ConfluenceVoice is derived from. Same WebRTC design, self-hosted and free. Built for
  Linux (a Linux host with systemd); on Windows it means Docker or WSL. Choose it if
  your voice server is Linux.
- **[ThinkVox](https://thinkvox.cloud/)** — a hosted, paid service (a free tier and
  tiers priced by concurrent users) with a web dashboard and an API key; its signup
  works much like Vivox's did. Firestorm 7.1.10+ uses WebRTC with nothing to install.
  For older Firestorm it asks users to swap in a replacement `SLVoice` binary, and the
  download its onboarding offered was for Linux x64. Choose it if you would rather not
  run a voice server yourself.
- **ConfluenceVoice** — self-hosted and free, a single `.exe` on Windows, with no
  per-user pricing and no third-party service in the voice path. Firestorm 7.1.10+
  uses WebRTC natively, so viewers need no replacement files.

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
