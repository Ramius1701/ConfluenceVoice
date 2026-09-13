# Configuring OpenSimulator

Three things must be true before a viewer will use wolfvoice. Miss any one and the
symptom is silence with nothing obvious in the logs.

1. Your OpenSim has the **os-webrtc-janus addon** compiled in.
2. The region is **configured** to send voice to wolfvoice.
3. Voice is **allowed on the estate and on the parcel**.

## 1. The addon

wolfvoice does not talk to OpenSim directly. It relies on
[os-webrtc-janus](https://github.com/wolfsoftwaresystemsltd/os-webrtc-janus) for the region-side
half: registering the `ProvisionVoiceAccountRequest`, `VoiceSignalingRequest` and
`ParcelVoiceInfoRequest` capabilities, advertising `VoiceServerType = webrtc` to the
viewer, and forwarding those capability calls onward as JSON-RPC.

The addon ships two voice backends. `WebRtcJanusService` drives a Janus gateway;
`WebRtcVoiceServiceConnector` is a plain JSON-RPC 2.0 client that will post to any
URL. wolfvoice plugs into the second, so you do not need to run a **Janus gateway
server** — but you very much do need this addon.

The reason for choosing the connector rather than Janus is that Janus's AudioBridge
plugin is a conference mixer: one mix per room, and no WebRTC data channel. The parts
of Linden Lab's protocol that ride the data channel (position, per-user gain, mute,
the speaker roster) therefore have nowhere to travel. wolfvoice terminates that
channel and mixes per listener instead.

**First check whether your OpenSim already ships it.** Since 26 Feb 2026 the addon is
integrated into both upstream OpenSimulator (`OpenSim/Addons/os-webrtc-janus/`) and
OpenSim-NGC (`Addons/os-webrtc-janus/`). If either directory exists, **do not clone
anything** — a second copy in `addon-modules/` breaks the build with duplicate project
names. Just build OpenSim as you normally do and skip to region configuration.

On an **older tree** without it, clone the Wolf fork's fixed branch (upstream
`Misterblue/os-webrtc-janus` is archived; its `main` still invents a P2P session id in
`ChatSessionRequest`, which broke Firestorm text IMs grid-wide whenever WebRTC voice
was enabled):

```bash
cd opensim/addon-modules
git clone -b chatsession-p2p-session-id-and-fast-fail https://github.com/wolfsoftwaresystemsltd/os-webrtc-janus.git os-webrtc-janus
cd ..
```

Then build — which build you have is told by the tree itself:

**Classic tree** (`runprebuild.sh` exists at the OpenSim root):

```bash
./runprebuild.sh    # also (re)generates compile.sh — normal for it to be missing before this
./compile.sh        # or: dotnet build -c Release OpenSim.sln
```

The DLLs land directly in `bin/`.

**dotnet-era tree** (no `runprebuild.sh`; `OpenSim.sln` + `Directory.Build.props` at
the root — "the only .sh is the addon's own updateVersion.sh" means you are here).
Prebuild is gone and the solution only builds registered projects, so register the
module once, then build via the solution:

```bash
dotnet sln OpenSim.sln add \
    addon-modules/os-webrtc-janus/WebRtcVoice/WebRtcVoice.csproj \
    addon-modules/os-webrtc-janus/WebRtcVoiceServiceModule/WebRtcVoiceServiceModule.csproj \
    addon-modules/os-webrtc-janus/WebRtcVoiceRegionModule/WebRtcVoiceRegionModule.csproj \
    addon-modules/os-webrtc-janus/Janus/WebRtcJanusService.csproj
dotnet build --configuration Release OpenSim.sln
```

Output goes to `build/Release/<AssemblyName>/`, **not** `bin/` — copy
`WebRtcVoice.dll`, `WebRtcVoiceServiceModule.dll` and `WebRtcVoiceRegionModule.dll`
into the `bin/` your regions run from, then restart the region.

All three DLLs are needed. `WebRtcJanusService.dll` is also built, and is simply
unused when you point the connector at wolfvoice. (A binary-only distribution with no
source tree cannot build the addon — build in a source tree of the exact same OpenSim
version and copy the DLLs in.)

Check what you actually have deployed rather than trusting the build:

```bash
tr -d '\000' < bin/WebRtcVoice.dll | grep -c provision_voice_account_request
# 1 or more means the connector is present
```

