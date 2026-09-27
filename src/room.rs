//! Rooms, sessions and the shared registry.
//!
//! A "session" is one viewer peer connection. Firestorm opens one per region it
//! can hear (its own plus any WebRTC-enabled neighbour — llvoicewebrtc.cpp:2237-2250),
//! so one agent may legitimately hold several sessions at once. Only the one whose
//! region is the agent's current region reports itself as primary via the join
//! message (llvoicewebrtc.cpp:3304-3307), and the viewer deliberately ignores joins
//! announced by non-primary servers (:3179-3186) so a neighbour cannot duplicate a
//! participant.

use parking_lot::{Mutex, RwLock};
use std::collections::{HashMap, HashSet, VecDeque};
use std::sync::atomic::{AtomicBool, AtomicU8, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

use crate::mixer::{self, FRAME_SAMPLES};
use crate::proto::{self, RosterEntry};

/// How many decoded frames we are willing to hold per speaker before dropping the
/// oldest. 10 frames == 200 ms; past that the audio is stale enough that keeping it
/// only adds delay.
const JITTER_MAX_FRAMES: usize = 10;

/// Which room a session belongs to.
///
/// Spatial ("local") voice is keyed on region + parcel because that is exactly what
/// the viewer asks for: one connection per region, carrying parcel_local_id only
/// when parcel voice is in play (llvoicewebrtc.cpp:2796-2799). Group and IM voice
/// ("multiagent") is keyed on the channel id the viewer supplies (:3400-3401).
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub enum RoomKey {
    Spatial { region: String, parcel: i32 },
    MultiAgent { channel: String },
}

impl std::fmt::Display for RoomKey {
    /// Used by the admin page's session list — see main.rs's sessions_table_html.
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            RoomKey::Spatial { region, parcel } => write!(f, "{region} (parcel {parcel})"),
            RoomKey::MultiAgent { channel } => write!(f, "multiagent {channel}"),
        }
    }
}

#[derive(Clone, Copy, Debug, Default)]
pub struct SpatialState {
    pub avatar_pos: [f64; 3],
    pub avatar_rot: [f64; 4],
    pub listener_pos: [f64; 3],
    pub listener_rot: [f64; 4],
    /// False until the viewer has actually sent a position. Until then the session
    /// must not be placed at the origin — that would make every silent newcomer
    /// audible to everyone standing near <0,0,0>.
    pub known: bool,
}

impl SpatialState {
    fn identity() -> Self {
        SpatialState {
            avatar_rot: [0.0, 0.0, 0.0, 1.0],
            listener_rot: [0.0, 0.0, 0.0, 1.0],
            ..Default::default()
        }
    }
}

/// Per-session mutable audio and control state.
///
/// The peer connection itself is deliberately NOT stored here: it is owned by the
/// task that created it, and everything the mixer needs (the outbound track, the
/// decoded frames) is reachable through this struct. That keeps the mixer free of
/// any lock that a network callback also wants.
pub struct Session {
    pub id: String,
    pub agent_id: String,
    pub room: RoomKey,
    pub spatial: bool,
    /// When this session was created. Read only by the admin page's session list
    /// (main.rs's sessions_table_html) to show how long each participant has been
    /// connected — nothing on the mixer path depends on it.
    pub created_at: Instant,

    pub state: RwLock<SpatialState>,
    /// Listener-chosen gain per speaker, already converted out of the viewer's
    /// PEER_GAIN_CONVERSION_FACTOR units.
    pub gains: RwLock<HashMap<String, f32>>,
    pub mutes: RwLock<HashMap<String, bool>>,

    /// Decoded 20 ms mono frames from THIS session's microphone.
    pub inbound: Mutex<VecDeque<Vec<f32>>>,
    /// Most recent speech level of this session, on the viewer's 0-127 scale.
    pub level: AtomicU8,

    pub joined: AtomicBool,
    pub primary: AtomicBool,
    pub closed: AtomicBool,
    /// True while this viewer's SLData data channel is open. Roster updates can only be
    /// delivered then, so nothing may be marked as announced to this listener before it.
    pub dc_open: AtomicBool,
    /// When the peer connection last entered WebRTC's Disconnected state, or None if it
    /// is not currently disconnected. Disconnected does not by itself mean the peer is
    /// gone — see session.rs's on_connection_state_change — but one that never recovers
    /// is escalated to closed after a grace period by escalate_stale_disconnects, so a
    /// truly-dead peer does not hold its port and mixer slot forever.
    pub disconnected_since: Mutex<Option<Instant>>,

