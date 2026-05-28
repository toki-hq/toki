//! HTTP handlers for the admin API.
//!
//! All handlers return either `Result<impl IntoResponse, StatusCode>`
//! or a concrete `Response` so axum's error handling stays consistent
//! and there's exactly one place per failure mode that converts to a
//! status code.
//!
//! # Mutation handlers (`kick` / `move` / `rename`)
//!
//! Each one:
//!   1. Validates input (frequency canonicalisation, display-name limits).
//!   2. Acquires the registry lock, performs the mutation, snapshots the
//!      list of channels to notify, releases the lock.
//!   3. Awaits the broadcasts off-lock so a slow consumer can't stall
//!      the global registry.
//!   4. Logs the action at INFO with the admin username for forensics.

use std::time::Duration;

use axum::{
    extract::{Path, State},
    http::{header, StatusCode},
    response::{sse::KeepAlive, IntoResponse, Json, Response, Sse},
    Extension,
};
use tokio::sync::mpsc;
use tracing::info;

use toki_proto::v1::{
    event, DisplayNameChanged, Event, FrequencyChanged, MemberJoined, MemberLeft, PttEvent,
};

use crate::signaling;
use crate::validation;

use super::auth::{
    self, generate_session_token, session_clear_cookie, session_set_cookie, AdminUser, COOKIE_NAME,
};
use super::db::now_unix;
use super::dto::{ApiError, LoginRequest, MoveRequest, RenameRequest, Snapshot};
use super::sse::{build_sse_stream, snapshot_now};
use super::AppState;

/// `GET /` — serve the embedded SPA shell.
///
/// We `include_str!` the HTML at compile time so the binary is
/// self-contained and there's no asset path to misconfigure in
/// production. The same pattern is used for `/static/app.js` and
/// `/static/style.css`.
///
/// All three asset responses carry `Cache-Control: no-cache` so a
/// `cargo build && cargo run` cycle reliably reaches the browser
/// without forcing the operator to hard-refresh (Cmd/Ctrl-Shift-R)
/// every time. Browsers happily cache `/static/style.css` and
/// `/static/app.js` *separately* from the HTML page that links to
/// them, which was the source of an extremely confusing "I rebuilt
/// the server but the page still looks the same" bug in v1.
pub async fn index() -> impl IntoResponse {
    static HTML: &str = include_str!("assets/index.html");
    asset_response("text/html; charset=utf-8", HTML)
}

/// `GET /static/app.js` — embedded JS bundle.
pub async fn static_js() -> impl IntoResponse {
    static JS: &str = include_str!("assets/app.js");
    asset_response("application/javascript; charset=utf-8", JS)
}

/// `GET /static/style.css` — embedded stylesheet.
pub async fn static_css() -> impl IntoResponse {
    static CSS: &str = include_str!("assets/style.css");
    asset_response("text/css; charset=utf-8", CSS)
}

/// Shared envelope for the three embedded asset endpoints: sets the
/// right Content-Type and forces revalidation on every request. The
/// admin panel is internal-tooling-grade traffic; we lose nothing by
/// skipping the cache and gain robustness against stale-asset bugs.
fn asset_response(content_type: &'static str, body: &'static str) -> impl IntoResponse {
    (
        [
            (header::CONTENT_TYPE, content_type),
            (header::CACHE_CONTROL, "no-cache, must-revalidate"),
        ],
        body,
    )
}

