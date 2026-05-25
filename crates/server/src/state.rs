use std::collections::HashMap;
use std::net::SocketAddr;
use std::sync::Arc;
use std::time::Instant;

use tokio::sync::{Mutex, mpsc};

use toki_proto::v1::ChannelEvent;

#[derive(Clone)]
pub struct Client {
    pub id: String,
    pub display_name: String,
    pub audio_token: Vec<u8>,
    pub audio_addr: Option<SocketAddr>,
    pub events_tx: Option<mpsc::Sender<ChannelEvent>>,
    pub channels: Vec<String>,
    /// Refreshed on every UDP packet from this client (keepalive or audio).
    /// The reaper evicts clients whose `last_seen` is older than the
    /// configured timeout — see `reaper`.
    pub last_seen: Instant,
}

#[derive(Default)]
pub struct ChannelInfo {
    pub members: Vec<String>,
    /// Walkie-talkie lock: client_id of the current PTT holder. At most one
    /// member may transmit at a time. `None` means the channel is free.
    pub holder: Option<String>,
}

#[derive(Default)]
pub struct Registry {
    pub clients: HashMap<String, Client>,
    pub channels: HashMap<String, ChannelInfo>,
    pub tokens: HashMap<Vec<u8>, String>,
}

pub type SharedRegistry = Arc<Mutex<Registry>>;

pub fn shared() -> SharedRegistry {
    Arc::new(Mutex::new(Registry::default()))
}
