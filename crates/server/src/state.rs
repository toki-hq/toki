use std::collections::HashMap;
use std::net::SocketAddr;
use std::sync::Arc;

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
}

#[derive(Default)]
pub struct Registry {
    pub clients: HashMap<String, Client>,
    pub channels: HashMap<String, Vec<String>>,
    pub tokens: HashMap<Vec<u8>, String>,
}

pub type SharedRegistry = Arc<Mutex<Registry>>;

pub fn shared() -> SharedRegistry {
    Arc::new(Mutex::new(Registry::default()))
}