/// `POST /api/login`. Public route. Verifies the username/password
/// against argon2id, mints a session row, sets the cookie. Returns
/// `204 No Content` on success.
pub async fn login(
    State(state): State<AppState>,
    Json(body): Json<LoginRequest>,
) -> Result<Response, (StatusCode, Json<ApiError>)> {
    // Opportunistic prune so the sessions table doesn't grow on a
    // server that's been up for weeks. Cheap when the table is empty.
    let _ = state.db.prune_expired_sessions().await;

    let stored = state
        .db
        .get_password_hash(&body.username)
        .await
        .map_err(internal_error)?;
    // Always run the verify path even on a missing user so the
    // timing signal between "user exists" and "user doesn't" is
    // dominated by the argon2 step. Compare against a sentinel hash
    // of fixed cost rather than skipping the check.
    let ok = match stored {
        Some(hash) => auth::verify_password(&body.password, &hash),
        None => {
            // Run a hash to spend the same CPU we would on a real
            // verify; result is intentionally discarded.
            let _ = auth::verify_password(
                &body.password,
                // PHC-format placeholder. Argon2's PasswordHash::new
                // accepts this so we exercise the same code path.
                "$argon2id$v=19$m=19456,t=2,p=1$YWFhYWFhYWFhYWFhYWFhYQ$AAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA",
            );
            false
        }
    };
    if !ok {
        tracing::warn!(username = %body.username, "admin login failed");
        return Err((
            StatusCode::UNAUTHORIZED,
            Json(ApiError::new("invalid username or password")),
        ));
    }

    // Mint a session row + cookie. TTL came from AdminConfig at
    // startup; we recompute the absolute expiry off the same value.
    let token = generate_session_token();
    let ttl_secs = state.session_ttl.as_secs();
    let expires_at = now_unix() + ttl_secs as i64;
    state
        .db
        .create_session(&token, &body.username, expires_at)
        .await
        .map_err(internal_error)?;
    info!(username = %body.username, "admin login success");

    let mut response = StatusCode::NO_CONTENT.into_response();
    response
        .headers_mut()
        .insert(header::SET_COOKIE, session_set_cookie(&token, ttl_secs));
    Ok(response)
}

/// `POST /api/logout`. Removes the session row (so the cookie value
/// stops being valid even if the browser keeps it) and emits a
/// `Max-Age=0` cookie so the browser drops it.
pub async fn logout(
    State(state): State<AppState>,
    headers: axum::http::HeaderMap,
) -> Result<Response, StatusCode> {
    if let Some(token) = auth::extract_session_cookie(&headers) {
        let _ = state.db.delete_session(&token).await;
    }
    let mut response = StatusCode::NO_CONTENT.into_response();
    response
        .headers_mut()
        .insert(header::SET_COOKIE, session_clear_cookie());
    Ok(response)
}

/// `GET /api/state`. Synchronous full-snapshot endpoint. Used by the
/// JS on first paint and as a fallback when the SSE stream breaks.
/// Authentication is enforced by the middleware layer.
pub async fn state_snapshot(State(state): State<AppState>) -> Json<Snapshot> {
    // generation = 0 here is fine; the JS only uses it for ordering
    // across SSE messages, and the snapshot path is one-shot.
    let snap = snapshot_now(&state.registry, 0).await;
    Json(snap)
}

/// `GET /api/events`. SSE stream of `Snapshot` payloads. Subscribes
/// to the broadcaster created in `admin::run` and pipes each
/// snapshot to the connected browser as a `state` event.
pub async fn events(
    State(state): State<AppState>,
) -> Sse<
    impl tokio_stream::Stream<Item = Result<axum::response::sse::Event, std::convert::Infallible>>,
> {
    let rx = state.broadcaster.subscribe();
    let stream = build_sse_stream(rx);
    // Send a keepalive comment every 15s so proxies (nginx, etc.)
    // don't close idle connections. The default `KeepAlive` emits
    // `: keep-alive\n\n` which is invisible to the EventSource API.
    Sse::new(stream).keep_alive(KeepAlive::new().interval(Duration::from_secs(15)))
}