    /// Agents we have already announced to this listener, so a join is sent once.
    announced: Mutex<HashSet<String>>,
}

impl Session {
    pub fn new(id: String, agent_id: String, room: RoomKey, spatial: bool) -> Self {
        Session {
            id,
            agent_id,
            room,
            spatial,
            created_at: Instant::now(),
            state: RwLock::new(SpatialState::identity()),
            gains: RwLock::new(HashMap::new()),
            mutes: RwLock::new(HashMap::new()),
            inbound: Mutex::new(VecDeque::new()),
            level: AtomicU8::new(0),
            joined: AtomicBool::new(false),
            primary: AtomicBool::new(false),
            closed: AtomicBool::new(false),
            dc_open: AtomicBool::new(false),
            disconnected_since: Mutex::new(None),
            announced: Mutex::new(HashSet::new()),
        }
    }

    /// Push one decoded frame, dropping the oldest if the listener is not keeping up.
    pub fn push_frame(&self, frame: Vec<f32>) {
        let mut q = self.inbound.lock();
        if q.len() >= JITTER_MAX_FRAMES {
            q.pop_front();
        }
        q.push_back(frame);
    }

    /// Take the next frame for this tick, or None for silence.
    pub fn take_frame(&self) -> Option<Vec<f32>> {
        self.inbound.lock().pop_front()
    }

    /// Apply a control message that arrived on the data channel.
    pub fn apply_data(&self, m: &proto::ViewerDataMessage) {
        if let Some(primary) = m.join {
            self.joined.store(true, Ordering::Relaxed);
            self.primary.store(primary, Ordering::Relaxed);
            log::info!(
                "session {} agent {} join primary={primary}",
                self.id,
                self.agent_id
            );
        }

        // Reject anything not finite before it reaches the mixer. JSON can carry
        // 1e308, and squaring that gives inf; inf/inf in the panning maths is NaN,
        // which would then be summed into the mix and handed to the Opus encoder.
        let finite3 = |v: Option<[f64; 3]>| v.filter(|a| a.iter().all(|c| c.is_finite()));
        let finite4 = |v: Option<[f64; 4]>| v.filter(|a| a.iter().all(|c| c.is_finite()));
        let m = &proto::ViewerDataMessage {
            join: m.join,
            avatar_pos: finite3(m.avatar_pos),
            avatar_rot: finite4(m.avatar_rot),
            listener_pos: finite3(m.listener_pos),
            listener_rot: finite4(m.listener_rot),
            user_gain: m.user_gain.clone(),
            user_mute: m.user_mute.clone(),
        };

        if m.avatar_pos.is_some()
            || m.avatar_rot.is_some()
            || m.listener_pos.is_some()
            || m.listener_rot.is_some()
        {
            let mut s = self.state.write();
            if let Some(p) = m.avatar_pos {
                s.avatar_pos = p;
                s.known = true;
            }
            if let Some(r) = m.avatar_rot {
                s.avatar_rot = r;
            }
            if let Some(p) = m.listener_pos {
                s.listener_pos = p;
            }
            if let Some(r) = m.listener_rot {
                s.listener_rot = r;
            }
            // Re-apply the 50 m tether server-side. llvoicewebrtc.cpp:1209-1212 is
            // explicit that this must be enforced here and not merely in the client:
            // a modified viewer could otherwise move its ear anywhere.
            if s.known {
                s.listener_pos = mixer::tether(s.listener_pos, s.avatar_pos);
            }
        }

        if !m.user_gain.is_empty() {
            let mut g = self.gains.write();
            for (id, raw) in &m.user_gain {
                // Clamped: a raw u32 divides down to a gain of ~19 million, and the
                // map is bounded so a client cannot grow it without limit.
                let gain = (*raw as f32 / proto::PEER_GAIN_CONVERSION_FACTOR)
                    .clamp(0.0, proto::MAX_PEER_GAIN);
                if g.len() < proto::MAX_PEER_ENTRIES || g.contains_key(id) {
                    g.insert(id.clone(), gain);
                }
            }
        }
        if !m.user_mute.is_empty() {
            let mut mu = self.mutes.write();
            for (id, muted) in &m.user_mute {
                if mu.len() < proto::MAX_PEER_ENTRIES || mu.contains_key(id) {
                    mu.insert(id.clone(), *muted);
                }
            }
        }
    }

