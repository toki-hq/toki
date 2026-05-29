use std::collections::{HashMap, VecDeque};
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
}

pub type SharedState = Arc<Mutex<ClientState>>;

pub fn shared() -> SharedState {
    Arc::new(Mutex::new(ClientState::default()))
}