/// `POST /api/clients/:id/kick`. Evicts a client from the registry
/// entirely — same effect as if the reaper had timed them out. The
/// kicked client's gRPC streams will see their channels close on
/// the next send and they'll bounce back to the connect screen.
pub async fn kick(
    State(state): State<AppState>,
    Path(id): Path<String>,
    Extension(admin): Extension<AdminUser>,
) -> Result<StatusCode, (StatusCode, Json<ApiError>)> {
    // Snapshot the work to do under the lock, drop it before
    // awaiting the broadcasts (which can yield).
    let plan = {
        let mut registry = state.registry.lock().await;
        let Some(client) = registry.clients.remove(&id) else {
            return Err((
                StatusCode::NOT_FOUND,
                Json(ApiError::new("client not found")),
            ));
        };
        registry.tokens.remove(&client.audio_token_hash);

        // If they were in a room, remove them from it and collect the
        // notify-list. Mirrors the reaper's per-client cleanup.
        let mut recipients: Vec<mpsc::Sender<Event>> = Vec::new();
        let mut was_holder = false;
        let frequency = client.current_frequency.clone();
        if let Some(freq) = &frequency {
            if let Some(room) = registry.rooms.get_mut(freq) {
                room.members.retain(|m| m != &id);
                if room.holder.as_deref() == Some(id.as_str()) {
                    room.holder = None;
                    was_holder = true;
                }
            }
            // Drop newly-empty rooms.
            if let Some(room) = registry.rooms.get(freq) {
                if room.members.is_empty() && room.holder.is_none() {
                    registry.rooms.remove(freq);
                }
            }
            if let Some(room) = registry.rooms.get(freq) {
                for mid in &room.members {
                    if let Some(c) = registry.clients.get(mid) {
                        if let Some(tx) = &c.events_tx {
                            recipients.push(tx.clone());
                        }
                    }
                }
            }
        }
        KickPlan {
            client_id: id.clone(),
            display_name: client.display_name.clone(),
            frequency,
            recipients,
            was_holder,
        }
    };

    info!(
        admin_user = %admin.0,
        target_id = %plan.client_id,
        target_name = %plan.display_name,
        frequency = plan.frequency.as_deref().unwrap_or("(none)"),
        "admin kicked client",
    );

    // Off-lock broadcasts. Same shape the reaper sends so connected
    // clients can't tell admin-eviction from timeout-eviction.
    let left = Event {
        event: Some(event::Event::Left(MemberLeft {
            client_id: plan.client_id.clone(),
        })),
    };
    let release = plan.was_holder.then(|| Event {
        event: Some(event::Event::Ptt(PttEvent {
            client_id: plan.client_id.clone(),
            pressed: false,
            sequence: 0,
        })),
    });
    for tx in plan.recipients {
        let _ = tx.send(left.clone()).await;
        if let Some(ev) = &release {
            let _ = tx.send(ev.clone()).await;
        }
    }
    Ok(StatusCode::NO_CONTENT)
}

