use std::collections::{HashMap, HashSet, VecDeque};
use std::sync::{Arc, Mutex};

/// State shared between the GUI thread and the tokio runtime thread.
///
/// The runtime writes (connection status, member list, log lines, current
/// PTT holder); the GUI reads each frame and renders a snapshot.
#[derive(Default)]
pub struct ClientState {
    pub connection: ConnState,
    pub self_id: Option<String>,
    /// Our own display name, mirrored here so the runtime can re-seed
    /// `members` with ourselves after a frequency change (the server's
    /// roster backfill only contains *other* members).
    pub display_name: String,
    /// Currently-joined frequency room, e.g. `"446.05"`. `None` between
    /// connect and the initial Join, and again after disconnect.
    pub frequency: Option<String>,
    /// Admin-assigned name of the current channel, delivered by the
    /// server (`ChannelNameChanged`) on join / change-frequency and on
    /// live rename. `None` when the channel is unnamed or the server's
    /// named-channels feature is off; cleared on every frequency change
    /// so a stale label never sticks to the wrong frequency.
    pub channel_name: Option<String>,
    /// client_id → display_name for everyone on the current frequency.
    pub members: HashMap<String, String>,
    /// Walkie-talkie lock: client_id of whoever is currently transmitting,
    /// or `None` if the floor is free. Updated only from authoritative
    /// server broadcasts — the local press never sets this.
    pub holder: Option<String>,
    /// client_ids an operator has server-side muted, for the roster
    /// badge. Populated from `MuteChanged` events; pruned when a member
    /// leaves. Our own id appears here when *we're* muted (the runtime
    /// also mirrors that into the session's `self_muted` gate).
    pub muted: HashSet<String>,
    /// `true` when the *current* channel is muted by an operator (no one
    /// may transmit on it). Delivered by `ChannelMuteChanged` on join /
    /// change-frequency and on live toggle; cleared on every frequency
    /// change so a stale mute never sticks to the wrong channel. Folds
    /// into the local "can I talk" check alongside our own member-mute.
    pub channel_muted: bool,
    /// `true` when *we* are a priority speaker on the current channel
    /// (admin-granted, per-channel). Delivered by `PriorityChanged` on
    /// grant/revoke and on change-frequency; cleared on every frequency
    /// change (the server re-asserts it for the new channel). A priority
    /// speaker is the No-Talk exception — they keep a live PTT button
    /// even on a muted channel — so this *overrides* `channel_muted` in
    /// `locally_silenced` (but never an individual member-mute).
    pub channel_priority: bool,
    /// `true` when an admin has granted us the global-broadcast capability
    /// for this session. Set by `BroadcastCapabilityChanged`. The broadcast
    /// PTT binding is inert until this is true. Session-scoped; re-asserted
    /// by the server on join/change-frequency, so never cleared locally on
    /// channel hop — let the server's `BroadcastCapabilityChanged` be the
    /// source of truth. Does NOT affect `locally_silenced()` / normal-speak
    /// gating.
    pub can_broadcast: bool,
    /// Live connection-quality readout for the current session, published
    /// by the runtime's measurement task. `None` while disconnected; the
    /// UI strip reads it each frame for the signal-bars glyph. Not part of
    /// `#[derive(Default)]`'s concern — it's an `Option`, so default-None.
    pub conn_quality: Option<crate::telemetry::QualityHandle>,
    pub log: VecDeque<String>,
}

#[derive(Default, Clone, PartialEq, Eq)]
pub enum ConnState {
    #[default]
    Disconnected,
    Connecting,
    Connected,
    Failed(String),
}

impl ClientState {
    pub fn log<S: Into<String>>(&mut self, line: S) {
        if self.log.len() >= 200 {
            self.log.pop_front();
        }
        self.log.push_back(line.into());
    }

    /// Record a member's server-side mute state for the roster badge.
    pub fn set_muted(&mut self, client_id: &str, muted: bool) {
        if muted {
            self.muted.insert(client_id.to_string());
        } else {
            self.muted.remove(client_id);
        }
    }