## 2. Region configuration

Copy `contrib/wolfvoice.ini` to `<region>/bin/config/wolfvoice.ini` and set your
server URL:

```ini
[WebRtcVoice]
    Enabled = true
    SpatialVoiceService = WebRtcVoice.dll:WebRtcVoiceServiceConnector
    NonSpatialVoiceService = WebRtcVoice.dll:WebRtcVoiceServiceConnector
    WebRtcVoiceServerURI = https://voice.example.org:9443
    MessageDetails = false

[VivoxVoice]
    enabled = false
```

Then restart the region.

**Why `bin/config/` and not `OpenSim.ini`.** OpenSim reads every `.ini` in that
directory *after* `OpenSim.ini` and treats them as overrides
(`OpenSim/Region/Application/ConfigurationLoader.cs` — "Override distro settings with
contents of inidirectory", default `inidirectory = "config"`). So a shared or
templated `OpenSim.ini` can be regenerated without clobbering your voice config. If
you run one OpenSim process per region, it also means the change is scoped to that
region alone.

**Turn Vivox off wherever WebRTC is on.** Both modules register the *same*
capability name, and the last registration wins (`CapsHandlers.AddSimpleHandler`
removes any existing entry before adding). Leaving both enabled is a race, not a
choice.

**One process, many regions?** `[WebRtcVoice] Enabled` is read once per *process*, so
in a multi-region simulator this switches every region in that process.

**`MessageDetails`** logs entire SDP bodies. Invaluable on one region while
diagnosing; wasteful across a fleet.

## 3. Estate and parcel flags

This is the one that catches everyone. The viewer checks **both** before it will even
create a voice session:

```cpp
// llvoicewebrtc.cpp
voiceEnabled = voiceEnabled && regionp->isVoiceEnabled();   // estate/region flag
if (voiceEnabled) {
    if (!parcel->getParcelFlagAllowVoice()) voiceEnabled = false;   // parcel flag
}
if (!voiceEnabled) leaveChannel(true);   // no session, no provision, no log
```

So:

- **Estate**: Region/Estate → Estate → *Allow Voice Chat*
  (OpenSim exposes this as `EstateSettings.AllowVoice`, which becomes the
  `REGION_FLAGS_ALLOW_VOICE` region flag).
- **Parcel**: About Land → Sound → *Allow Voice Chat*
  (`PF_ALLOW_VOICE_CHAT`, bit 29 of the parcel flags).

The failure mode is genuinely silent: the region log will happily show
`setting VoiceServerType=webrtc`, and then no provision request ever arrives, because
the viewer decided locally that voice is not available here. If you see that pattern,
check these flags first.

Auditing a whole grid, if your regions share a content database:

```sql
SELECT COUNT(*) AS parcels,
       SUM((LandFlags & 536870912) <> 0) AS voice_on,
       SUM((LandFlags & 536870912) =  0) AS voice_off
FROM land;
```

Do not simply set the bit for everyone — parcel voice may be off deliberately, and it
is the landowner's setting. Also note that a running region holds land data in memory
and will overwrite a database edit on its next save, so change it in-world or before
a restart.

## 4. Required: the `ChatSessionRequest` capability

**Enabling WebRTC voice on a region can break plain TEXT instant messaging** for every
Firestorm/LL-derived viewer, unless the addon serves the `ChatSessionRequest` capability.
Current `os-webrtc-janus` does serve it. Older builds do not — if yours predates it, you
will hit this, and the symptom does not look like a voice problem at all:

> Unable to start a new chat session with <Name>.
> The session initialization is timed out

Why a *voice* setting breaks *text*:

1. With WebRTC selected, the viewer asks its voice module for a P2P outgoing-call
   interface. WebRTC deliberately has none — `llvoicewebrtc.h`
   `getOutgoingCallInterface() override { return nullptr; }`. Only Vivox implements it.
   LL's own comment in `llimview.cpp` says why: *"webrtc uses the multiagent chat
   mechanism for p2p calls, instead of relying on vivox calling."*
2. Because that returns null, `LLIMSession`'s constructor flags **every** new P2P
   session as "P2P as ad-hoc call" — including an ordinary text IM.
3. So `sendStartSession` routes the text IM through `ChatSessionRequest` and arms a 30s
   timer (`SESSION_INITIALIZATION_TIMEOUT`).
4. Until that session is initialised the viewer does **not** send typed messages. It
   queues them (`fsfloaterim.cpp` -> `mQueuedMsgsForInit`) and flushes only on the
   reply. There is no timeout flush and no retry, so with no reply the user's messages
   are **silently discarded**.

The reply must go out on the agent's **event queue**, not in the HTTP response — the
viewer inspects only the HTTP status of `start p2p voice` and ignores the body. So this
cannot be served by an external process: `EventQueueGetModule` holds per-agent in-memory
queues and exposes no external enqueue endpoint. It has to be in-region code calling
`IEventQueue.ChatterBoxSessionStartReply(...)`.

Note this is **not** gated by `Cap_ChatSessionRequest` in `[ClientStack.LindenCaps]`.
Each module reads its own `Cap_*` setting and self-enables; this one does not consult it,
so leaving that value empty is fine.

### If you are on an older addon build

This is fixed in the
[Wolf fork branch](https://github.com/wolfsoftwaresystemsltd/os-webrtc-janus) and in the
copies integrated into OpenSimulator core and OpenSim-NGC since 26 Feb 2026 — those
handle `start p2p voice` by recomputing the P2P session id (XOR of the two agent ids)
and replying via `IEventQueue.ChatterBoxSessionStartReply`. **Update the addon (or
OpenSim itself) if you can.**

Check first whether you actually can: current upstream uses API that older OpenSim trees
do not have (`OSDMap.TryGetUUID` / `TryGetString`, `UUID.ulonga`/`ulongb`). If your
OpenSim predates those, the current addon will not build against it, and updating the
addon means updating OpenSim too. In that case adding the handler to your existing
`WebRtcVoiceRegionModule.cs` is the legitimate short path — it needs only
`IEventQueue.ChatterBoxSessionStartReply`, which has been present for a long time.

Whichever route, the DLL is loaded into the OpenSim process
at startup via Mono.Addins, so **the region must be restarted** — replacing the file on
disk has no effect on a running region, and not even on a fresh avatar login, because
`OnRegisterCaps` still executes the already-loaded assembly.

Verify with (debug logging on):

```
[REGION WEBRTC VOICE][CHATSESSION]: ChatterBoxSessionStartReply session=<uuid> success=True agent=<uuid>
```

A useful tell that you are looking at the right thing: two viewers that *don't* gate on
session initialisation (e.g. a viewer that sends `ImprovedInstantMessage` straight to the
wire) will IM each other perfectly on an affected grid, while two Firestorms fail. That
asymmetry is the viewer's precondition, not your IM transport.

### Template and rebuild traps

If your tooling rebuilds a region's `bin/` from a template — the ocean-batch pattern in
some grid managers does exactly this, `rm -f bin/*` then re-copy — then patching the live
regions is not enough. **Update the template too**, or the next restart silently reverts
it. Same applies to whatever template new regions are created from.

## Rolling out to many regions

The config file is inert until the region restarts, which lets you separate placement
from activation:

```bash
# 1. place everywhere (no impact)
for d in /path/to/regions/*/bin; do
    install -d "$d/config"
    install -m0644 wolfvoice.ini "$d/config/wolfvoice.ini"
done

# 2. activate on your normal restart cycle, or stagger deliberately
```

Stagger the restarts. A thousand regions re-registering at once will hammer your
Robust and asset services far harder than the voice change itself. If you already
restart regions on a schedule, place the file and let that cycle do the work.

**Validate on one region before touching the fleet.** A malformed ini can stop a
region booting, and discovering that across a thousand regions is a bad afternoon.
After the first restart, confirm:

```bash
grep -E "REGION WEBRTC VOICE\]: enabled|WebRtcVoiceServiceConnector enabled" OpenSim.log
grep -c VivoxVoice OpenSim.log     # should be 0 for this boot
```

**New regions.** Whatever provisions your regions needs to place this file too.
If region directories are created from a template, put `bin/config/wolfvoice.ini` in
the template; if a script builds them, have it copy the file from one canonical
location so voice config is edited in a single place.

## Rollback

Delete `bin/config/wolfvoice.ini` (or set `Enabled = false`), re-enable
`[VivoxVoice]` if you want it back, and restart the region.
