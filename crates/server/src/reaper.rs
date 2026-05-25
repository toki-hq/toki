//! Heartbeat-based stale-client reaper.
//!
//! Every `INTERVAL` we scan the registry and evict any client whose
//! `last_seen` (refreshed on every inbound UDP packet — see [`crate::audio`])
//! is older than `TIMEOUT`. Eviction is the same cleanup that
//! `Signaling::leave_channel` performs:
//!
//!   - drop the client from the registry and token table
//!   - remove them from every channel's member list
//!   - if they were holding the PTT lock, clear it
//!   - broadcast `MemberLeft` to remaining channel members
//!   - if they were the holder, also broadcast a PTT release so peers'
//!     UIs unlock and play the release beep
//!
//! Effect: a client that crashes mid-transmission no longer freezes the
//! channel — the lock is released automatically within `TIMEOUT`.

use std::time::{Duration, Instant};

use tokio::sync::mpsc;
use tracing::info;

use toki_proto::v1::{ChannelEvent, MemberLeft, PttEvent, channel_event};

use crate::state::SharedRegistry;

/// How long a client may go without sending a UDP packet before we evict
/// them. The client's keepalive runs every 3 s, so 10 s tolerates two
/// missed keepalives plus jitter.
pub const TIMEOUT: Duration = Duration::from_secs(10);

/// How often the reaper scans. Should be well below `TIMEOUT` so eviction
/// happens promptly after the deadline passes.
pub const INTERVAL: Duration = Duration::from_secs(2);

pub async fn run(registry: SharedRegistry) {
    info!(timeout = ?TIMEOUT, interval = ?INTERVAL, "heartbeat reaper running");
    let mut ticker = tokio::time::interval(INTERVAL);
    ticker.tick().await; // consume the immediate first tick
    loop {
        ticker.tick().await;
        reap_once(&registry).await;
    }
}

async fn reap_once(registry: &SharedRegistry) {
    // Do all the registry mutation in a single critical section, and
    // collect the broadcast work to do once the lock is released. tx.send
    // on an mpsc channel can in principle .await on a full buffer, which
    // we don't want to do while holding the global registry lock.
    let broadcasts: Vec<EvictionBroadcast> = {
        let mut r = registry.lock().await;
        let now = Instant::now();

        let stale_ids: Vec<String> = r
            .clients
            .iter()
            .filter_map(|(id, c)| {
                (now.duration_since(c.last_seen) > TIMEOUT).then(|| id.clone())
            })
            .collect();

        let mut broadcasts = Vec::new();
        for id in stale_ids {
            let Some(client) = r.clients.remove(&id) else {
                continue;
            };
            r.tokens.remove(&client.audio_token);

            // One broadcast group per channel they were in: remaining
            // channel members get notified.
            for ch_name in &client.channels {
                let was_holder = if let Some(ch) = r.channels.get_mut(ch_name) {
                    ch.members.retain(|m| m != &id);
                    let holding = ch.holder.as_deref() == Some(id.as_str());
                    if holding {
                        ch.holder = None;
                    }
                    holding
                } else {
                    false
                };

                let recipients: Vec<mpsc::Sender<ChannelEvent>> = r
                    .channels
                    .get(ch_name)
                    .map(|c| c.members.clone())
                    .unwrap_or_default()
                    .iter()
                    .filter_map(|m| r.clients.get(m))
                    .filter_map(|c| c.events_tx.clone())
                    .collect();

                broadcasts.push(EvictionBroadcast {
                    client_id: id.clone(),
                    display_name: client.display_name.clone(),
                    channel: ch_name.clone(),
                    recipients,
                    was_holder,
                });
            }
        }
        broadcasts
    };

    for b in broadcasts {
        info!(
            client = %b.client_id,
            name = %b.display_name,
            channel = %b.channel,
            was_holder = b.was_holder,
            "evicted stale client",
        );
        let left = ChannelEvent {
            event: Some(channel_event::Event::Left(MemberLeft {
                client_id: b.client_id.clone(),
            })),
        };
        let release = b.was_holder.then(|| ChannelEvent {
            event: Some(channel_event::Event::Ptt(PttEvent {
                client_id: b.client_id.clone(),
                channel: b.channel.clone(),
                pressed: false,
                sequence: 0,
            })),
        });
        for tx in &b.recipients {
            let _ = tx.send(left.clone()).await;
            if let Some(ev) = &release {
                let _ = tx.send(ev.clone()).await;
            }
        }
    }
}

struct EvictionBroadcast {
    client_id: String,
    display_name: String,
    channel: String,
    recipients: Vec<mpsc::Sender<ChannelEvent>>,
    was_holder: bool,
}
