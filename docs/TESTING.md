# Testing and troubleshooting

## What has been verified

Tested 2026-09-27 on Windows 11 with Firestorm 7.2.5, one live OpenSim region
(OpenSim-Confluence build, `os-webrtc-janus` addon), ConfluenceVoice running as a
console app on the same machine.

| Check | Result |
|---|---|
| `cargo build --release`, `cargo test` (31 tests) | pass |
| `two_clients` end-to-end harness (real SDP, Opus, per-listener mixing) | pass |
| Region provisions a viewer through ConfluenceVoice | pass |
| Firestorm peer connection reaches **connected**, `SLData` data channel **open** | pass |
| Own speaking indicator (voice dot) shows | pass, with one participant |
| Two or more real participants: hearing each other, panning, other people's dots | **not yet tested** |
| Windows firewall rules (TCP 9443, UDP 40000-40999) | not needed on the same machine; not tested remotely |
| Running as a Windows Service | not implemented (on hold) |

## Vivox replacement checklist

ConfluenceVoice is meant to replace Vivox, so a release should only claim what is ticked
here. Tick an item only after it has been seen working with real viewers, and note the
date and viewer version beside it.

### Connection
- [x] Region provisions a viewer and the peer connection connects (2026-09-27, Firestorm 7.2.5)
- [x] `SLData` data channel opens (2026-09-27, Firestorm 7.2.5)
- [ ] Reconnects after a viewer relog, and after the region restarts
- [ ] **Reconnects on its own after a ConfluenceVoice restart, with no viewer action** — wolfvoice's own `docs/SERVER.md` claims "viewers re-provision automatically within a few seconds." Every restart tonight (2026-09-27) needed a manual relog or a voice toggle from both testers; nobody waited without touching anything first, so this is genuinely untested, not confirmed broken. Leading theory: specific to a mixed Vivox/WebRTC/ThinkVox grid (see "Mixed grids" below), not a general failure — try this on the next restart before assuming either way.
- [ ] Session is removed when the viewer logs out (check `sessions` on `https://host:9443/`)
- [ ] Voice connects from a viewer **outside** your network (needs `public_ip` and UDP 40000-40999 forwarded)
- [ ] Voice works from a viewer on a network that blocks outbound UDP (expected to fail: no TURN)

### Nearby (spatial) voice — needs two or more real participants
- [x] Own speaking indicator shows (2026-09-27, one participant)
- [ ] Each person hears the other
- [ ] Other people's speaking dots appear above their avatars
- [ ] Panning: a speaker to your left is louder in the left ear, and it follows you turning
- [ ] Distance: full volume within 10 m, fading out, silent beyond 60 m
- [ ] Per-person volume slider and per-person mute affect only that listener
- [ ] Crossing into a neighbouring region keeps voice, without a gap or a duplicate participant
- [x] Teleporting within the region keeps the same voice session (2026-09-27, Firestorm 7.2.5, one participant)
- [ ] Teleporting to another region and back reconnects cleanly
- [ ] Two parcels with different voice settings: parcel channel and estate channel behave as set
- [ ] Voice disabled on a parcel or estate silences it, and re-enabling restores it

### Group and person-to-person voice — implemented, never run live
- [ ] Group voice call: all members hear each other, not positional
- [ ] Person-to-person voice call: both sides connect and hear each other
- [ ] Ordinary text IMs are not turned into voice calls (needs the `ChatSessionRequest` fix in `os-webrtc-janus`, see `docs/OPENSIM.md`)
- [ ] Leaving a call removes the person from everyone else's list

### Moderation
- [ ] Estate or group moderator can mute another person — **not implemented** (`moderator_muted` is always `false`)

### Viewers
- [x] Firestorm 7.2.5 (Windows)
- [ ] Other Firestorm versions, including one older than 7.2.4
- [ ] Second Life viewer
- [ ] Browser viewer using `client/voice_llwebrtc.js`

### Operations
- [ ] Runs unattended for 24 hours with no `mixer overloaded` warnings
- [x] `cargo run --release --example load_test` at the expected number of listeners, CPU noted (2026-09-27, see Capacity below)
- [ ] Survives a Windows reboot and starts on its own — **on hold**: needs the Windows Service wrapper, which is not being worked on for now
- [ ] Certificate renewal procedure documented and tried

## Capacity