/// `POST /api/clients/:id/move`. Moves a client to a different
/// frequency. Body: `{"frequency": "446.05"}`. Mirrors the on-wire
/// `ChangeFrequency` RPC's effect.
pub async fn move_client(
    State(state): State<AppState>,
    Path(id): Path<String>,
    Extension(admin): Extension<AdminUser>,
    Json(body): Json<MoveRequest>,
) -> Result<StatusCode, (StatusCode, Json<ApiError>)> {
    let new_freq = validation::frequency(&body.frequency).map_err(|s| {
        (
            StatusCode::BAD_REQUEST,
            Json(ApiError::new(s.message().to_string())),
        )
    })?;

    // Do the registry mutation in one critical section, collect
    // the broadcast plan, release the lock, do I/O.
    let plan = {
        let mut registry = state.registry.lock().await;
        let (old_freq, client_tx, display_name) = {
            let Some(client) = registry.clients.get(&id) else {
                return Err((
                    StatusCode::NOT_FOUND,
                    Json(ApiError::new("client not found")),
                ));
            };
            (
                client.current_frequency.clone(),
                client.events_tx.clone(),
                client.display_name.clone(),
            )
        };
        if old_freq.as_deref() == Some(new_freq.as_str()) {
            // No-op move. Return 204 without churning broadcasts.
            return Ok(StatusCode::NO_CONTENT);
        }

        // Remove from the old room (if any). Reuses the signaling
        // helper to keep the leave-side semantics identical between
        // client-driven ChangeFrequency and admin-driven Move.
        let (old_recipients, old_left, old_release) = if let Some(old) = &old_freq {
            let (r, l, p, _name, _rem) = signaling::remove_from_room(&mut registry, &id, old);
            (r, Some(l), p)
        } else {
            (Vec::new(), None, None)
        };

        // Add to the new room.
        let (new_other_ids, new_holder) = {
            let room = registry.rooms.entry(new_freq.clone()).or_default();
            if !room.members.contains(&id) {
                room.members.push(id.clone());
            }
            let others: Vec<String> = room.members.iter().filter(|m| *m != &id).cloned().collect();
            (others, room.holder.clone())
        };
        if let Some(client) = registry.clients.get_mut(&id) {
            client.current_frequency = Some(new_freq.clone());
        }
        // Snapshot the new-room peers' txs while we're still locked.
        let new_recipients: Vec<mpsc::Sender<Event>> = new_other_ids
            .iter()
            .filter_map(|m| registry.clients.get(m))
            .filter_map(|c| c.events_tx.clone())
            .collect();
        let new_roster_for_backfill: Vec<(String, String)> = new_other_ids
            .iter()
            .filter_map(|m| registry.clients.get(m))
            .map(|c| (c.id.clone(), c.display_name.clone()))
            .collect();

        MovePlan {
            client_id: id.clone(),
            display_name,
            old_freq,
            new_freq,
            client_tx,
            old_recipients,
            old_left,
            old_release,
            new_recipients,
            new_holder,
            new_roster_for_backfill,
        }
    };

    info!(
        admin_user = %admin.0,
        target_id = %plan.client_id,
        target_name = %plan.display_name,
        from = plan.old_freq.as_deref().unwrap_or("(none)"),
        to = %plan.new_freq,
        "admin moved client",
    );

    // Old-room: tell the people they left behind that they're gone.
    for tx in &plan.old_recipients {
        if let Some(ev) = &plan.old_left {
            let _ = tx.send(ev.clone()).await;
        }
        if let Some(ev) = &plan.old_release {
            let _ = tx.send(ev.clone()).await;
        }
    }
    // Moved client: tell their own stream they're on a new freq,
    // then backfill the new roster + any existing PTT holder, so
    // their UI lands in the right state.
    if let Some(tx) = &plan.client_tx {
        let _ = tx
            .send(Event {
                event: Some(event::Event::FrequencyChanged(FrequencyChanged {
                    frequency: plan.new_freq.clone(),
                })),
            })
            .await;
        for (mid, mname) in &plan.new_roster_for_backfill {
            let _ = tx
                .send(Event {
                    event: Some(event::Event::Joined(MemberJoined {
                        client_id: mid.clone(),
                        display_name: mname.clone(),
                    })),
                })
                .await;
        }
        if let Some(holder_id) = &plan.new_holder {
            if holder_id != &plan.client_id {
                let _ = tx
                    .send(Event {
                        event: Some(event::Event::Ptt(PttEvent {
                            client_id: holder_id.clone(),
                            pressed: true,
                            sequence: 0,
                        })),
                    })
                    .await;
            }
        }
    }
    // New-room: announce the arrival.
    let join_event = Event {
        event: Some(event::Event::Joined(MemberJoined {
            client_id: plan.client_id.clone(),
            display_name: plan.display_name.clone(),
        })),
    };
    for tx in &plan.new_recipients {
        let _ = tx.send(join_event.clone()).await;
    }

    Ok(StatusCode::NO_CONTENT)
}

