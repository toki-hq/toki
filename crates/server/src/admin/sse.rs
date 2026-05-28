//! Periodic registry snapshot broadcaster + SSE endpoint helpers.
//!
//! # Design choice: poll, don't instrument
//!
//! For v1 the broadcaster snapshots the registry every
//! [`SNAPSHOT_INTERVAL`] and pushes the result to a tokio broadcast
//! channel. SSE handlers subscribe to the channel and re-emit each
//! snapshot as a JSON event. The alternative — instrumenting every
//! signaling.rs mutation site to publish deltas — was rejected for
//! v1 because:
//!
//! * It would touch ~6 sites in `signaling.rs` and the reaper,
//!   expanding the blast radius of an "admin panel" PR.
//! * The admin UI doesn't need sub-second latency; a busy server's
//!   member-join cadence is on the order of seconds.
//! * The snapshot path is also exactly what `GET /api/state` needs,
//!   so we get the snapshot endpoint "for free".
//!
//! When (if) admin needs lower latency or per-event deltas, swap this
//! file's loop for hooks in the signaling helpers — the channel
//! contract and JS consumer don't have to change.

use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, Instant};

use tokio::sync::broadcast::Sender;
use tokio_stream::wrappers::BroadcastStream;
use tokio_stream::StreamExt;

use crate::state::SharedRegistry;

use super::dto::{MemberDto, RoomDto, Snapshot};

/// How often the broadcaster wakes, snapshots the registry, and
/// fans the result out to SSE subscribers. 1Hz is plenty for an
/// admin dashboard — the operator perceives it as "live" and the
/// per-snapshot CPU cost is dominated by the JSON serialisation,
/// which is microseconds.
pub const SNAPSHOT_INTERVAL: Duration = Duration::from_secs(1);

/// Top-level loop: tick, snapshot, broadcast. Never returns under
/// normal operation. If all subscribers disconnect, `send` returns
/// `Err(NoReceivers)` — we ignore it because new subscribers will
/// arrive whenever a fresh `/api/events` request lands.
///
/// `started_at` is the timestamp the admin task booted; every
/// snapshot carries `now - started_at` as `server_uptime_secs`.
pub async fn run_broadcaster(registry: SharedRegistry, tx: Sender<Snapshot>, started_at: Instant) {
    static GEN: AtomicU64 = AtomicU64::new(0);
    let mut ticker = tokio::time::interval(SNAPSHOT_INTERVAL);
    // `Burst` skipped ticks rather than firing late ones back-to-
    // back if the snapshot ever runs over its budget. This will
    // virtually never happen but is the conservative default for
    // background pumps.
    ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
    loop {
        ticker.tick().await;
        let generation = GEN.fetch_add(1, Ordering::Relaxed) + 1;
        let snapshot = snapshot_now(&registry, generation, started_at).await;
        // Ignore "no receivers" — that just means no admin browsers
        // are currently connected. We keep snapshotting so the next
        // subscriber doesn't have to wait a whole interval.
        let _ = tx.send(snapshot);
    }
}

/// Lock the registry, walk it, and produce a self-contained
/// snapshot. Public so `GET /api/state` can call it without
/// going through the broadcaster (synchronous first-paint path).
pub async fn snapshot_now(
    registry: &SharedRegistry,
    generation: u64,
    started_at: Instant,
) -> Snapshot {
    let r = registry.lock().await;
    let now = Instant::now();

    // Group by frequency. We walk the rooms table (not the clients
    // table) because we want to render even rooms whose members are
    // all stale-but-not-yet-reaped; conversely, lobby = clients with
    // no current_frequency.
    let mut rooms: Vec<RoomDto> = r
        .rooms
        .iter()
        .map(|(freq, room)| {
            let members: Vec<MemberDto> = room
                .members
                .iter()
                .filter_map(|id| r.clients.get(id))
                .map(|c| MemberDto {
                    id: c.id.clone(),
                    display_name: c.display_name.clone(),
                    connected_secs: now.saturating_duration_since(c.connected_at).as_secs(),
                })
                .collect();
            RoomDto {
                frequency: freq.clone(),
                holder: room.holder.clone(),
                members,
            }
        })
        .collect();
    // Stable, frequency-ascending order so the UI doesn't reshuffle
    // every snapshot.
    rooms.sort_by(|a, b| a.frequency.cmp(&b.frequency));

    let lobby: Vec<MemberDto> = r
        .clients
        .values()
        .filter(|c| c.current_frequency.is_none())
        .map(|c| MemberDto {
            id: c.id.clone(),
            display_name: c.display_name.clone(),
            connected_secs: now.saturating_duration_since(c.connected_at).as_secs(),
        })
        .collect();

    Snapshot {
        rooms,
        lobby,
        generation,
        server_uptime_secs: now.saturating_duration_since(started_at).as_secs(),
    }
}

