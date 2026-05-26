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
    /// client_id → display_name for everyone in the room.
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
