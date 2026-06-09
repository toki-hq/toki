//! Periodic registry snapshot broadcaster + the `Watch` stream adapter.
//!
//! The broadcaster snapshots the registry every [`SNAPSHOT_INTERVAL`] and
//! pushes the result (a `toki.admin.v1.Snapshot`) onto a tokio broadcast
//! channel. The gRPC `Watch` server-stream subscribes to that channel and
//! re-emits each snapshot to the connected browser. Mutating RPCs also
//! `send` a fresh snapshot immediately so the UI updates without waiting
//! for the next tick.
//!
//! This replaces the previous Server-Sent Events path; the snapshot
//! function is shared so a `Watch` open can emit the current state right
//! away rather than making the client wait a whole interval.

use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, Instant};

use tokio::sync::broadcast::{Receiver, Sender};
use tokio_stream::wrappers::BroadcastStream;
use tokio_stream::{Stream, StreamExt};
use tonic::Status;

use toki_proto::admin::v1 as pb;

use crate::metrics::{SharedByteCounters, SharedLiveRate};
use crate::state::{Client, SharedChannelNames, SharedRegistry};

/// Copy a session's verified identity (if any) onto the wire member.
/// Identity-less sessions keep the proto defaults (empty/0) — the UI
/// reads that as "no identity".
fn fill_member_identity(m: &mut pb::Member, c: &Client) {
    if let Some(identity) = &c.identity {
        m.identity = identity.display_id.clone();
        m.identity_pubkey = identity.pubkey_hex.clone();
        m.identity_machine_hash = identity.machine_hash.clone();
        m.identity_first_seen_unix = identity.first_seen.max(0) as u64;
    }
}

/// How often the broadcaster wakes, snapshots the registry, and fans the
/// result out to `Watch` subscribers. 1 Hz is plenty for an admin
/// dashboard; per-mutation pushes cover the snappy-feedback case.
pub const SNAPSHOT_INTERVAL: Duration = Duration::from_secs(1);

/// Top-level loop: tick, snapshot, broadcast. Never returns under normal
/// operation. `send` returning `Err(NoReceivers)` (no admin browsers
/// connected) is ignored — we keep snapshotting so the next subscriber
/// doesn't wait a whole interval.
pub async fn run_broadcaster(
    registry: SharedRegistry,
    channel_names: SharedChannelNames,
    counters: SharedByteCounters,
    live_rate: SharedLiveRate,
    tx: Sender<pb::Snapshot>,
    started_at: Instant,
) {
    let mut ticker = tokio::time::interval(SNAPSHOT_INTERVAL);
    ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
    let mut last_rx = counters.rx.load(Ordering::Relaxed);
    let mut last_tx = counters.tx.load(Ordering::Relaxed);
    let mut last_tick = Instant::now();
    loop {
        ticker.tick().await;
        // Recompute instantaneous throughput from the counter deltas over
        // the real elapsed interval (robust to a skipped/late tick), and
        // publish it so every snapshot source reflects the latest rate.
        let rx = counters.rx.load(Ordering::Relaxed);
        let tx_bytes = counters.tx.load(Ordering::Relaxed);
        let elapsed = last_tick.elapsed().as_secs_f64().max(0.001);
        live_rate.rx.store(
            (rx.saturating_sub(last_rx) as f64 / elapsed).round() as u64,
            Ordering::Relaxed,
        );
        live_rate.tx.store(
            (tx_bytes.saturating_sub(last_tx) as f64 / elapsed).round() as u64,
            Ordering::Relaxed,
        );
        last_rx = rx;
        last_tx = tx_bytes;
        last_tick = Instant::now();

        let snapshot = snapshot_now(
            &registry,
            &channel_names,
            &live_rate,
            next_generation(),
            started_at,
        )
        .await;
        let _ = tx.send(snapshot);
    }
}

/// Monotonic snapshot generation counter, shared by the periodic loop and
/// the per-mutation pushes so the UI can detect gaps/ordering.
pub fn next_generation() -> u64 {
    static GEN: AtomicU64 = AtomicU64::new(0);
    GEN.fetch_add(1, Ordering::Relaxed) + 1
}