/// `POST /api/clients/:id/rename`. Mutates `client.display_name`
/// in-place and broadcasts a `DisplayNameChanged` event to:
///
///   * the renamed client's own event stream, so their topbar
///     callsign refreshes without a reconnect, and
///   * every peer in the renamed client's current room, so peer
///     rosters update in place (their `members[client_id]` map
///     gets rebound to the new name).
///
/// If the client is between Join/Leave (no current_frequency),
/// only the subject gets notified — there's no room to broadcast
/// to. That's deliberate: a lobby-only rename is rare and we don't
/// want to invent fake roster events for it.
pub async fn rename(
    State(state): State<AppState>,
    Path(id): Path<String>,
    Extension(admin): Extension<AdminUser>,
    Json(body): Json<RenameRequest>,
) -> Result<StatusCode, (StatusCode, Json<ApiError>)> {
    let new_name = validation::display_name(&body.display_name).map_err(|s| {
        (
            StatusCode::BAD_REQUEST,
            Json(ApiError::new(s.message().to_string())),
        )
    })?;

    let plan = {
        let mut registry = state.registry.lock().await;
        let Some(client) = registry.clients.get_mut(&id) else {
            return Err((
                StatusCode::NOT_FOUND,
                Json(ApiError::new("client not found")),
            ));
        };
        let old_name = std::mem::replace(&mut client.display_name, new_name.clone());
        let frequency = client.current_frequency.clone();
        // The subject themselves (so their topbar callsign updates)
        // is collected separately from peers so we send to them
        // unconditionally — even when they're not in a room.
        let self_tx = client.events_tx.clone();
        // Peers: every other member of the renamed user's current
        // room. Their UI rebinds `members[id]` to the new name.
        let peer_recipients: Vec<mpsc::Sender<Event>> = match &frequency {
            Some(freq) => registry
                .rooms
                .get(freq)
                .map(|r| r.members.clone())
                .unwrap_or_default()
                .iter()
                .filter(|m| *m != &id)
                .filter_map(|m| registry.clients.get(m))
                .filter_map(|c| c.events_tx.clone())
                .collect(),
            None => Vec::new(),
        };
        RenamePlan {
            client_id: id.clone(),
            old_name,
            new_name,
            self_tx,
            peer_recipients,
        }
    };

    info!(
        admin_user = %admin.0,
        target_id = %plan.client_id,
        old_name = %plan.old_name,
        new_name = %plan.new_name,
        "admin renamed client",
    );

    let rename_evt = Event {
        event: Some(event::Event::DisplayNameChanged(DisplayNameChanged {
            client_id: plan.client_id.clone(),
            display_name: plan.new_name.clone(),
        })),
    };
    if let Some(tx) = &plan.self_tx {
        let _ = tx.send(rename_evt.clone()).await;
    }
    for tx in plan.peer_recipients {
        let _ = tx.send(rename_evt.clone()).await;
    }
    Ok(StatusCode::NO_CONTENT)
}

// -- internal plan structs -------------------------------------------------
//
// These exist purely to move data out of the registry-locked section
// before we start awaiting on broadcasts. They're not public API.

struct KickPlan {
    client_id: String,
    display_name: String,
    frequency: Option<String>,
    recipients: Vec<mpsc::Sender<Event>>,
    was_holder: bool,
}

struct MovePlan {
    client_id: String,
    display_name: String,
    old_freq: Option<String>,
    new_freq: String,
    client_tx: Option<mpsc::Sender<Event>>,
    old_recipients: Vec<mpsc::Sender<Event>>,
    old_left: Option<Event>,
    old_release: Option<Event>,
    new_recipients: Vec<mpsc::Sender<Event>>,
    new_holder: Option<String>,
    new_roster_for_backfill: Vec<(String, String)>,
}

struct RenamePlan {
    client_id: String,
    old_name: String,
    new_name: String,
    /// The renamed client's own events_tx. Always notified so their
    /// topbar callsign refreshes — independent of whether they're
    /// currently in a room.
    self_tx: Option<mpsc::Sender<Event>>,
    /// Other members of the renamed client's current room.
    peer_recipients: Vec<mpsc::Sender<Event>>,
}

/// Map an internal `anyhow::Error` to a 500 with a safe message.
/// Used by every db-touching handler so the client never sees raw
/// sqlite errors (which can leak path / column info).
fn internal_error(err: anyhow::Error) -> (StatusCode, Json<ApiError>) {
    tracing::error!(error = ?err, "admin internal error");
    (
        StatusCode::INTERNAL_SERVER_ERROR,
        Json(ApiError::new("internal server error")),
    )
}

// COOKIE_NAME is re-exported via auth; this `use` makes the path stable
// for the (admin)tests module without each test re-deriving it.
#[allow(dead_code)]
const _COOKIE_NAME_RE_EXPORT: &str = COOKIE_NAME;