    /// Gain this listener wants applied to `speaker`, combining their explicit
    /// volume choice with their mute list. Defaults to unity for an unknown peer.
    pub fn peer_gain(&self, speaker: &str) -> f32 {
        if *self.mutes.read().get(speaker).unwrap_or(&false) {
            return 0.0;
        }
        *self.gains.read().get(speaker).unwrap_or(&1.0)
    }

    /// True the first time this listener is told about `agent`.
    fn mark_announced(&self, agent: &str) -> bool {
        self.announced.lock().insert(agent.to_owned())
    }

    fn forget_announced(&self, agent: &str) {
        self.announced.lock().remove(agent);
    }
}

/// All live sessions, indexed both ways.
#[derive(Default)]
pub struct Registry {
    by_session: RwLock<HashMap<String, Arc<Session>>>,
    by_room: RwLock<HashMap<RoomKey, Vec<String>>>,
}

impl Registry {
    pub fn insert(&self, s: Arc<Session>) {
        self.by_room
            .write()
            .entry(s.room.clone())
            .or_default()
            .push(s.id.clone());
        self.by_session.write().insert(s.id.clone(), s);
    }

    pub fn remove(&self, id: &str) -> Option<Arc<Session>> {
        let s = self.by_session.write().remove(id)?;
        let mut rooms = self.by_room.write();
        if let Some(members) = rooms.get_mut(&s.room) {
            members.retain(|m| m != id);
            if members.is_empty() {
                rooms.remove(&s.room);
            }
        }
        // Everyone who had been told about this agent should hear about them again
        // if they come back.
        drop(rooms);
        for other in self.by_session.read().values() {
            other.forget_announced(&s.agent_id);
        }
        s.closed.store(true, Ordering::Relaxed);
        Some(s)
    }

    pub fn rooms(&self) -> Vec<RoomKey> {
        self.by_room.read().keys().cloned().collect()
    }

    pub fn members(&self, room: &RoomKey) -> Vec<Arc<Session>> {
        let ids = match self.by_room.read().get(room) {
            Some(v) => v.clone(),
            None => return Vec::new(),
        };
        let map = self.by_session.read();
        ids.iter().filter_map(|id| map.get(id).cloned()).collect()
    }

    pub fn session_count(&self) -> usize {
        self.by_session.read().len()
    }
}

/// Escalate any session that has been reported Disconnected (not Failed/Closed) for at
/// least `grace` with no recovery to Connected. WebRTC's Disconnected state alone does
/// not mean the peer is gone — it fires on a few seconds of missed connectivity checks
/// and genuinely recovers for real viewers on flaky links, which is why
/// on_connection_state_change deliberately does not treat it as terminal. But one that
/// never recovers would otherwise hold its media port and mixer slot forever. Setting
/// `closed` here is enough: the mixer loop's own reaper (main.rs) already tears down
/// anything with `closed` set, on its next tick.
pub fn escalate_stale_disconnects(sessions: &[Arc<Session>], grace: Duration) {
    for s in sessions {
        if s.closed.load(Ordering::Relaxed) {
            continue;
        }
        let since = *s.disconnected_since.lock();
        if since.is_some_and(|t| t.elapsed() >= grace) {
            log::warn!(
                "session {} agent {} disconnected for over {grace:?} with no recovery; closing",
                s.id,
                s.agent_id
            );
            s.closed.store(true, Ordering::Relaxed);
        }
    }
}

/// Reusable stereo mix buffers, so a fresh `Vec<f32>` (1920 samples, ~7.5 KB) is not
/// allocated and dropped for every listener on every 20 ms tick. A stress test at 120
/// sessions showed the mixer missing its tick deadline well before CPU was the limit
/// (211% of one core used out of 1600% available, yet ticks still got skipped) — this
/// and the matching Opus-encode buffer in session.rs are the two per-tick allocations
/// on that path. Module-private and Mutex-guarded rather than threaded through
/// `mix_room`'s signature, so the function stays testable exactly as before with no
/// pool to construct in every test.
static STEREO_POOL: Mutex<Vec<Vec<f32>>> = Mutex::new(Vec::new());

// Pool logic takes the pool explicitly so tests can exercise it against a private,
// isolated instance instead of the real global one — this crate's tests run each
// #[test] concurrently, and mix_room calls from unrelated tests hit STEREO_POOL too, so
// asserting exact buffer identity against the shared static would be flaky.
fn take_from(pool: &Mutex<Vec<Vec<f32>>>) -> Vec<f32> {
    pool.lock().pop().unwrap_or_else(|| vec![0.0f32; FRAME_SAMPLES * 2])
}