Measured 2026-09-27 with `cargo run --release --example load_test`, on a 16-logical-core
Windows machine, against an isolated instance (not the machine's live traffic). Each tier
started from a freshly restarted, empty instance — running tiers back-to-back without a
restart left the previous tier's sessions still connected (the harness does not log its
clients out) and produced misleadingly high numbers, since the next tier's load stacked on
top of the leftover one.

Windows has no `/proc`, so the harness's own CPU measurement doesn't work here; CPU below
is the host process's own CPU-time counter, sampled from just before the run to just after,
so it includes connection setup for all clients, not only the steady-state speaking window
wolfvoice's own published numbers (12%, 57%, 203% of one core at these same three tiers, on
a 12-core Linux VM) were measured over — the two are not a fair apples-to-apples comparison.

| Clients | Speaking | CPU used | Result |
|---|---|---|---|
| 10 | 5 | 31% of one core | clean, no overload |
| 40 | 20 | 86% of one core | clean, no overload |
| 120 | 60 | 211% of one core | **103 of 500 ticks skipped in one 10s window** — real audio breakup |

All connections succeeded at every tier, including 120/60 — the failure is missed mixer
deadlines under load, not connection capacity.

**The tick-overload finding.** At 120 sessions the mixer missed its 20ms deadline while
using only ~13% of this machine's total CPU (2.1 of 16 cores) — not a raw CPU shortage,
something about meeting the real-time deadline itself was the problem. Re-measured after
the buffer-reuse fix (`room.rs`, reusing the per-listener mix buffer across ticks instead
of allocating fresh every 20ms), same instance, same isolated setup, two consecutive
120/60 runs:

| Run | CPU used | Ticks skipped |
|---|---|---|
| pre-fix | 211% of one core | 103 of 500 in the worst 10s window |
| post-fix, run 1 | 215% of one core | 1 |
| post-fix, run 2 | 193% of one core | 1 |

CPU cost is unchanged (all three runs are within normal run-to-run variance of each
other), but skipped ticks dropped from 103 to consistently 1. That fits the theory this
fix was written for: the same amount of work, done with far less allocator churn, avoids
whatever was causing the mixer to miss its deadline — this wasn't a CPU-throughput problem
at all. One tick still gets skipped at this tier even after the fix; the cause of that
one has not been investigated.

**A session count multiplier to plan around, from wolfvoice's own `docs/SERVER.md`:**
Firestorm opens a WebRTC connection to *every* WebRTC-enabled region it can currently
hear, not just the one you're standing in — it probes eight compass directions at twice
the 50 m audio range, so one viewer near a region corner can hold up to **four**
simultaneous sessions. With Sandbox as the only WebRTC region on Casperia this doesn't
bite yet, but `max_sessions` and the media port range need to budget for this multiplier
the moment a second WebRTC region exists nearby — capacity is per-viewer-near-a-border,
not per-viewer.

## Problems found, and what fixed them

### Mic button greyed out in Firestorm 7.2.4+
Viewer log: `ICE server parsing failed: Empty uri (SYNTAX_ERROR)`.

Firestorm 7.2.4+ reads the STUN list from `SimulatorFeatures["stun-servers"]`, as a
comma-separated list of `stun:host:port` URIs. The original `os-webrtc-janus` only
advertised `VoiceStunServers`, which the viewer ignores. The fix is region-side: send
`stun-servers` too. Wolf's fork of `os-webrtc-janus` (commit `d88c35f`) does this with a
hardcoded list; the OpenSim-Confluence build derives it from the `StunServers` setting
and adds the `stun:` prefix.

### Mixed grids: voice dies in Vivox or ThinkVox regions after visiting a WebRTC region
Symptom: after being in a WebRTC region (Sandbox), voice stops working in every Vivox or
ThinkVox region until the viewer is relogged. Seen 2026-09-27 on Firestorm 7.2.5.

Cause, from the viewer log and the Firestorm source (`llvoicevivox.cpp`,
`provisionVoiceAccount` and `giveUp`):
1. At login Firestorm starts its **Vivox** client before it knows which voice system the
   region uses.
2. That client asks the region for a Vivox voice account. A WebRTC region answers
   "voice_server_type is not 'webrtc'" as a failed response.
3. Firestorm treats any failed answer as fatal: the log shows `Unable to provision voice
   account`, then `giveUp : Terminating Voice Service`. The Vivox client stops for the rest
   of the session.
