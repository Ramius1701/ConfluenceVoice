# Viewers and clients

## Firestorm (and other Linden-derived viewers)

**Nothing to do.** Firestorm 7.1.10 and newer support WebRTC voice, and the region
tells the viewer which system to use via `SimulatorFeatures.VoiceServerType`. There is
no viewer-side voice-server setting, nothing to install and nothing to tick.

Users only need voice enabled in Preferences as usual, and the parcel/estate flags set
(see [OPENSIM.md](OPENSIM.md)).

In a mixed grid the viewer checks each neighbouring region's own `VoiceServerType`
before connecting to it, so you can migrate regions one at a time without the viewer
thrashing between systems.

One cosmetic wrinkle: opening **Preferences → Sound & Media → Voice → Audio Device
Settings** makes Firestorm attempt a Vivox login regardless of the active provider,
which fails and shows "unable to connect to the voice server: www.bhr.vivox.com".
That is a Firestorm bug, harmless, and unrelated to WebRTC voice — see
[TROUBLESHOOTING.md](TROUBLESHOOTING.md).

## Browser and custom clients

`client/voice_llwebrtc.js` is a working reference implementation, extracted from the
WolfStorm web viewer. It is plain ES2020 with no build step and no dependencies, and
is written against the same protocol document as the server —
[PROTOCOL.md](PROTOCOL.md).

It is not a drop-in library: it reaches into WolfStorm's own managers for the
capability URLs, the agent's position, and name lookups. Treat it as a specification
you can read, and lift the parts you need. The interesting parts are:

| What | Where |
|---|---|
| Building the LLSD-XML provision body | `_negotiate()` |
| Applying the answer and trickling ICE | `_negotiate()`, `_onLocalCandidate()`, `_flushCandidates()` |
| The join / position / gain / mute messages | `_onDataChannelOpen()`, `_sendPosition()`, `setPeerVolume()` |
| Parsing the roster, including `l` for leave | `_onDataChannelMessage()` |
| Global coordinates from a region handle | `_regionOrigin()` |

### Things that will bite you

**The capability body is LLSD XML, not JSON.** The region module parses it with
`OSDParser.DeserializeLLSDXml`; JSON is rejected with no content.

**Create the data channel before the offer,** so the `m=application` section is in it.

**Positions are global metres × 100.** Add the region's south-west corner, which comes
from the region handle: `x = handle >> 32`, `y = handle & 0xFFFFFFFF`. Region handles
sit comfortably inside `Number.MAX_SAFE_INTEGER`, so plain arithmetic is fine.

**Do not add a local panner.** The stream you receive is already spatialised to
stereo, per listener, by the server. Panning it again is wrong.

**Per-user volume and mute are requests to the server,** not local gain nodes. That is
what makes your mute of another user behave identically to theirs of you.

**Attach the remote stream to a (muted) `<audio>` element as well** as routing it
through WebAudio. In Chromium a remote `MediaStream` that is not attached to a media
element yields silence through `MediaStreamAudioSourceNode` while every CPU-side probe
looks perfectly healthy (crbug.com/121673).

**You need no STUN server** for a client-to-server topology: the server advertises a
routable host candidate and learns your address from the connectivity check. An empty
`iceServers: []` is correct and avoids a pointless dependency.

**Retry name resolution.** The protocol carries agent UUIDs only. Name lookup is
asynchronous, so the first attempt usually returns nothing — retry on later roster
updates or you will display raw UUIDs forever.

## Writing a client from scratch

Read [PROTOCOL.md](PROTOCOL.md), then use `examples/two_clients.rs` as a reference
handshake — it performs the whole flow (offer with data channel, JSON-RPC provision,
answer, ICE, join, position, audio, logout) and asserts the outcome. It talks to the
JSON-RPC endpoint directly rather than through a region, which makes it a convenient
harness while you are getting the SDP exchange right.