fn give_to(pool: &Mutex<Vec<Vec<f32>>>, buf: Vec<f32>) {
    let mut p = pool.lock();
    // Cap how many idle buffers accumulate: a burst of listeners leaving at once should
    // not let this grow without bound. 256 buffers is ~2 MB, far more than any real tick
    // count, so this is a safety ceiling, not a tuned limit.
    if p.len() < 256 {
        p.push(buf);
    }
}

fn take_stereo_buf() -> Vec<f32> {
    take_from(&STEREO_POOL)
}

fn give_stereo_buf(buf: Vec<f32>) {
    give_to(&STEREO_POOL, buf)
}

/// What one listener should be sent this tick: the mixed stereo frame and any
/// roster changes.
pub struct ListenerOutput {
    pub session: Arc<Session>,
    pub stereo: Vec<f32>,
    pub roster: Option<String>,
}

impl Drop for ListenerOutput {
    fn drop(&mut self) {
        // Runs once this tick's send has completed (or been skipped) and the caller's
        // ListenerOutput goes out of scope, on every path — success, a missing
        // endpoint, or a closed data channel — since it is unconditional cleanup, not
        // tied to how the send went. std::mem::take leaves an empty Vec behind (no
        // allocation) and moves the real buffer back to the pool.
        give_stereo_buf(std::mem::take(&mut self.stereo));
    }
}

/// Compute one 20 ms tick for a whole room.
///
/// Pulled out of the network layer so it is testable without any WebRTC at all.
pub fn mix_room(members: &[Arc<Session>]) -> Vec<ListenerOutput> {
    // Pull exactly one frame per speaker, once, and reuse it for every listener.
    let mut frames: Vec<(Arc<Session>, Option<Vec<f32>>)> = Vec::with_capacity(members.len());
    for m in members {
        let f = m.take_frame();
        if let Some(ref frame) = f {
            m.level.store(mixer::level_to_wire(frame), Ordering::Relaxed);
        } else {
            m.level.store(0, Ordering::Relaxed);
        }
        frames.push((m.clone(), f));
    }

    let mut out = Vec::with_capacity(members.len());

    for listener in members {
        let lstate = *listener.state.read();
        let mut contributions: Vec<mixer::Contribution<'_>> = Vec::new();

        for (speaker, frame) in &frames {
            if speaker.id == listener.id {
                continue; // never echo a listener back to themselves
            }
            let Some(frame) = frame else { continue };

            let mut gain = listener.peer_gain(&speaker.agent_id);
            if gain <= 0.0 {
                continue;
            }

            let pan = if listener.spatial {
                let sstate = *speaker.state.read();
                // A speaker who has not yet reported a position is not placed at
                // the origin — they are simply not mixed until they do.
                if !sstate.known || !lstate.known {
                    continue;
                }
                let d = {
                    let dx = sstate.avatar_pos[0] - lstate.listener_pos[0];
                    let dy = sstate.avatar_pos[1] - lstate.listener_pos[1];
                    let dz = sstate.avatar_pos[2] - lstate.listener_pos[2];
                    (dx * dx + dy * dy + dz * dz).sqrt()
                };
                let dg = mixer::distance_gain(d);
                if dg <= 0.0 {
                    continue; // out of range entirely
                }
                gain *= dg;
                mixer::pan_weights(sstate.avatar_pos, lstate.listener_pos, lstate.listener_rot)
            } else {
                // Group/IM voice is not positional: centre everyone.
                (std::f32::consts::FRAC_1_SQRT_2, std::f32::consts::FRAC_1_SQRT_2)
            };

            contributions.push(mixer::Contribution { frame, gain, pan });
        }

        let mut stereo = take_stereo_buf();
        debug_assert_eq!(stereo.len(), FRAME_SAMPLES * 2, "pooled buffer has the wrong length");
        mixer::mix_stereo(&contributions, &mut stereo);

        out.push(ListenerOutput {
            session: listener.clone(),
            stereo,
            // Build (and so record as announced) only when it can actually be delivered:
            // otherwise a listener whose channel opens a moment after it joins never
            // learns about the people already in the room.
            //
            // The listener must also have finished its OWN join (primary for spatial):
            // until then its viewer is still setting the session up, and a join
            // announced to it that early can be dropped by the viewer and is never
            // repeated. That happens whenever someone joins a room that already has
            // people in it.
            roster: if listener.dc_open.load(Ordering::Relaxed)
                && listener.joined.load(Ordering::Relaxed)
                && (listener.primary.load(Ordering::Relaxed) || !listener.spatial)
            {
                build_roster(listener, &frames)
            } else {
                None
            },
        });
    }

    out
}