/// Build the SSE response stream from a freshly-subscribed receiver.
/// Wraps each broadcast item in an `Event` with the `state` name so
/// the JS can `addEventListener('state', ...)`.
pub fn build_sse_stream(
    rx: tokio::sync::broadcast::Receiver<Snapshot>,
) -> impl tokio_stream::Stream<Item = Result<axum::response::sse::Event, std::convert::Infallible>>
{
    BroadcastStream::new(rx).filter_map(|res| {
        // BroadcastStream emits `Err(Lagged(n))` when the subscriber
        // has fallen `n` snapshots behind. We just skip the laggy
        // items; the next successful tick is a fresh full snapshot,
        // so the UI is never stuck on a stale view.
        let snap = res.ok()?;
        let event = axum::response::sse::Event::default()
            .event("state")
            .json_data(&snap)
            .ok()?;
        Some(Ok::<_, std::convert::Infallible>(event))
    })
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
            expected_ip: None,
        }
    }

    #[tokio::test]
    async fn snapshot_groups_clients_by_frequency() {
        // Build a hand-rolled registry with two rooms and a lobby
        // client; snapshot_now should reproduce the same grouping.
        let mut reg = crate::state::Registry::default();
        reg.clients
            .insert("a".into(), mk_client("a", "Alice", Some("446.05")));
        reg.clients
            .insert("b".into(), mk_client("b", "Bob", Some("446.05")));
        reg.clients
            .insert("c".into(), mk_client("c", "Carol", Some("446.10")));
        reg.clients.insert("d".into(), mk_client("d", "Dave", None));
        reg.rooms.insert(
            "446.05".into(),
            Room {
                members: vec!["a".into(), "b".into()],
                holder: Some("a".into()),
            },
        );
        reg.rooms.insert(
            "446.10".into(),
            Room {
                members: vec!["c".into()],
                holder: None,
            },
        );
        let shared: SharedRegistry = Arc::new(Mutex::new(reg));

        let snap = snapshot_now(&shared, 7, Instant::now()).await;
        assert_eq!(snap.generation, 7);
        // Frequencies are sorted lexicographically.
        assert_eq!(snap.rooms.len(), 2);
        assert_eq!(snap.rooms[0].frequency, "446.05");
        assert_eq!(snap.rooms[0].holder.as_deref(), Some("a"));
        assert_eq!(snap.rooms[0].members.len(), 2);
        assert_eq!(snap.rooms[1].frequency, "446.10");
        assert!(snap.rooms[1].holder.is_none());
        // Lobby contains exactly the un-joined client.
        assert_eq!(snap.lobby.len(), 1);
        assert_eq!(snap.lobby[0].id, "d");
    }

    #[tokio::test]
    async fn snapshot_with_empty_registry_is_well_formed() {
        // A fresh server has no clients and no rooms; the snapshot
        // must still serialize cleanly (no NaN, no missing fields).
        let reg: SharedRegistry = Arc::new(Mutex::new(crate::state::Registry::default()));
        let snap = snapshot_now(&reg, 1, Instant::now()).await;
        assert!(snap.rooms.is_empty());
        assert!(snap.lobby.is_empty());
        let json = serde_json::to_string(&snap).unwrap();
        assert!(json.contains("\"rooms\":[]"));
    }

    // Suppress unused HashMap import on builds where the test config
    // changes — keeps the file self-consistent with future tests.
    #[allow(dead_code)]
    fn _hashmap_marker() -> HashMap<String, String> {
        HashMap::new()
    }
}