/// Lock the registry, walk it, and produce a self-contained snapshot.
/// Shared by the broadcaster, the `Watch` open (immediate first frame),
/// and the post-mutation pushes.
pub async fn snapshot_now(
    registry: &SharedRegistry,
    channel_names: &SharedChannelNames,
    live_rate: &SharedLiveRate,
    generation: u64,
    started_at: Instant,
) -> pb::Snapshot {
    // Snapshot the name map up front (its own lock, held only here) so
    // the registry lock below never overlaps it.
    let names = channel_names.read().await.clone();
    let r = registry.lock().await;
    let now = Instant::now();

    // Walk the rooms table (not clients) so we render rooms whose members
    // are stale-but-not-yet-reaped; lobby = clients with no frequency.
    let mut rooms: Vec<pb::Room> = r
        .rooms
        .iter()
        .map(|(freq, room)| {
            let members = room
                .members
                .iter()
                .filter_map(|id| r.clients.get(id))
                .map(|c| {
                    let mut m = pb::Member {
                        id: c.id.clone(),
                        display_name: c.display_name.clone(),
                        connected_secs: now.saturating_duration_since(c.connected_at).as_secs(),
                        // Priority is per-channel: priority only if the elected
                        // frequency is the room we're listing them under.
                        priority: c.priority_freq.as_deref() == Some(freq.as_str()),
                        ..Default::default()
                    };
                    fill_member_identity(&mut m, c);
                    m
                })
                .collect();
            pb::Room {
                frequency: freq.clone(),
                holder: room.holder.clone(),
                members,
            }
        })
        .collect();
    // Stable, frequency-ascending order so the UI doesn't reshuffle.
    rooms.sort_by(|a, b| a.frequency.cmp(&b.frequency));

    let lobby: Vec<pb::Member> = r
        .clients
        .values()
        .filter(|c| c.current_frequency.is_none())
        .map(|c| {
            let mut m = pb::Member {
                id: c.id.clone(),
                display_name: c.display_name.clone(),
                connected_secs: now.saturating_duration_since(c.connected_at).as_secs(),
                priority: false,
                ..Default::default()
            };
            fill_member_identity(&mut m, c);
            m
        })
        .collect();

    pb::Snapshot {
        rooms,
        lobby,
        generation,
        server_uptime_secs: now.saturating_duration_since(started_at).as_secs(),
        // Carry every named channel regardless of occupancy so the panel
        // can label unoccupied frequencies too. This reflects the stored
        // names even while the feature is off (they go dormant, not
        // deleted) — the admin still sees them; the toggle only gates
        // delivery to clients and whether the editor is writable.
        channel_names: names,
        // Latest 1 Hz throughput, for the dashboard's live bandwidth trace.
        rx_bytes_per_sec: live_rate.rx.load(Ordering::Relaxed),
        tx_bytes_per_sec: live_rate.tx.load(Ordering::Relaxed),
    }
}