/// Build the roster update for one listener, or None when nothing changed.
fn build_roster(listener: &Arc<Session>, frames: &[(Arc<Session>, Option<Vec<f32>>)]) -> Option<String> {
    let mut entries: Vec<(String, RosterEntry)> = Vec::new();

    for (speaker, _) in frames {
        // Report every participant including the listener: the viewer keys its own
        // participant list off these ids and shows its own dot from them too.
        //
        // Do not announce anyone until their own join is complete. Firestorm sends a
        // first join WITHOUT "p" and a second one with "p":true once its connection is
        // up (llvoicewebrtc.cpp sendJoin / SESSION_UP). A viewer only adds a participant
        // from a spatial join that carries "p":true, so announcing on the first join —
        // and never again — leaves that person permanently missing from the listener's
        // list, and with no entry there is no speaking dot above their avatar.
        let ready = speaker.joined.load(Ordering::Relaxed)
            && (speaker.primary.load(Ordering::Relaxed) || !speaker.spatial);
        if !ready {
            continue;
        }

        let level = speaker.level.load(Ordering::Relaxed);
        let joined = listener.mark_announced(&speaker.agent_id);
        let speaking =
            (level as f32 / proto::LEVEL_SCALE_TO_WIRE) > proto::SPEAKING_AUDIO_LEVEL;

        // Only emit an entry when there is something to say: a first-time join, or
        // an active level. Otherwise a silent room would push a full roster every
        // 20 ms for no reason.
        if !joined && level == 0 {
            continue;
        }

        entries.push((
            speaker.agent_id.clone(),
            RosterEntry {
                joined,
                primary: speaker.primary.load(Ordering::Relaxed),
                power: level,
                speaking,
                moderator_muted: false,
            },
        ));
    }

    if entries.is_empty() {
        return None;
    }
    Some(proto::roster_json(&entries))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn session(id: &str, agent: &str, pos: [f64; 3]) -> Arc<Session> {
        let s = Arc::new(Session::new(
            id.into(),
            agent.into(),
            RoomKey::Spatial { region: "region".into(), parcel: 0 },
            true,
        ));
        {
            let mut st = s.state.write();
            st.avatar_pos = pos;
            st.listener_pos = pos;
            st.known = true;
        }
        // A fully joined viewer: announced its join with "p":true.
        s.joined.store(true, Ordering::Relaxed);
        s.primary.store(true, Ordering::Relaxed);
        s.dc_open.store(true, Ordering::Relaxed);
        s
    }

    #[test]
    fn nobody_is_announced_to_a_listener_that_has_not_finished_joining() {
        // b joins a room where a is already talking. Until b's own join (primary) has
        // arrived, b's viewer is not ready to take announcements.
        let a = session("a", "agent-a", [0.0, 0.0, 0.0]);
        let b = session("b", "agent-b", [1.0, 0.0, 0.0]);
        b.primary.store(false, Ordering::Relaxed);
        loud(&a);
        let out = mix_room(&[a.clone(), b.clone()]);
        assert!(
            out.iter().find(|o| o.session.id == "b").unwrap().roster.is_none(),
            "no roster before b's own join is complete"
        );

        b.primary.store(true, Ordering::Relaxed);
        loud(&a);
        let out = mix_room(&[a.clone(), b.clone()]);
        let r = out.iter().find(|o| o.session.id == "b").unwrap().roster.clone().unwrap();
        assert!(r.contains("agent-a") && r.contains(r#""j""#), "a announced once b is ready: {r}");
    }

    #[test]
    fn nobody_is_announced_to_a_listener_whose_channel_is_not_open_yet() {
        // b joined 40 s after a. While b's data channel is still opening, a roster built
        // for b would be dropped — it must not count as announced.
        let a = session("a", "agent-a", [0.0, 0.0, 0.0]);
        let b = session("b", "agent-b", [1.0, 0.0, 0.0]);
        b.dc_open.store(false, Ordering::Relaxed);
        loud(&a);
        let out = mix_room(&[a.clone(), b.clone()]);
        let for_b = out.iter().find(|o| o.session.id == "b").unwrap();
        assert!(for_b.roster.is_none(), "no roster while b's channel is closed");

        // b's channel opens: a must now be announced to b.
        b.dc_open.store(true, Ordering::Relaxed);
        loud(&a);
        let out = mix_room(&[a.clone(), b.clone()]);
        let r = out.iter().find(|o| o.session.id == "b").unwrap().roster.clone().unwrap();
        assert!(r.contains("agent-a") && r.contains(r#""j""#), "a announced to b once open: {r}");
    }

    #[test]
    fn join_is_announced_only_once_the_speaker_is_primary() {
        // Firestorm's real order: first join has no "p", the second has "p":true.
        let a = session("a", "agent-a", [0.0, 0.0, 0.0]);
        let b = session("b", "agent-b", [1.0, 0.0, 0.0]);
        b.primary.store(false, Ordering::Relaxed); // first join, no "p" yet
        loud(&b);

        let out = mix_room(&[a.clone(), b.clone()]);
        let r = out.iter().find(|o| o.session.id == "a").unwrap().roster.clone().unwrap_or_default();
        assert!(!r.contains("agent-b"), "must not announce b before it is primary: {r}");

        // b's second join arrives, carrying "p":true.
        b.primary.store(true, Ordering::Relaxed);
        loud(&b);
        let out = mix_room(&[a.clone(), b.clone()]);
        let r = out.iter().find(|o| o.session.id == "a").unwrap().roster.clone().unwrap();
        assert!(r.contains("agent-b"), "b must be announced once primary: {r}");
        assert!(r.contains(r#""j":{"p":true}"#), "join must carry p:true: {r}");
    }

    fn loud(s: &Arc<Session>) {
        s.push_frame(vec![0.4f32; FRAME_SAMPLES]);
    }

    #[test]
    fn stale_disconnect_is_closed_only_after_the_grace_period() {
        let a = session("a", "agent-a", [0.0, 0.0, 0.0]);

        // Never disconnected: untouched.
        escalate_stale_disconnects(std::slice::from_ref(&a), Duration::from_secs(60));
        assert!(!a.closed.load(Ordering::Relaxed));

        // Disconnected, but well within grace.
        *a.disconnected_since.lock() = Some(Instant::now());
        escalate_stale_disconnects(std::slice::from_ref(&a), Duration::from_secs(60));
        assert!(!a.closed.load(Ordering::Relaxed), "must not close before grace elapses");

        // Disconnected long enough ago.
        *a.disconnected_since.lock() = Some(Instant::now() - Duration::from_secs(61));
        escalate_stale_disconnects(std::slice::from_ref(&a), Duration::from_secs(60));
        assert!(a.closed.load(Ordering::Relaxed), "must close once grace has elapsed");
    }

    #[test]
    fn recovered_connection_is_never_escalated() {
        let a = session("a", "agent-a", [0.0, 0.0, 0.0]);
        // Cleared by on_connection_state_change on reaching Connected — see session.rs.
        *a.disconnected_since.lock() = None;
        escalate_stale_disconnects(std::slice::from_ref(&a), Duration::from_secs(0));
        assert!(!a.closed.load(Ordering::Relaxed), "None must never be treated as disconnected");
    }

    #[test]
    fn stereo_buffer_is_recycled_not_reallocated() {
        // A private pool, not the shared STEREO_POOL: exact pointer identity would be
        // flaky against the real one while other tests' mix_room calls run concurrently.
        let pool: Mutex<Vec<Vec<f32>>> = Mutex::new(Vec::new());

        let buf = take_from(&pool);
        let ptr = buf.as_ptr();
        give_to(&pool, buf);

        let recycled = take_from(&pool);
        assert_eq!(recycled.as_ptr(), ptr, "expected the same allocation back, not a new one");
        assert_eq!(recycled.len(), FRAME_SAMPLES * 2);
    }

    #[test]
    fn listener_output_drop_returns_its_buffer_to_the_shared_pool() {
        // Unlike the test above, this one deliberately goes through the real
        // give_stereo_buf/STEREO_POOL, since that is what ListenerOutput::drop is wired
        // to — the point is to prove that wiring exists, not to re-prove recycling
        // itself. A count comparison (pool grew by exactly one) is NOT safe here: other
        // tests' own ListenerOutputs are being dropped into this same shared pool at the
        // same time. Checking that our specific buffer is present afterwards is robust
        // to that unrelated traffic — the only way to defeat it is another thread
        // popping this exact buffer back out in the same instant, which is negligible
        // for synchronous, no-yield-point operations like these.
        let a = session("a", "agent-a", [0.0, 0.0, 0.0]);
        loud(&a);
        let mut out = mix_room(std::slice::from_ref(&a));
        let ptr = out[0].stereo.as_ptr();
        drop(out.pop().unwrap());

        let mut pool = STEREO_POOL.lock();
        let pos = pool.iter().position(|b| b.as_ptr() == ptr);
        assert!(pos.is_some(), "ListenerOutput's Drop impl must return its buffer to STEREO_POOL");
        pool.swap_remove(pos.unwrap()); // leave the shared pool as we found it
    }

    #[test]
    fn listener_never_hears_themselves() {
        let a = session("a", "agent-a", [0.0, 0.0, 0.0]);
        loud(&a);
        let out = mix_room(std::slice::from_ref(&a));
        assert_eq!(out.len(), 1);
        assert!(
            out[0].stereo.iter().all(|s| *s == 0.0),
            "a lone speaker must hear silence, not their own voice"
        );
    }

    #[test]
    fn nearby_speaker_is_audible_and_distant_one_is_not() {
        let a = session("a", "agent-a", [0.0, 0.0, 0.0]);
        let near = session("b", "agent-b", [5.0, 0.0, 0.0]);
        loud(&near);
        let out = mix_room(&[a.clone(), near.clone()]);
        let for_a = out.iter().find(|o| o.session.id == "a").unwrap();
        assert!(
            for_a.stereo.iter().any(|s| s.abs() > 0.01),
            "a speaker 5 m away must be audible"
        );

        // Beyond MAX_DISTANCE (60 m) they must be gone completely.
        let far = session("c", "agent-c", [500.0, 0.0, 0.0]);
        loud(&far);
        let out = mix_room(&[a.clone(), far.clone()]);
        let for_a = out.iter().find(|o| o.session.id == "a").unwrap();
        assert!(
            for_a.stereo.iter().all(|s| s.abs() < 1e-6),
            "a speaker 500 m away must be inaudible"
        );
    }

    #[test]
    fn each_listener_gets_its_own_mix() {
        // The whole point of per-listener mixing: A hears B on one side while B
        // hears A on the other. A shared room mix cannot express this.
        let a = session("a", "agent-a", [0.0, 0.0, 0.0]);
        let b = session("b", "agent-b", [5.0, 0.0, 0.0]);
        loud(&a);
        loud(&b);
        let out = mix_room(&[a.clone(), b.clone()]);

        let for_a = out.iter().find(|o| o.session.id == "a").unwrap();
        let for_b = out.iter().find(|o| o.session.id == "b").unwrap();

        // B is east of A, so A hears B louder on the right.
        let a_left: f32 = for_a.stereo.iter().step_by(2).map(|s| s.abs()).sum();
        let a_right: f32 = for_a.stereo.iter().skip(1).step_by(2).map(|s| s.abs()).sum();
        assert!(a_right > a_left, "A should hear B to the right");

        // A is west of B, so B hears A louder on the left.
        let b_left: f32 = for_b.stereo.iter().step_by(2).map(|s| s.abs()).sum();
        let b_right: f32 = for_b.stereo.iter().skip(1).step_by(2).map(|s| s.abs()).sum();
        assert!(b_left > b_right, "B should hear A to the left");
    }

    #[test]
    fn per_listener_mute_and_gain_are_respected() {
        let a = session("a", "agent-a", [0.0, 0.0, 0.0]);
        let b = session("b", "agent-b", [1.0, 0.0, 0.0]);
        a.mutes.write().insert("agent-b".into(), true);
        loud(&b);
        let out = mix_room(&[a.clone(), b.clone()]);
        let for_a = out.iter().find(|o| o.session.id == "a").unwrap();
        assert!(
            for_a.stereo.iter().all(|s| s.abs() < 1e-6),
            "a muted speaker must be silent for that listener only"
        );

        // Half gain must be quieter than unity, and it must not affect anyone else.
        let c = session("c", "agent-c", [0.0, 0.0, 0.0]);
        c.gains.write().insert("agent-b".into(), 0.5);
        let d = session("d", "agent-d", [0.0, 0.0, 0.0]);
        loud(&b);
        let out = mix_room(&[c.clone(), d.clone(), b.clone()]);
        let for_c: f32 = out.iter().find(|o| o.session.id == "c").unwrap()
            .stereo.iter().map(|s| s.abs()).sum();
        let for_d: f32 = out.iter().find(|o| o.session.id == "d").unwrap()
            .stereo.iter().map(|s| s.abs()).sum();
        assert!(for_d > for_c * 1.5, "half gain should be clearly quieter: c={for_c} d={for_d}");
    }

    #[test]
    fn position_unknown_speakers_are_not_placed_at_the_origin() {
        let a = session("a", "agent-a", [0.0, 0.0, 0.0]);
        let ghost = Arc::new(Session::new(
            "g".into(),
            "agent-g".into(),
            RoomKey::Spatial { region: "region".into(), parcel: 0 },
            true,
        ));
        ghost.push_frame(vec![0.9f32; FRAME_SAMPLES]);
        let out = mix_room(&[a.clone(), ghost.clone()]);
        let for_a = out.iter().find(|o| o.session.id == "a").unwrap();
        assert!(
            for_a.stereo.iter().all(|s| s.abs() < 1e-6),
            "a speaker with no reported position must not be audible at <0,0,0>"
        );
    }

    #[test]
    fn roster_announces_a_join_once() {
        let a = session("a", "agent-a", [0.0, 0.0, 0.0]);
        let b = session("b", "agent-b", [1.0, 0.0, 0.0]);
        loud(&b);
        let out = mix_room(&[a.clone(), b.clone()]);
        let r = out.iter().find(|o| o.session.id == "a").unwrap().roster.clone().unwrap();
        assert!(r.contains("agent-b"), "first tick announces the join: {r}");
        assert!(r.contains(r#""j""#));

        // Second tick, still speaking: no repeated join for the same agent.
        loud(&b);
        let out = mix_room(&[a.clone(), b.clone()]);
        let r = out.iter().find(|o| o.session.id == "a").unwrap().roster.clone().unwrap();
        assert!(!r.contains(r#""j""#), "join must not repeat: {r}");
    }

    #[test]
    fn silent_room_produces_no_roster_traffic() {
        let a = session("a", "agent-a", [0.0, 0.0, 0.0]);
        let b = session("b", "agent-b", [1.0, 0.0, 0.0]);
        // First tick announces joins.
        let _ = mix_room(&[a.clone(), b.clone()]);
        // Second tick, nobody speaking, nothing new to say.
        let out = mix_room(&[a.clone(), b.clone()]);
        assert!(
            out.iter().all(|o| o.roster.is_none()),
            "an idle room must not push a roster every tick"
        );
    }

    #[test]
    fn removing_a_session_drops_its_room_entry() {
        let reg = Registry::default();
        let a = session("a", "agent-a", [0.0, 0.0, 0.0]);
        let room = a.room.clone();
        reg.insert(a);
        assert_eq!(reg.members(&room).len(), 1);
        reg.remove("a");
        assert_eq!(reg.session_count(), 0);
        assert!(reg.rooms().is_empty(), "empty rooms must not linger");
    }

    #[test]
    fn jitter_buffer_drops_oldest_rather_than_growing() {
        let a = session("a", "agent-a", [0.0, 0.0, 0.0]);
        for i in 0..(JITTER_MAX_FRAMES + 5) {
            a.push_frame(vec![i as f32; FRAME_SAMPLES]);
        }
        assert_eq!(a.inbound.lock().len(), JITTER_MAX_FRAMES);
        // The surviving frames must be the NEWEST ones.
        let first = a.take_frame().unwrap()[0];
        assert_eq!(first, 5.0);
    }

    #[test]
    fn gain_conversion_uses_the_viewer_factor() {
        let a = session("a", "agent-a", [0.0, 0.0, 0.0]);
        // 220 == unity per PEER_GAIN_CONVERSION_FACTOR.
        let m = proto::ViewerDataMessage {
            user_gain: vec![("agent-b".into(), 220)],
            ..Default::default()
        };
        a.apply_data(&m);
        assert!((a.peer_gain("agent-b") - 1.0).abs() < 1e-6);

        let m = proto::ViewerDataMessage {
            user_gain: vec![("agent-b".into(), 110)],
            ..Default::default()
        };
        a.apply_data(&m);
        assert!((a.peer_gain("agent-b") - 0.5).abs() < 1e-6);
    }

    #[test]
    fn tether_is_reapplied_server_side() {
        let a = session("a", "agent-a", [0.0, 0.0, 0.0]);
        // A hostile client claims its ear is 1 km away from its avatar.
        let m = proto::ViewerDataMessage {
            avatar_pos: Some([0.0, 0.0, 0.0]),
            listener_pos: Some([1000.0, 0.0, 0.0]),
            ..Default::default()
        };
        a.apply_data(&m);
        let st = *a.state.read();
        let d = (st.listener_pos[0] - st.avatar_pos[0]).abs();
        assert!(
            (d - mixer::MAX_AUDIO_DIST).abs() < 1e-6,
            "ear must be clamped to 50 m, got {d}"
        );
    }
}