4. It only restarts when voice is switched off and on, or at the next login. Vivox and
   ThinkVox regions (ThinkVox also runs through the Vivox client) then have no client.

**Workaround, no relog (tested 2026-09-27):** Preferences → Sound & Media → Voice, untick
"Enable voice chat", OK, tick it again, OK.

Not yet tested: the reverse direction (arriving in a WebRTC region from a Vivox region and
toggling voice).

**Not the same thing as wolfvoice's own "Firestorm uses Vivox even though the region says
webrtc" troubleshooting entry.** Their doc describes the same `{"voice_server_type":"vivox"}`
request as usually harmless noise from Firestorm's dual-provider startup — look for a second,
`jsep`-carrying request to confirm WebRTC actually worked. That's a narrower claim (is voice
working *on this one region right now*) and doesn't cover what happens afterward. It neither
confirms nor contradicts the session-wide `giveUp` finding above; they're answering different
questions, not disagreeing.

Lasting fix, region side: **done on Casperia's Sandbox region** (not upstreamed to
ConfluenceVoice or `os-webrtc-janus` generally) — the WebRTC module now hands Vivox-type
requests to the Vivox module instead of failing them, so the viewer's Vivox client gets real
credentials and never calls `giveUp`. Confirmed live: the region log shows a real Vivox admin
connection established, not the rejection above. Stock viewers cannot be changed, so this had
to be fixed region-side.

### Voice never connects after teleporting through Vivox regions
Symptom: the region log shows `voice_server_type is not 'webrtc'` for requests of type
`vivox`. Firestorm switches to its Vivox client when it enters a region that
advertises Vivox, and does not switch back when you return to a WebRTC region.

Fix: log out while standing in the WebRTC region, then log in again. You then start in
it and Firestorm picks WebRTC from the start. This only matters while some regions in a
grid still use Vivox.

### Region log spam from Vivox requests
While a Vivox client is still active, the region logs one rejected `vivox` request every
few seconds. Harmless.

### "viewer session 0000… not found" on `VoiceSignalingRequest`
The viewer sent an ICE candidate before it had the session id from the provision
reply. Harmless: the connection completes using ConfluenceVoice's host candidate.

### Self-signed certificate
OpenSim's outbound HTTPS defaults to `NoVerifyCertChain = true` and
`NoVerifyCertHostname = true`, so the region's connector accepts a self-signed
ConfluenceVoice certificate. Use a CA-issued certificate for anything reachable from the
internet.

### Firestorm: "unable to connect to the voice server: www.bhr.vivox.com" — not a bug
Not seen directly this session, but documented in wolfvoice's own `docs/TROUBLESHOOTING.md`
and worth knowing before it causes alarm: opening **Preferences → Sound & Media → Voice →
Audio Device Settings** makes Firestorm call `tuningStart()` on *both* voice modules
unconditionally, regardless of which one the region actually uses. The Vivox one launches a
doomed login to Vivox's own servers and eventually raises this alert. WebRTC voice can be
working perfectly at the same time — it's specific to that one settings panel, not a sign
anything is broken.

### "Only some participants hear each other" — check the parcel voice channel, not the code
Also from wolfvoice's docs, not yet hit directly here. Spatial rooms are keyed on region +
parcel, so two people on different parcels of the same region are in different rooms —
correct behaviour when a parcel has its own voice channel rather than using the estate-wide
one. Whether a parcel does depends on its `PF_USE_ESTATE_VOICE_CHAN` flag (bit 30). If
everyone should be sharing one channel and isn't, check that flag before suspecting a
ConfluenceVoice or room-keying problem.

## Reading the logs

- **ConfluenceVoice:** a working session logs `connection state Connected` and
  `data channel "SLData"`.
- **Region:** with `MessageDetails = true` you see each `ProvisionVoice` request.
- **Firestorm:** `Documents\..\Firestorm_x64\logs\Firestorm.log`; search for `#Voice#`.
  Healthy: `Peer Connection State Change connected` and `Data Channel State: open`.

## Public IP and NAT
`public_ip` in `confluencevoice.toml` is the only address a viewer learns for media. On
the same machine or LAN, use the machine's LAN address. For viewers outside your network,
use the public address and forward UDP 40000-40999 to this machine.
