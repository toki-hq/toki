use std::net::SocketAddr;

use tokio::net::UdpSocket;
use tracing::{debug, warn};

use crate::state::SharedRegistry;

// Minimum UDP packet: [16-byte token][1-byte version][... opus frame ...]
const TOKEN_LEN: usize = 16;
const HEADER_LEN: usize = TOKEN_LEN + 1;
const MAX_PACKET: usize = 1500;

pub async fn run(bind: SocketAddr, registry: SharedRegistry) -> anyhow::Result<()> {
    let socket = UdpSocket::bind(bind).await?;
    tracing::info!(?bind, "audio relay listening");

    let mut buf = vec![0u8; MAX_PACKET];

    loop {
        let (len, peer) = match socket.recv_from(&mut buf).await {
            Ok(v) => v,
            Err(e) => {
                warn!(error = %e, "udp recv failed");
                continue;
            }
        };

        if len < HEADER_LEN {
            debug!(len, "packet too small");
            continue;
        }

        let token = &buf[..TOKEN_LEN];
        let payload = &buf[TOKEN_LEN..len];

        let targets: Vec<SocketAddr> = {
            let mut registry = registry.lock().await;
            let Some(sender_id) = registry.tokens.get(token).cloned() else {
                debug!(?peer, "unknown audio token");
                continue;
            };

            if let Some(client) = registry.clients.get_mut(&sender_id) {
                client.audio_addr = Some(peer);
            }

            let active_channels: Vec<String> = registry
                .clients
                .get(&sender_id)
                .map(|c| c.channels.clone())
                .unwrap_or_default();

            let mut targets: Vec<SocketAddr> = Vec::new();
            for channel in &active_channels {
                if let Some(members) = registry.channels.get(channel) {
                    for id in members {
                        if id == &sender_id {
                            continue;
                        }
                        if let Some(other) = registry.clients.get(id) {
                            if let Some(addr) = other.audio_addr {
                                targets.push(addr);
                            }
                        }
                    }
                }
            }
            targets
        };

        for addr in targets {
            if let Err(e) = socket.send_to(payload, addr).await {
                warn!(error = %e, "failed to forward audio");
            }
        }
    }
}
