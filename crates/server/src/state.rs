use std::collections::HashMap;
use std::net::SocketAddr;
use std::sync::Arc;
use std::time::Instant;

use tokio::sync::{Mutex, mpsc};

use toki_proto::v1::Event;

#[derive(Clone)]
pub struct Client {
    pub id: String,
    pub display_name: String,
    pub audio_token: Vec<u8>,
    pub audio_addr: Option<SocketAddr>,
    pub events_tx: Option<mpsc::Sender<Event>>,
    /// True after a successful `Join` — false at register time and after
    /// `Leave`. We track this explicitly (rather than just checking
    /// `events_tx.is_some()`) so the audio relay can skip non-members
    /// even if their event stream is still being torn down.
    pub joined: bool,
    /// Refreshed on every UDP packet from this client (keepalive or audio).
    /// The reaper evicts clients whose `last_seen` is older than the
    /// configured timeout — see `reaper`.
    pub last_seen: Instant,
}

/// The one global room. Toki used to support multiple named channels but
/// nobody used the abstraction, so it was collapsed into this single
/// shared room: every joined client is a member, and at most one of them
/// holds PTT at a time.
#[derive(Default)]
pub struct Room {
    pub members: Vec<String>,
    /// Walkie-talkie lock: client_id of the current PTT holder. At most one
    /// member may transmit at a time. `None` means the room is free.
    pub holder: Option<String>,
}

#[derive(Default)]
pub struct Registry {
    pub clients: HashMap<String, Client>,
    pub room: Room,
    pub tokens: HashMap<Vec<u8>, String>,
}

pub type SharedRegistry = Arc<Mutex<Registry>>;

pub fn shared() -> SharedRegistry {
    Arc::new(Mutex::new(Registry::default()))
}
