use std::collections::HashMap;
use std::net::{IpAddr, SocketAddr};
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
    /// The frequency room the client is currently in. `None` between
    /// `Register` and `Join`, and again after `Leave`. We key the
    /// audio relay's forwarding fan-out off this — silent clients on
    /// frequency A never receive a sender's voice on frequency B.
    pub current_frequency: Option<String>,
    /// Refreshed on every UDP packet from this client (keepalive or audio).
    /// The reaper evicts clients whose `last_seen` is older than the
    /// configured timeout — see `reaper`.
    pub last_seen: Instant,
    /// IP the gRPC `Register` call came from (`None` only on
    /// transports that don't expose one, e.g. Unix sockets). The
    /// audio relay enforces that incoming UDP packets bearing this
    /// client's token *must* originate from this IP — closes the
    /// "token-capture → audio hijack" path. The *port* is allowed to
    /// vary because NAT will usually pick a different one for UDP.
    pub expected_ip: Option<IpAddr>,
}

/// One frequency channel. Each holds its own member list and PTT lock —
/// frequencies are independent walkie-talkie channels that don't see
/// each other's traffic.
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
    /// Rooms keyed by frequency string (e.g. `"446.05"`). Lazily
    /// inserted on first join; we don't pre-populate the full
    /// 41-channel grid because most are usually empty.
    pub rooms: HashMap<String, Room>,
    pub tokens: HashMap<Vec<u8>, String>,
}

pub type SharedRegistry = Arc<Mutex<Registry>>;

pub fn shared() -> SharedRegistry {
    Arc::new(Mutex::new(Registry::default()))
}
