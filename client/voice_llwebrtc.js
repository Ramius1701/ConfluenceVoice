/**
 * @file voice_llwebrtc.js
 * @brief Linden Lab WebRTC voice provider — the SAME path Firestorm takes.
 *
 * This replaces the old peer-to-peer mesh (voice_webrtc.js). The mesh could never
 * interoperate with Firestorm: Firestorm does not join a mesh, it opens ONE peer
 * connection to a server and expects that server to do the mixing. So we now speak
 * the same contract, through the same region capability, to the same service
 * (wolfvoice). Both viewers therefore land in the same room with the same
 * spatialisation.
 *
 * Consequences of the server doing the work — all deliberate:
 *   - ONE inbound audio stream, already spatialised to stereo. No per-peer
 *     PannerNode, because per-listener panning happened on the server.
 *   - Per-user volume and mute are REQUESTS sent up the data channel, not local
 *     gain nodes, so a Firestorm user's mute of us behaves identically to ours.
 *   - The participant list comes from the server's roster, not from peer records.
 *
 * Wire contract, all read from Firestorm 7.2.2:
 *   provision   llvoicewebrtc.cpp:2794-2802  POST ProvisionVoiceAccountRequest
 *   answer      llvoicewebrtc.cpp:2999-3010  {viewer_session, jsep:{type:"answer",sdp}}
 *   teardown    llvoicewebrtc.cpp:2731-2734  {logout:true, viewer_session}
 *   ICE         llvoicewebrtc.cpp:2496-2519  POST VoiceSignalingRequest
 *   join        llvoicewebrtc.cpp:3294-3311  {"j":{"p":true}}
 *   position    llvoicewebrtc.cpp:1224-1249  sp/sh/lp/lh, ints x100, GLOBAL metres
 *   gain/mute   llvoicewebrtc.cpp:2666-2683  {"ug":{...}} / {"m":{...}}
 *   roster in   llvoicewebrtc.cpp:3155-3243  {"<uuid>":{j,p,v,m}}
 */

class VoiceLLWebRTC {

    /**
     * Data channel label. Firestorm names it "SLData" (llwebrtc.cpp:1145). Ours can
     * be anything since the server keys off the channel's existence, but matching
     * the reference viewer keeps server-side logs comparable.
     */
    static DATA_CHANNEL_LABEL = 'SLData';

    /**
     * Position send cadence. Source: llvoicewebrtc.cpp:105
     * UPDATE_THROTTLE_SECONDS = 0.1f — the viewer coalesces position updates to
     * 10 Hz rather than sending one per frame.
     */
    static POSITION_INTERVAL_MS = 100;

    /** Centimetre scaling on the data channel (llvoicewebrtc.cpp:1226-1249). */
    static POSITION_SCALE = 100;

    /**
     * Source: llvoicewebrtc.cpp:100 PEER_GAIN_CONVERSION_FACTOR = 220.
     * A user volume of 1.0 is sent as the integer 220.
     */
    static PEER_GAIN_CONVERSION_FACTOR = 220;

    /** Source: llvoicewebrtc.cpp:3227 — the viewer reads mLevel = p / 128.0. */
    static LEVEL_SCALE = 128;

    /** Source: llvoicewebrtc.cpp:98 SPEAKING_AUDIO_LEVEL = 0.30f. */
    static SPEAKING_LEVEL = 0.30;

    /** How long to batch trickled ICE candidates before POSTing them. */
    static ICE_BATCH_MS = 200;

    constructor(manager) {
        this.mgr = manager;
        this.pc = null;
        this.dc = null;
        this.localStream = null;
        this.viewerSession = null;
        this.connected = false;
        this._closing = false;
        this._roomKey = null;

        /** agentId -> {agentId, name, speaking, muted, level} from the server roster. */
        this.roster = new Map();

        this._pendingCandidates = [];
        this._iceTimer = null;
        this._posTimer = null;
        this._lastPosJson = null;

        /** Kept alive deliberately — see _attachRemoteAudio. */
        this._remoteAudioEl = null;
        this._remoteSource = null;
    }

    isConnected() { return this.connected; }

    // ─────────────────────────── capabilities ───────────────────────────