/// Adapt a freshly-subscribed broadcast receiver into the `Watch`
/// server-stream item type. Lagged items (a slow browser fell behind) are
/// skipped — the next tick is a fresh full snapshot, so the UI never
/// sticks on a stale view.
pub fn broadcast_stream(
    rx: Receiver<pb::Snapshot>,
) -> impl Stream<Item = Result<pb::Snapshot, Status>> {
    BroadcastStream::new(rx).filter_map(|res| res.ok().map(Ok))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::state::{Client, Room};
    use std::collections::HashMap;
    use std::sync::Arc;
    use tokio::sync::Mutex;

    fn mk_client(id: &str, name: &str, freq: Option<&str>) -> Client {
        Client {
            id: id.to_string(),
            identity: None,
            display_name: name.to_string(),
            audio_token_hash: [0u8; crate::state::TOKEN_HASH_LEN],
            audio_mac_key: [0u8; toki_proto::wire::MAC_KEY_LEN],
            audio_last_seq: 0,
            audio_outbound_seq: 1,
            audio_addr: None,
            events_tx: None,
            current_frequency: freq.map(str::to_string),
            last_seen: Instant::now(),
            connected_at: Instant::now(),
            priority_freq: None,
            expected_ip: None,
        }
    }

    #[tokio::test]
    async fn snapshot_groups_clients_by_frequency() {
        let mut reg = crate::state::Registry::default();
        reg.clients
            .insert("a".into(), mk_client("a", "Alice", Some("446.05")));
        reg.clients
            .insert("b".into(), mk_client("b", "Bob", Some("446.05")));
        reg.clients.insert("c".into(), mk_client("c", "Cara", None));
        reg.rooms.insert(
            "446.05".into(),
            Room {
                members: vec!["a".into(), "b".into()],
                holder: Some("a".into()),
                ..Default::default()
            },
        );
        let registry: SharedRegistry = Arc::new(Mutex::new(reg));
        let names = crate::state::shared_channel_names(Default::default());
        let lr = crate::metrics::shared_live_rate();

        let snap = snapshot_now(&registry, &names, &lr, 7, Instant::now()).await;
        assert_eq!(snap.generation, 7);
        assert_eq!(snap.rooms.len(), 1);
        assert_eq!(snap.rooms[0].frequency, "446.05");
        assert_eq!(snap.rooms[0].holder.as_deref(), Some("a"));
        assert_eq!(snap.rooms[0].members.len(), 2);
        assert_eq!(snap.lobby.len(), 1);
        assert_eq!(snap.lobby[0].id, "c");
    }

    #[tokio::test]
    async fn snapshot_marks_priority_only_on_matching_freq() {
        let mut reg = crate::state::Registry::default();
        let mut alice = mk_client("a", "Alice", Some("446.05"));
        alice.priority_freq = Some("446.05".into());
        let mut bob = mk_client("b", "Bob", Some("446.05"));
        bob.priority_freq = Some("447.00".into()); // dormant here
        reg.clients.insert("a".into(), alice);
        reg.clients.insert("b".into(), bob);
        reg.rooms.insert(
            "446.05".into(),
            Room {
                members: vec!["a".into(), "b".into()],
                holder: None,
                ..Default::default()
            },
        );
        let registry: SharedRegistry = Arc::new(Mutex::new(reg));
        let names = crate::state::shared_channel_names(Default::default());
        let lr = crate::metrics::shared_live_rate();

        let snap = snapshot_now(&registry, &names, &lr, 1, Instant::now()).await;
        let members: HashMap<_, _> = snap.rooms[0]
            .members
            .iter()
            .map(|m| (m.id.clone(), m.priority))
            .collect();
        assert!(members["a"]);
        assert!(!members["b"]);
    }

    #[tokio::test]
    async fn snapshot_carries_channel_names_for_all_named_freqs() {
        // Names cover unoccupied frequencies too: "447.00" has a name
        // but no room/members, and it must still surface in the map.
        let mut reg = crate::state::Registry::default();
        reg.clients
            .insert("a".into(), mk_client("a", "Alice", Some("446.05")));
        reg.rooms.insert(
            "446.05".into(),
            Room {
                members: vec!["a".into()],
                holder: None,
                ..Default::default()
            },
        );
        let registry: SharedRegistry = Arc::new(Mutex::new(reg));
        let mut seed = HashMap::new();
        seed.insert("446.05".to_string(), "Ops Net".to_string());
        seed.insert("447.00".to_string(), "Backup".to_string());
        let names = crate::state::shared_channel_names(seed);
        let lr = crate::metrics::shared_live_rate();

        let snap = snapshot_now(&registry, &names, &lr, 1, Instant::now()).await;
        assert_eq!(
            snap.channel_names.get("446.05").map(String::as_str),
            Some("Ops Net")
        );
        assert_eq!(
            snap.channel_names.get("447.00").map(String::as_str),
            Some("Backup")
        );
        assert_eq!(snap.channel_names.len(), 2);
    }
}