    /// Is this member currently server-side muted? No UI consumer on
    /// the minimal radio strip yet — kept as the accessor a future
    /// per-member roster badge / self-muted strip indicator reads,
    /// alongside the `muted` set it queries.
    #[allow(dead_code)]
    pub fn is_muted(&self, client_id: &str) -> bool {
        self.muted.contains(client_id)
    }

    /// Are *we* currently barred from transmitting? Mirrors the server's
    /// speak-gate so the PTT "unable to talk" cue matches what the server
    /// will actually do:
    ///   * member-mute (we're individually silenced) always bars us —
    ///     an individual sanction outranks any channel grant;
    ///   * otherwise channel-mute bars us, *unless* we're a priority
    ///     speaker on this channel (the No-Talk granted-voice exception).
    pub fn locally_silenced(&self) -> bool {
        let member_muted = self
            .self_id
            .as_deref()
            .is_some_and(|id| self.muted.contains(id));
        member_muted || (self.channel_muted && !self.channel_priority)
    }
}

pub type SharedState = Arc<Mutex<ClientState>>;

pub fn shared() -> SharedState {
    Arc::new(Mutex::new(ClientState::default()))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn can_broadcast_defaults_false() {
        let s = ClientState::default();
        assert!(!s.can_broadcast);
        // can_broadcast does not affect locally_silenced.
        let mut s2 = ClientState {
            self_id: Some("me".into()),
            can_broadcast: true,
            channel_muted: true,
            ..Default::default()
        };
        // Channel-muted without priority → silenced regardless of can_broadcast.
        assert!(s2.locally_silenced());
        // Grant priority — priority unblocks us (not can_broadcast).
        s2.channel_priority = true;
        assert!(!s2.locally_silenced());
    }

    #[test]
    fn mute_set_tracks_membership() {
        let mut s = ClientState::default();
        assert!(!s.is_muted("alice"));
        s.set_muted("alice", true);
        assert!(s.is_muted("alice"));
        // Idempotent set; unrelated member unaffected.
        s.set_muted("alice", true);
        assert!(s.is_muted("alice"));
        assert!(!s.is_muted("bob"));
        s.set_muted("alice", false);
        assert!(!s.is_muted("alice"));
        // Clearing an absent member is a harmless no-op.
        s.set_muted("ghost", false);
        assert!(!s.is_muted("ghost"));
    }

    #[test]
    fn locally_silenced_covers_member_and_channel_mute() {
        let mut s = ClientState {
            self_id: Some("me".into()),
            ..Default::default()
        };
        assert!(!s.locally_silenced());
        // Personal member-mute silences us.
        s.set_muted("me", true);
        assert!(s.locally_silenced());
        s.set_muted("me", false);
        assert!(!s.locally_silenced());
        // Channel-mute silences us independently of member-mute.
        s.channel_muted = true;
        assert!(s.locally_silenced());
        s.channel_muted = false;
        assert!(!s.locally_silenced());
        // Another member's mute never silences *us*.
        s.set_muted("someone-else", true);
        assert!(!s.locally_silenced());
    }

    #[test]
    fn priority_speaker_overrides_channel_mute_but_not_member_mute() {
        let mut s = ClientState {
            self_id: Some("me".into()),
            ..Default::default()
        };
        // No-Talk channel: muted, but we're a priority speaker → we can
        // still talk (the granted-voice exception).
        s.channel_muted = true;
        s.channel_priority = true;
        assert!(!s.locally_silenced());
        // A personal member-mute outranks the priority grant — still silenced.
        s.set_muted("me", true);
        assert!(s.locally_silenced());
        // Lift the member-mute: priority exception applies again.
        s.set_muted("me", false);
        assert!(!s.locally_silenced());
        // Lose priority on a still-muted channel → silenced once more.
        s.channel_priority = false;
        assert!(s.locally_silenced());
    }
}