    _cap(name) {
        return window.gWolfstorm?.capsManager?.getCap?.(name) || null;
    }

    /**
     * POST an LLSD-XML body to a capability and return the parsed LLSD reply.
     *
     * The region module parses the request with OSDParser.DeserializeLLSDXml
     * (WebRtcVoiceRegionModule.cs:217), so this MUST be LLSD XML — JSON is rejected
     * with NoContent. The reply is LLSD XML too (:258).
     */
    async _capPost(capName, llsdXml) {
        const url = this._cap(capName);
        if (!url) {
            console.warn(`[Voice] capability ${capName} not granted by this region`);
            return null;
        }
        const result = await window.gWolfstorm.networkManager.sendProxyRequest({
            url,
            method: 'POST',
            raw: llsdXml,
            headers: { 'Content-Type': 'application/llsd+xml' }
        });
        if (!(result && result.success)) {
            console.warn(`[Voice] ${capName} POST failed:`,
                result?.message || `http ${result?.http_code}` || 'unknown error');
            return null;
        }
        let data = result.response;
        if (typeof data === 'string' && (data.trim().startsWith('<') || data.includes('<?xml'))) {
            data = window.LLSDParser?.parseXML?.(data);
        }
        return data;
    }

    /** Escape text for inclusion in an XML element. SDP contains no XML-special
     *  characters in practice, but a fingerprint or ufrag is attacker-influenced
     *  enough that escaping is not optional. */
    static _xml(s) {
        return String(s)
            .replace(/&/g, '&amp;')
            .replace(/</g, '&lt;')
            .replace(/>/g, '&gt;')
            .replace(/"/g, '&quot;')
            .replace(/'/g, '&apos;');
    }

    // ─────────────────────────── lifecycle ───────────────────────────

    /**
     * Acquire the microphone, negotiate with the region's voice service and join.
     *
     * `token` is accepted for interface compatibility with the old mesh provider
     * and deliberately unused: identity now comes from the capability itself. The
     * region issued that cap to this agent and tells the voice service which agent
     * it belongs to (WebRtcVoiceServiceConnector.cs:86-91 userID), so the client
     * cannot assert who it is. That is strictly better than the HMAC token, which
     * proved WHO but not WHERE.
     */
    async start(roomKey, _token) {
        this._closing = false;
        this._roomKey = roomKey;

        // THROW, do not return. voice_manager._rejoin() retries switchRoom() with
        // backoff precisely because caps and parcel data arrive over the seconds
        // after a region crossing — but only if the promise REJECTS. Returning
        // normally reported success, so the retry never ran and voice stayed down
        // until the user toggled it by hand. No toast here either: the manager
        // reports once after it has exhausted its attempts, rather than per attempt.
        if (!this._cap('ProvisionVoiceAccountRequest')) {
            throw new Error('region has not granted ProvisionVoiceAccountRequest yet');
        }

        if (!this.localStream) {
            this.localStream = await navigator.mediaDevices.getUserMedia({
                audio: { echoCancellation: true, noiseSuppression: true, autoGainControl: true },
                video: false
            });
            this.mgr._attachLocalAnalyser(this.localStream);
            this.setLocalTrackEnabled(!this.mgr.micMuted);
        }

        await this._negotiate();
    }

    async _negotiate() {
        // No ICE servers. The viewer side needs none: wolfvoice answers from a
        // directly-attached public address, so its host candidate is routable and
        // our own peer-reflexive address is learned from the connectivity check.
        // (Firestorm cannot use STUN here either — llvoicewebrtc.cpp:2883 hardcodes
        // stun%d.<grid>.secondlife.io, which does not resolve off Second Life.)
        this.pc = new RTCPeerConnection({ iceServers: [] });

        // Create the data channel BEFORE the offer so the m=application section is
        // in it. The server must accept this channel; without it the session never
        // reaches the equivalent of VOICE_STATE_SESSION_UP.
        this.dc = this.pc.createDataChannel(VoiceLLWebRTC.DATA_CHANNEL_LABEL, { ordered: true });
        this.dc.onopen = () => this._onDataChannelOpen();
        this.dc.onmessage = (e) => this._onDataChannelMessage(e);
        this.dc.onclose = () => console.log('[Voice] data channel closed');

        for (const track of this.localStream.getAudioTracks()) {
            this.pc.addTrack(track, this.localStream);
        }

        this.pc.ontrack = (e) => this._attachRemoteAudio(e.streams[0] || new MediaStream([e.track]));
        this.pc.onicecandidate = (e) => this._onLocalCandidate(e.candidate);
        this.pc.onconnectionstatechange = () => this._onConnectionState();

        const offer = await this.pc.createOffer({ offerToReceiveAudio: true });
        await this.pc.setLocalDescription(offer);

        const parcel = window.gWolfstorm?.parcelManager?.getCurrentParcel?.();
        const parcelId = Number.isFinite(parcel?.localId) ? parcel.localId : null;

        // Body shape: llvoicewebrtc.cpp:2794-2802. parcel_local_id is omitted when
        // there is no valid parcel, exactly as the viewer omits it for
        // INVALID_PARCEL_ID (:2796-2798).
        const body =
            '<?xml version="1.0"?><llsd><map>'
            + '<key>jsep</key><map>'
            + '<key>type</key><string>offer</string>'
            + `<key>sdp</key><string>${VoiceLLWebRTC._xml(offer.sdp)}</string>`
            + '</map>'
            + '<key>channel_type</key><string>local</string>'
            + '<key>voice_server_type</key><string>webrtc</string>'
            + (parcelId !== null ? `<key>parcel_local_id</key><integer>${parcelId}</integer>` : '')
            + '</map></llsd>';

        const reply = await this._capPost('ProvisionVoiceAccountRequest', body);
        // These all THROW so a rejoin is retried — see the note in start().
        if (!reply || typeof reply !== 'object') {
            await this._teardown(true);
            throw new Error('voice server did not answer the provision request');
        }
        if (reply.error) {
            await this._teardown(true);
            throw new Error(`voice server refused the connection: ${reply.error}`);
        }

        const jsep = reply.jsep;
        const sdp = jsep && jsep.sdp;
        if (!reply.viewer_session || !sdp) {
            await this._teardown(true);
            throw new Error('voice server sent an incomplete answer (no viewer_session or sdp)');
        }

        this.viewerSession = String(reply.viewer_session);
        await this.pc.setRemoteDescription({ type: 'answer', sdp: String(sdp) });
        console.log(`[Voice] session ${this.viewerSession} negotiated`);

        // Anything gathered before we learned the session id could not be sent yet.
        this._flushCandidates();
    }

    /**
     * Route the single mixed stream to the master gain.
     *
     * The stream is ALSO attached to a muted <audio> element. That is not
     * redundant: in Chromium a remote MediaStream that is not attached to a media
     * element yields silence through MediaStreamAudioSourceNode while every CPU-side
     * probe looks correct (crbug.com/121673).
     */
    _attachRemoteAudio(stream) {
        if (!stream) return;

        if (!this._remoteAudioEl) {
            const el = document.createElement('audio');
            el.autoplay = true;
            el.muted = true;              // the WebAudio graph does the audible playback
            el.playsInline = true;
            el.style.display = 'none';
            document.body.appendChild(el);
            this._remoteAudioEl = el;
        }
        this._remoteAudioEl.srcObject = stream;
        this._remoteAudioEl.play?.().catch(() => { /* muted element; autoplay is allowed */ });

        const ac = this.mgr.audioContext;
        if (!ac || !this.mgr.masterGain) return;
        try {
            this._remoteSource?.disconnect();
        } catch (_) { /* first attach */ }
        // No PannerNode: the server already produced a per-listener stereo mix.
        this._remoteSource = ac.createMediaStreamSource(stream);
        this._remoteSource.connect(this.mgr.masterGain);
        console.log('[Voice] mixed stream attached');
    }

    _onConnectionState() {
        const st = this.pc?.connectionState;
        console.log(`[Voice] connection state ${st}`);
        if (st === 'connected') {
            this.connected = true;
            this.mgr._onProviderState?.();
        } else if (st === 'failed') {
            this.connected = false;
            // Worth saying out loud: with no TURN available this is the expected
            // outcome on a network that blocks UDP, and the user can act on it.
            this.mgr._reportError('Voice could not connect — your network may be blocking UDP.');
            this.mgr._onProviderState?.();
        } else if (st === 'disconnected' || st === 'closed') {
            this.connected = false;
            this.mgr._onProviderState?.();
        }
    }

    // ─────────────────────────── ICE trickle ───────────────────────────

    _onLocalCandidate(candidate) {
        if (this._closing) return;
        if (!candidate) {
            // End of gathering. Source: llvoicewebrtc.cpp:2510-2515 sends
            // {"candidate":{"completed":true}} — note the SINGULAR key here, where
            // real candidates use the plural "candidates".
            this._postSignaling(
                '<key>candidate</key><map><key>completed</key><boolean>1</boolean></map>'
            );
            return;
        }
        this._pendingCandidates.push(candidate);
        if (this._iceTimer) return;
        this._iceTimer = setTimeout(() => {
            this._iceTimer = null;
            this._flushCandidates();
        }, VoiceLLWebRTC.ICE_BATCH_MS);
    }

    _flushCandidates() {
        if (!this.viewerSession || this._pendingCandidates.length === 0) return;
        const batch = this._pendingCandidates.splice(0);
        // Source: llvoicewebrtc.cpp:2498-2507 — an array of maps carrying exactly
        // sdpMid, sdpMLineIndex and candidate.
        let xml = '<key>candidates</key><array>';
        for (const c of batch) {
            xml += '<map>'
                + `<key>candidate</key><string>${VoiceLLWebRTC._xml(c.candidate)}</string>`
                + `<key>sdpMid</key><string>${VoiceLLWebRTC._xml(c.sdpMid ?? '')}</string>`
                + `<key>sdpMLineIndex</key><integer>${Number(c.sdpMLineIndex ?? 0)}</integer>`
                + '</map>';
        }
        xml += '</array>';
        this._postSignaling(xml);
    }

    _postSignaling(innerXml) {
        if (!this.viewerSession) return;
        const body = '<?xml version="1.0"?><llsd><map>'
            + innerXml
            + `<key>viewer_session</key><string>${VoiceLLWebRTC._xml(this.viewerSession)}</string>`
            + '<key>voice_server_type</key><string>webrtc</string>'
            + '</map></llsd>';
        // Fire and forget: the region replies <undef/> regardless
        // (WebRtcVoiceRegionModule.cs:320), so there is nothing to read.
        this._capPost('VoiceSignalingRequest', body).catch((e) =>
            console.warn('[Voice] ICE signalling failed:', e));
    }

    // ─────────────────────────── data channel ───────────────────────────

    _onDataChannelOpen() {
        console.log('[Voice] data channel open');
        // Announce ourselves. Source: llvoicewebrtc.cpp:3302-3310 — {"j":{"p":true}}
        // where "p" marks this as the agent's PRIMARY region rather than a
        // neighbour. We only ever connect to our own region, so we are primary.
        this._sendData({ j: { p: true } });
        this._startPositionUpdates();
        // Push the user's existing per-speaker choices now that there is a channel.
        this._resendLocalPreferences();
    }

    _sendData(obj) {
        if (this.dc?.readyState !== 'open') return false;
        try {
            this.dc.send(JSON.stringify(obj));
            return true;
        } catch (e) {
            console.warn('[Voice] data channel send failed:', e);
            return false;
        }
    }

    _onDataChannelMessage(e) {
        if (typeof e.data !== 'string') return;
        let obj;
        try {
            obj = JSON.parse(e.data);
        } catch (_) {
            return;
        }
        if (!obj || typeof obj !== 'object') return;

        let changed = false;
        // Source: llvoicewebrtc.cpp:3155-3243 — every top-level key is an agent
        // UUID; a key that is not a UUID is skipped as a test client (:3161-3165).
        for (const [agentId, entry] of Object.entries(obj)) {
            if (!VoiceLLWebRTC._isUuid(agentId) || !entry || typeof entry !== 'object') continue;

            let p = this.roster.get(agentId);
            if (!p) {
                p = { agentId, name: null, speaking: false, muted: false, level: 0 };
                this.roster.set(agentId, p);
                changed = true;
            }
            if (Object.prototype.hasOwnProperty.call(entry, 'p')
                && typeof entry.p === 'number') {
                p.level = entry.p / VoiceLLWebRTC.LEVEL_SCALE;
            }
            if (typeof entry.v === 'boolean') {
                if (p.speaking !== entry.v) changed = true;
                p.speaking = entry.v;
            } else if (Object.prototype.hasOwnProperty.call(entry, 'p')) {
                // No explicit speaking flag: derive it on the viewer's own threshold
                // (llvoicewebrtc.cpp:1938 compares mLevel > SPEAKING_AUDIO_LEVEL).
                const sp = p.level > VoiceLLWebRTC.SPEAKING_LEVEL;
                if (p.speaking !== sp) changed = true;
                p.speaking = sp;
            }
            if (typeof entry.m === 'boolean') {
                if (p.muted !== entry.m) changed = true;
                p.muted = entry.m;
            }
            // Retry every update until a name resolves — the first attempt usually
            // fires the UUIDNameRequest and returns null, and the answer arrives a
            // moment later. Without the retry the panel keeps showing the UUID.
            if (!p.name) {
                const resolved = VoiceLLWebRTC._displayName(agentId);
                if (resolved) {
                    p.name = resolved;
                    changed = true;
                }
            }
        }

        if (changed) this.mgr._onProviderState?.();
    }

    static _isUuid(s) {
        return /^[0-9a-f]{8}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{12}$/i.test(s)
            && s !== '00000000-0000-0000-0000-000000000000';
    }

    /**
     * Resolve an agent id to a display name, requesting it if we do not have it.
     *
     * The name cache is a plain Map<uuid, {firstName, lastName}> living on the
     * network manager (network_manager.js:1806, filled at :3226), and the request
     * verb is sendUUIDNameRequest (:1800) — which dedupes against the cache and its
     * own pending set, so calling it repeatedly is safe.
     *
     * The previous version of this method guessed at `nameCache.getName()` and an
     * `avatarManager.getAvatarName()`, neither of which exists. Every call was
     * optional-chained, so it silently returned null and the voice panel showed raw
     * UUIDs — exactly the failure mode where `?.()` hides a missing method.
     */
    static _displayName(agentId) {
        const nm = window.gWolfstorm?.networkManager;
        if (!nm) return null;

        const cached = nm.nameCache?.get(agentId);
        if (cached) {
            return cached.lastName
                ? `${cached.firstName} ${cached.lastName}`
                : cached.firstName;
        }

        // Not known yet. Ask, and let a later roster tick pick the name up. Also
        // try the avatar in the scene, which may already carry a resolved name.
        const av = window.gWolfstorm?.avatarManager?.getAvatarByUUID?.(agentId);
        if (av?.mDisplayName) return av.mDisplayName;

        nm.sendUUIDNameRequest?.([agentId]);
        return null;
    }

    // ─────────────────────────── position ───────────────────────────

    /**
     * South-west corner of the current region in world metres.
     *
     * Source: llmessage/llregionhandle.h:129-132 from_region_handle —
     *   x = (U32)(handle >> 32), y = (U32)(handle & 0xFFFFFFFF), z = 0.
     * llviewerregion.cpp:680 assigns exactly this to mOriginGlobal, and :2300
     * getPosGlobalFromRegion adds it to the region-local position. Region handles
     * are around 1.1e15, comfortably inside Number.MAX_SAFE_INTEGER, so plain
     * arithmetic is safe; a BigInt is accepted too rather than assumed away.
     */
    _regionOrigin() {
        const nm = window.gWolfstorm?.networkManager;
        let handle = nm?.sessionInfo?.regionHandle
            ?? window.gWolfstorm?.loginInfo?.regionHandle
            ?? window.loginData?.regionHandle;
        if (handle === undefined || handle === null) return null;

        if (typeof handle === 'bigint') {
            return { x: Number(handle >> 32n), y: Number(handle & 0xFFFFFFFFn) };
        }
        handle = Number(handle);
        if (!Number.isFinite(handle)) return null;
        return {
            x: Math.floor(handle / 4294967296),
            y: handle % 4294967296
        };
    }

    _startPositionUpdates() {
        if (this._posTimer) return;
        this._posTimer = setInterval(() => this._sendPosition(),
            VoiceLLWebRTC.POSITION_INTERVAL_MS);
        this._sendPosition();
    }

    /**
     * Send avatar and ear position/orientation.
     *
     * Everything is GLOBAL metres scaled by 100 into integers, matching
     * llvoicewebrtc.cpp:1224-1249. The server re-applies the 50 m ear tether
     * itself — llvoicewebrtc.cpp:1209-1212 is explicit that the clamp must be
     * server-enforced — so there is no point trying to cheat it here.
     */
    _sendPosition() {
        if (this.dc?.readyState !== 'open') return;

        const origin = this._regionOrigin();
        if (!origin) return;

        // avatar_manager.js:27 declares `selfAvatar`; :2033 shows getPosition() is
        // how its position is read. There is no getSelfAvatar() accessor.
        const av = window.gWolfstorm?.avatarManager?.selfAvatar;
        const ap = av?.getPosition?.() || av?.position;
        if (!ap) return;

        const cam = window.gAgentCamera?.getCameraPositionGlobal?.()
            || window.gWolfstorm?.renderManager?.camera?.position;

        const S = VoiceLLWebRTC.POSITION_SCALE;
        const g = (localPos) => ({
            x: Math.trunc((origin.x + localPos.x) * S),
            y: Math.trunc((origin.y + localPos.y) * S),
            z: Math.trunc(localPos.z * S)
        });

        const avatarRot = this._selfRotation(av);
        const camRot = this._cameraRotation();

        const payload = {
            sp: g(ap),
            sh: avatarRot,
            lp: cam ? g(cam) : g(ap),
            lh: camRot || avatarRot
        };

        // Only send on change — the viewer marks its own state dirty rather than
        // resending unconditionally (llvoicewebrtc.cpp:1202-1206, :1221).
        const json = JSON.stringify(payload);
        if (json === this._lastPosJson) return;
        this._lastPosJson = json;
        this._sendData(payload);
    }

    _selfRotation(av) {
        const q = av?.getRotation?.() || av?.quaternion || av?.rotation;
        const S = VoiceLLWebRTC.POSITION_SCALE;
        if (!q || typeof q.w !== 'number') return { x: 0, y: 0, z: 0, w: S };
        return {
            x: Math.trunc(q.x * S), y: Math.trunc(q.y * S),
            z: Math.trunc(q.z * S), w: Math.trunc(q.w * S)
        };
    }

    _cameraRotation() {
        const cam = window.gWolfstorm?.renderManager?.camera;
        const q = cam?.quaternion;
        const S = VoiceLLWebRTC.POSITION_SCALE;
        if (!q || typeof q.w !== 'number') return null;
        return {
            x: Math.trunc(q.x * S), y: Math.trunc(q.y * S),
            z: Math.trunc(q.z * S), w: Math.trunc(q.w * S)
        };
    }

    // ─────────────────────────── per-user volume / mute ───────────────────────────

    /**
     * The manager's one hook for per-speaker level. `gain` is already a linear
     * multiplier where 0 means muted (voice_manager.js:643-651 folds mute into the
     * volume, matching Vivox's own behaviour where speaker mute is DERIVED from the
     * level rather than being independent).
     *
     * Under the mesh provider this set a local GainNode. It is now a request to the
     * server, because the server owns the mix — which is also what makes our mute
     * of a Firestorm user behave exactly like theirs of us.
     *
     * Both messages are sent, not just one: `ug` carries the level
     * (llvoicewebrtc.cpp:2666-2673, value = volume * 220) and `m` carries the mute
     * flag (:2676-2683). Sending both leaves no ambiguity in server state when a
     * user drags the slider to zero and back.
     */
    setPeerVolume(agentId, gain) {
        if (!VoiceLLWebRTC._isUuid(agentId)) return;
        const g = Math.max(0, Number(gain) || 0);
        const raw = Math.trunc(g * VoiceLLWebRTC.PEER_GAIN_CONVERSION_FACTOR);
        this._sendData({ ug: { [agentId]: raw }, m: { [agentId]: g <= 0 } });
    }

    /** Re-assert the user's saved choices once a channel exists to send them on. */
    _resendLocalPreferences() {
        for (const agentId of this.roster.keys()) {
            const gain = this.mgr.getUserVolume?.(agentId);
            // getUserVolume returns 1.0 for "normal" (voice_manager.js:653-659), so
            // only a deliberate change is worth a message.
            if (typeof gain === 'number' && gain !== 1) this.setPeerVolume(agentId, gain);
        }
    }

    // ─────────────────────────── manager interface ───────────────────────────

    /** Participants, excluding ourselves — VoiceManager.getParticipants prepends self. */
    getPeers() {
        const self = window.gAgentID;
        const out = [];
        for (const p of this.roster.values()) {
            if (p.agentId === self) continue;
            out.push({
                agentId: p.agentId,
                name: p.name || p.agentId,
                speaking: p.speaking,
                muted: p.muted,
                isSelf: false
            });
        }
        return out;
    }

    setLocalTrackEnabled(enabled) {
        for (const t of this.localStream?.getAudioTracks() || []) {
            t.enabled = !!enabled;
        }
    }

    async replaceLocalTrack(newStream) {
        this.localStream = newStream;
        const track = newStream?.getAudioTracks?.()[0];
        if (!track || !this.pc) return;
        for (const sender of this.pc.getSenders()) {
            if (sender.track?.kind === 'audio') {
                await sender.replaceTrack(track);
            }
        }
        this.setLocalTrackEnabled(!this.mgr.micMuted);
    }

    /**
     * The parcel changed. The region's capability already scopes the room, and the
     * server keys spatial rooms on region + parcel, so a parcel change needs a
     * fresh provision to land in the right room.
     */
    async switchRoom(roomKey, token) {
        // Only skip when we are ALREADY CONNECTED to this room. The old guard
        // compared the key alone, and switchRoom committed _roomKey before
        // connecting — so once a rejoin failed, every retry for that same room
        // returned here immediately and voice could never recover from a teleport.
        if (roomKey === this._roomKey && this.isConnected()) return;
        console.log(`[Voice] switching room ${this._roomKey} -> ${roomKey}`);
        await this._teardown(/* keepMic */ true);
        // start() sets _roomKey itself, and throws if it cannot connect, which is
        // what lets voice_manager retry.
        await this.start(roomKey, token);
    }

    stop() {
        this._teardown().catch((e) => console.warn('[Voice] teardown:', e));
    }

    async _teardown(keepMic = false) {
        this._closing = true;
        this.connected = false;

        if (this._posTimer) { clearInterval(this._posTimer); this._posTimer = null; }
        if (this._iceTimer) { clearTimeout(this._iceTimer); this._iceTimer = null; }
        this._pendingCandidates.length = 0;
        this._lastPosJson = null;

        // Tell the server first, while the session still exists.
        // Source: llvoicewebrtc.cpp:2731-2734 breakVoiceConnectionCoro.
        if (this.viewerSession) {
            const body = '<?xml version="1.0"?><llsd><map>'
                + '<key>logout</key><boolean>1</boolean>'
                + `<key>viewer_session</key><string>${VoiceLLWebRTC._xml(this.viewerSession)}</string>`
                + '<key>voice_server_type</key><string>webrtc</string>'
                + '</map></llsd>';
            try {
                await this._capPost('ProvisionVoiceAccountRequest', body);
            } catch (e) {
                console.warn('[Voice] logout POST failed:', e);
            }
            this.viewerSession = null;
        }

        try { this.dc?.close(); } catch (_) { /* already gone */ }
        this.dc = null;

        try { this._remoteSource?.disconnect(); } catch (_) { /* not connected */ }
        this._remoteSource = null;
        if (this._remoteAudioEl) {
            this._remoteAudioEl.srcObject = null;
            this._remoteAudioEl.remove();
            this._remoteAudioEl = null;
        }

        try { this.pc?.close(); } catch (_) { /* already closed */ }
        this.pc = null;

        if (!keepMic) {
            for (const t of this.localStream?.getTracks() || []) t.stop();
            this.localStream = null;
        }

        this.roster.clear();
        this.mgr._onProviderState?.();
    }
}

// Classic script, same pattern as voice_webrtc.js — no module system in this app.
if (typeof window !== 'undefined') {
    window.VoiceLLWebRTC = VoiceLLWebRTC;
}
