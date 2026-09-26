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
| Running as a Windows Service | not implemented |

## Problems found, and what fixed them

### Mic button greyed out in Firestorm 7.2.4+
Viewer log: `ICE server parsing failed: Empty uri (SYNTAX_ERROR)`.

Firestorm 7.2.4+ reads the STUN list from `SimulatorFeatures["stun-servers"]`, as a
comma-separated list of `stun:host:port` URIs. The original `os-webrtc-janus` only
advertised `VoiceStunServers`, which the viewer ignores. The fix is region-side: send
`stun-servers` too. Wolf's fork of `os-webrtc-janus` (commit `d88c35f`) does this with a
hardcoded list; the OpenSim-Confluence build derives it from the `StunServers` setting
and adds the `stun:` prefix.

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
