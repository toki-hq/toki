//! Heartbeat-based stale-client reaper.
//!
//! Every `INTERVAL` we scan the registry and evict any client whose
//! `last_seen` (refreshed on every inbound UDP packet — see [`crate::audio`])
//! is older than `TIMEOUT`. Eviction is the same cleanup that
//! `Signaling::leave` performs:
//!
//!   - drop the client from the registry and token table
//!   - remove them from the room's member list
//!   - if they were holding the PTT lock, clear it
//!   - broadcast `MemberLeft` to remaining room members
//!   - if they were the holder, also broadcast a PTT release so peers'
//!     UIs unlock and play the release beep
//!
//! Effect: a client that crashes mid-transmission no longer freezes the
//! room — the lock is released automatically within `TIMEOUT`.

use std::time::{Duration, Instant};

use tokio::sync::mpsc;
use tracing::info;

use toki_proto::v1::{event, Event, MemberLeft, PttEvent};

use crate::audit::{self, AuditSink};
use crate::server_config::SharedServerConfig;
use crate::state::SharedRegistry;

/// How often the reaper scans. Should be well below the eviction
/// timeout so eviction happens promptly after the deadline passes.
/// We keep this hardcoded — adjusting tick frequency from the admin
/// UI would muddy "how often" with "how aggressive", and operators
/// almost never want anything other than ~2 s.
pub const INTERVAL: Duration = Duration::from_secs(2);

pub async fn run(registry: SharedRegistry, server_config: SharedServerConfig, audit: AuditSink) {
    info!(interval = ?INTERVAL, "heartbeat reaper running");
    let mut ticker = tokio::time::interval(INTERVAL);
    ticker.tick().await; // consume the immediate first tick
    loop {
        ticker.tick().await;
        // Read the operator-configured idle threshold *per tick* so a
        // live update via the admin UI takes effect on the next scan
        // — at the cost of one extra RwLock read every 2 s, which is
        // unmeasurable.
        let timeout = Duration::from_secs(server_config.read().await.idle_kick_secs as u64);
        reap_once(&registry, timeout, &audit).await;
    }
}

async fn reap_once(registry: &SharedRegistry, timeout: Duration, audit: &AuditSink) {
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
            .filter(|(_, c)| now.duration_since(c.last_seen) > timeout)
            .map(|(id, _)| id.clone())
            .collect();

        let mut broadcasts = Vec::new();
        for id in stale_ids {
            let Some(client) = r.clients.remove(&id) else {
                continue;
            };
            r.tokens.remove(&client.audio_token_hash);

            // Audit every eviction as a disconnect (incl. clients that
            // never joined a room). The sink is an unbounded channel, so
            // this send is synchronous and safe under the registry lock.
            audit::record(
                audit,
                "disconnect",
                &client.display_name,
                client.current_frequency.as_deref().unwrap_or(""),
                "idle timeout",
            );

            // If this client held the global broadcast, tear it down and
            // collect all remaining clients' senders for the fleet-wide
            // broadcast-release notification.
            let broadcast_release_txs: Vec<mpsc::Sender<Event>> =
                if r.broadcast_active.as_deref() == Some(id.as_str()) {
                    r.broadcast_active = None;
                    r.clients
                        .values()
                        .filter_map(|c| c.events_tx.clone())
                        .collect()
                } else {
                    Vec::new()
                };

            // Pull them out of their frequency room (if any). Mirror
            // of `Signaling::leave`'s cleanup, minus the explicit RPC.
            let Some(frequency) = client.current_frequency.clone() else {
                // Never joined a room — but still need to send broadcast teardown.
                broadcasts.push(EvictionBroadcast {
                    client_id: id.clone(),
                    display_name: client.display_name.clone(),
                    frequency: String::new(),
                    recipients: Vec::new(),
                    was_holder: false,
                    broadcast_release_txs,
                });
                continue;
            };
            let was_holder = if let Some(room) = r.rooms.get_mut(&frequency) {
                room.members.retain(|m| m != &id);
                let holding = room.holder.as_deref() == Some(id.as_str());
                if holding {
                    room.holder = None;
                }
                holding
            } else {
                false
            };
            // Drop the room if it just emptied.
            if let Some(room) = r.rooms.get(&frequency) {
                if room.members.is_empty() && room.holder.is_none() {
                    r.rooms.remove(&frequency);
                }
            }

            let recipients: Vec<mpsc::Sender<Event>> = r
                .rooms
                .get(&frequency)
                .map(|room| room.members.clone())
                .unwrap_or_default()
                .iter()
                .filter_map(|m| r.clients.get(m))
                .filter_map(|c| c.events_tx.clone())
                .collect();

            broadcasts.push(EvictionBroadcast {
                client_id: id.clone(),
                display_name: client.display_name.clone(),
                frequency,
                recipients,
                was_holder,
                broadcast_release_txs,
            });
        }
        broadcasts
    };

    for b in broadcasts {
        info!(
            client = %b.client_id,
            name = %b.display_name,
            frequency = %b.frequency,
            was_holder = b.was_holder,
            "evicted stale client",
        );

        // If the evicted client was broadcasting, tell all remaining clients
        // the broadcast is over so their UIs clear the broadcast indicator.
        if !b.broadcast_release_txs.is_empty() {
            let bcast_end = Event {
                event: Some(event::Event::Ptt(PttEvent {
                    client_id: b.client_id.clone(),
                    pressed: false,
                    sequence: 0,
                    priority: false,
                    broadcast: true,
                    display_name: String::new(),
                })),
            };
            for tx in &b.broadcast_release_txs {
                let _ = tx.send(bcast_end.clone()).await;
            }
        }

        if b.frequency.is_empty() {
            // Lobby client — no room events to send.
            continue;
        }

        let left = Event {
            event: Some(event::Event::Left(MemberLeft {
                client_id: b.client_id.clone(),
            })),
        };
        let release = b.was_holder.then(|| Event {
            event: Some(event::Event::Ptt(PttEvent {
                client_id: b.client_id.clone(),
                pressed: false,
                sequence: 0,
                priority: false,
                broadcast: false,
                display_name: String::new(),
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
    frequency: String,
    recipients: Vec<mpsc::Sender<Event>>,
    was_holder: bool,
    /// Event senders for all remaining clients, populated when the evicted
    /// client was holding the global broadcast — used to send the fleet-wide
    /// broadcast-release notification after the registry lock is dropped.
    broadcast_release_txs: Vec<mpsc::Sender<Event>>,
}
