//! gRPC-Web admin control plane.
//!
//! Implements the `toki.admin.v1.Admin` service consumed by the React
//! SPA. Served over gRPC-Web on the admin TLS listener (same origin as
//! the SPA), wired up in [`super::run`].
//!
//! # Auth
//!
//! The browser auto-attaches the HttpOnly `toki_admin_session` cookie to
//! every same-origin gRPC-Web call. [`AuthInterceptor`] runs once per RPC
//! (including the `Watch` stream open): it pulls the session token out of
//! the `cookie` metadata and stashes it in the request extensions,
//! rejecting `Unauthenticated` when it's absent. The interceptor can't be
//! async, so each handler opens with [`AdminApi::authenticated`], which
//! does the actual async `db.lookup_session` and yields the [`AdminUser`].
//!
//! Login + logout stay tiny HTTP endpoints (see [`super::handlers`]) since
//! they set/clear the cookie — gRPC can't ergonomically issue `Set-Cookie`.

use std::pin::Pin;

use tokio::sync::mpsc;
use tokio_stream::{Stream, StreamExt};
use tonic::{Request, Response, Status};

use toki_proto::admin::v1 as pb;
use toki_proto::admin::v1::admin_server::Admin;
use toki_proto::v1::{
    event, ChannelNameChanged, DisplayNameChanged, Event, FrequencyChanged, MemberJoined,
    MemberLeft, MuteChanged, PttEvent,
};

use super::auth::{self, AdminUser};
use super::watch;
use super::AppState;
use crate::audit;
use crate::server_config::ServerConfig;
use crate::{signaling, validation};

/// The session token the [`AuthInterceptor`] lifted out of the request
/// `cookie` metadata, stashed in the request extensions for the async
/// guard to validate. Newtype so it can't collide with other extensions.
#[derive(Clone)]
struct RawSessionToken(String);

/// Tonic interceptor that extracts the session cookie from request
/// metadata. Synchronous by necessity (tonic interceptors can't await),
/// so it only parses + stashes the token; the DB lookup happens in
/// [`AdminApi::authenticated`].
#[derive(Clone, Default)]
pub struct AuthInterceptor;

impl tonic::service::Interceptor for AuthInterceptor {
    fn call(&mut self, mut req: Request<()>) -> Result<Request<()>, Status> {
        let token = req
            .metadata()
            .get_all("cookie")
            .iter()
            .filter_map(|v| v.to_str().ok())
            .find_map(auth::parse_session_cookie);

        match token {
            Some(t) => {
                req.extensions_mut().insert(RawSessionToken(t));
                Ok(req)
            }
            None => Err(Status::unauthenticated("no session cookie")),
        }
    }
}

/// gRPC implementation of the admin control plane.
#[derive(Clone)]
pub struct AdminApi {
    state: AppState,
}

impl AdminApi {
    pub fn new(state: AppState) -> Self {
        Self { state }
    }

    /// Validate the session token the interceptor stashed and resolve the
    /// admin username. Every authenticated RPC calls this first.
    async fn authenticated<T>(&self, req: &Request<T>) -> Result<AdminUser, Status> {
        let token = req
            .extensions()
            .get::<RawSessionToken>()
            .ok_or_else(|| Status::unauthenticated("missing session"))?;
        let row = self
            .state
            .db
            .lookup_session(&token.0)
            .await
            .map_err(|e| {
                tracing::error!(error = ?e, "admin session lookup failed");
                Status::internal("session lookup failed")
            })?
            .ok_or_else(|| Status::unauthenticated("session expired"))?;
        Ok(AdminUser(row.username))
    }

    /// Push a fresh snapshot to all `Watch` subscribers right now, so the
    /// UI reflects a just-applied mutation without waiting for the next
    /// periodic tick. Best-effort: no subscribers → ignored.
    async fn push_snapshot(&self) {
        let snap = watch::snapshot_now(
            &self.state.registry,
            &self.state.channel_names,
            &self.state.live_rate,
            watch::next_generation(),
            self.state.started_at,
        )
        .await;
        let _ = self.state.broadcaster.send(snap);
    }

    /// Remove a live session from the registry and notify its room —
    /// the shared core of `kick_client` and `ban_client`. Returns the
    /// removed session's display name + frequency; `NOT_FOUND` when the
    /// id isn't connected. The caller does its own audit + snapshot.
    async fn remove_session(&self, id: &str) -> Result<(String, Option<String>), Status> {
        // Snapshot the work under the lock; broadcast after releasing it.
        let (display_name, frequency, recipients, was_holder) = {
            let mut registry = self.state.registry.lock().await;
            let Some(client) = registry.clients.remove(id) else {
                return Err(Status::not_found("client not found"));
            };
            registry.tokens.remove(&client.audio_token_hash);

            let mut recipients: Vec<mpsc::Sender<Event>> = Vec::new();
            let mut was_holder = false;
            let frequency = client.current_frequency.clone();
            if let Some(freq) = &frequency {
                if let Some(room) = registry.rooms.get_mut(freq) {
                    room.members.retain(|m| m != id);
                    if room.holder.as_deref() == Some(id) {
                        room.holder = None;
                        was_holder = true;
                    }
                }
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
            (
                client.display_name.clone(),
                frequency,
                recipients,
                was_holder,
            )
        };

        let left = Event {
            event: Some(event::Event::Left(MemberLeft {
                client_id: id.to_string(),
            })),
        };
        let release = was_holder.then(|| Event {
            event: Some(event::Event::Ptt(PttEvent {
                client_id: id.to_string(),
                pressed: false,
                sequence: 0,
                priority: false,
            })),
        });
        for tx in recipients {
            let _ = tx.send(left.clone()).await;
            if let Some(ev) = &release {
                let _ = tx.send(ev.clone()).await;
            }
        }
        Ok((display_name, frequency))
    }

    /// Event senders for every client currently in `frequency`'s room.
    /// Empty when the room doesn't exist (no one tuned there) — used by
    /// the channel-name RPCs to push a `ChannelNameChanged` to occupants.
    async fn room_recipients(&self, frequency: &str) -> Vec<mpsc::Sender<Event>> {
        let registry = self.state.registry.lock().await;
        registry
            .rooms
            .get(frequency)
            .map(|r| r.members.clone())
            .unwrap_or_default()
            .iter()
            .filter_map(|m| registry.clients.get(m))
            .filter_map(|c| c.events_tx.clone())
            .collect()
    }
}

/// Map the internal `ServerConfig` to the wire form, **blanking the
/// gRPC password** (never echo the cleartext to the browser) and
/// reporting only whether one is armed.
fn config_to_wire(cfg: &ServerConfig) -> pb::ServerConfig {
    pb::ServerConfig {
        server_name: cfg.server_name.clone(),
        max_peers: cfg.max_peers,
        idle_kick_secs: cfg.idle_kick_secs,
        grpc_password: String::new(),
        grpc_password_set: !cfg.grpc_password.is_empty(),
        named_channels_enabled: cfg.named_channels_enabled,
        audio_quality: cfg.audio_quality,
        require_identity: cfg.require_identity,
    }
}

type WatchStream = Pin<Box<dyn Stream<Item = Result<pb::Snapshot, Status>> + Send + 'static>>;

#[tonic::async_trait]
impl Admin for AdminApi {
    type WatchStream = WatchStream;

    async fn watch(
        &self,
        req: Request<pb::WatchRequest>,
    ) -> Result<Response<Self::WatchStream>, Status> {
        self.authenticated(&req).await?;
        // Subscribe *before* snapshotting so we can't miss an update that
        // lands between the snapshot and the subscribe.
        let rx = self.state.broadcaster.subscribe();
        let first = watch::snapshot_now(
            &self.state.registry,
            &self.state.channel_names,
            &self.state.live_rate,
            watch::next_generation(),
            self.state.started_at,
        )
        .await;
        let stream = tokio_stream::once(Ok(first)).chain(watch::broadcast_stream(rx));
        Ok(Response::new(Box::pin(stream)))
    }

    async fn get_server_info(
        &self,
        req: Request<pb::GetServerInfoRequest>,
    ) -> Result<Response<pb::ServerInfo>, Status> {
        self.authenticated(&req).await?;
        let started_at_unix = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_secs())
            .unwrap_or(0)
            .saturating_sub(self.state.started_at.elapsed().as_secs());
        Ok(Response::new(pb::ServerInfo {
            version: env!("CARGO_PKG_VERSION").to_string(),
            admin_bind: self.state.admin_bind.clone(),
            started_at_unix,
            toml_password_override: self.state.toml_password_override,
        }))
    }

    async fn get_server_config(
        &self,
        req: Request<pb::GetServerConfigRequest>,
    ) -> Result<Response<pb::ServerConfig>, Status> {
        self.authenticated(&req).await?;
        let cfg = self.state.server_config.read().await.clone();
        Ok(Response::new(config_to_wire(&cfg)))
    }

    async fn update_server_config(
        &self,
        req: Request<pb::UpdateServerConfigRequest>,
    ) -> Result<Response<pb::ServerConfig>, Status> {
        let admin = self.authenticated(&req).await?;
        let body = req.into_inner();
        let (server_name, max_peers, idle_kick_secs) =
            validate_runtime_fields(body.server_name, body.max_peers, body.idle_kick_secs)
                .map_err(Status::invalid_argument)?;
        if body.audio_quality > 3 {
            return Err(Status::invalid_argument(
                "audio_quality must be 0 (raw), 1 (low), 2 (standard) or 3 (high)",
            ));
        }

        // Merge with the live config so we don't clobber grpc_password.
        let merged = {
            let current = self.state.server_config.read().await.clone();
            ServerConfig {
                server_name,
                max_peers,
                idle_kick_secs,
                grpc_password: current.grpc_password,
                named_channels_enabled: body.named_channels_enabled,
                audio_quality: body.audio_quality,
                require_identity: body.require_identity,
            }
        };
        self.state
            .db
            .save_server_config(&merged)
            .await
            .map_err(internal)?;
        *self.state.server_config.write().await = merged.clone();

        tracing::info!(
            admin_user = %admin.0,
            server_name = %merged.server_name,
            max_peers = merged.max_peers,
            idle_kick_secs = merged.idle_kick_secs,
            named_channels_enabled = merged.named_channels_enabled,
            "admin updated server config",
        );
        audit::record(
            &self.state.audit,
            "server-config",
            &admin.0,
            "",
            &format!(
                "name='{}' max_peers={} idle_kick={}s named_channels={} audio_quality={}",
                merged.server_name,
                merged.max_peers,
                merged.idle_kick_secs,
                merged.named_channels_enabled,
                merged.audio_quality
            ),
        );
        Ok(Response::new(config_to_wire(&merged)))
    }

    async fn set_server_password(
        &self,
        req: Request<pb::SetServerPasswordRequest>,
    ) -> Result<Response<pb::SetServerPasswordResponse>, Status> {
        let admin = self.authenticated(&req).await?;
        if self.state.toml_password_override {
            return Err(Status::failed_precondition(
                "server password is managed by config.toml; remove the `password = ...` \
                 line and restart the server to manage it here instead",
            ));
        }
        let new_pw =
            validate_grpc_password(&req.get_ref().password).map_err(Status::invalid_argument)?;

        let merged = {
            let current = self.state.server_config.read().await.clone();
            ServerConfig {
                grpc_password: new_pw,
                ..current
            }
        };
        self.state
            .db
            .save_server_config(&merged)
            .await
            .map_err(internal)?;
        *self.state.server_config.write().await = merged.clone();

        tracing::info!(
            admin_user = %admin.0,
            armed = !merged.grpc_password.is_empty(),
            "admin rotated server password",
        );
        audit::record(
            &self.state.audit,
            "server-config",
            &admin.0,
            "",
            if merged.grpc_password.is_empty() {
                "disarmed the server password (open mode)"
            } else {
                "armed/rotated the server password"
            },
        );
        Ok(Response::new(pb::SetServerPasswordResponse {}))
    }

    async fn change_password(
        &self,
        req: Request<pb::ChangePasswordRequest>,
    ) -> Result<Response<pb::ChangePasswordResponse>, Status> {
        let admin = self.authenticated(&req).await?;
        // The raw token (for the "keep this session" predicate) was
        // stashed by the interceptor.
        let raw_token = req
            .extensions()
            .get::<RawSessionToken>()
            .map(|t| t.0.clone());
        let body = req.get_ref();

        // 1. Verify current (constant-time-ish: always run a verify).
        let stored = self
            .state
            .db
            .get_password_hash(&admin.0)
            .await
            .map_err(internal)?;
        let ok = match stored {
            Some(hash) => auth::verify_password(&body.current, &hash),
            None => {
                let _ = auth::verify_password(
                    &body.current,
                    "$argon2id$v=19$m=19456,t=2,p=1$YWFhYWFhYWFhYWFhYWFhYQ$AAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA",
                );
                false
            }
        };
        if !ok {
            tracing::warn!(username = %admin.0, "change-password: bad current");
            return Err(Status::unauthenticated("current password incorrect"));
        }

        // 2. Validate new.
        validate_new_password(&body.new_password).map_err(Status::invalid_argument)?;

        // 3. Hash + persist.
        let new_hash = auth::hash_password(&body.new_password).map_err(internal)?;
        self.state
            .db
            .update_password_hash(&admin.0, &new_hash)
            .await
            .map_err(internal)?;

        // 4. Kill every other session for this admin, keeping the one
        //    we're authenticated on.
        let killed = if let Some(raw) = raw_token {
            let keep_hash = super::db::hash_session_token(&raw);
            self.state
                .db
                .delete_other_sessions_for_user(&admin.0, &keep_hash)
                .await
                .map_err(internal)?
        } else {
            0
        };

        tracing::info!(
            username = %admin.0,
            other_sessions_invalidated = killed,
            "admin changed password",
        );
        audit::record(
            &self.state.audit,
            "admin-password",
            &admin.0,
            "",
            "changed the admin password",
        );
        Ok(Response::new(pb::ChangePasswordResponse {}))
    }

    async fn kick_client(
        &self,
        req: Request<pb::KickClientRequest>,
    ) -> Result<Response<pb::KickClientResponse>, Status> {
        let admin = self.authenticated(&req).await?;
        let id = req.get_ref().id.clone();

        let (display_name, frequency) = self.remove_session(&id).await?;

        tracing::info!(
            admin_user = %admin.0,
            target_id = %id,
            target_name = %display_name,
            frequency = frequency.as_deref().unwrap_or("(none)"),
            "admin kicked client",
        );
        audit::record(
            &self.state.audit,
            "kick",
            &admin.0,
            frequency.as_deref().unwrap_or(""),
            &format!("kicked {display_name}"),
        );
        self.push_snapshot().await;
        Ok(Response::new(pb::KickClientResponse {}))
    }

    async fn ban_client(
        &self,
        req: Request<pb::BanClientRequest>,
    ) -> Result<Response<pb::BanClientResponse>, Status> {
        let admin = self.authenticated(&req).await?;
        let body = req.get_ref();
        let id = body.id.clone();
        let reason = body.reason.trim().to_string();
        if reason.len() > 256 || reason.chars().any(char::is_control) {
            return Err(Status::invalid_argument(
                "reason must be ≤256 characters with no control characters",
            ));
        }

        // Resolve the target's identity before touching anything — a
        // ban needs something durable to key on. Identity-less members
        // can only be kicked.
        let (identity, display_name) = {
            let registry = self.state.registry.lock().await;
            let Some(client) = registry.clients.get(&id) else {
                return Err(Status::not_found("client not found"));
            };
            (client.identity.clone(), client.display_name.clone())
        };
        let Some(identity) = identity else {
            return Err(Status::failed_precondition(
                "member has no verified identity to ban — kick is the only available action",
            ));
        };
        if body.ban_machine && identity.machine_hash.is_empty() {
            return Err(Status::failed_precondition(
                "member reported no machine hash — ban the identity alone",
            ));
        }

        let record = crate::state::BanRecord {
            display_id: identity.display_id.clone(),
            last_callsign: display_name.clone(),
            machine_hash: if body.ban_machine {
                identity.machine_hash.clone()
            } else {
                String::new()
            },
            reason,
            banned_by: admin.0.clone(),
            banned_at: crate::admin::db::now_unix(),
        };

        // Durable first, then the live map — the ban must land even if
        // the session vanishes mid-call or the kick below races a
        // disconnect.
        self.state
            .db
            .insert_ban(&identity.pubkey_hex, &record)
            .await
            .map_err(internal)?;
        self.state
            .bans
            .write()
            .await
            .insert(identity.pubkey_hex.clone(), record.clone());

        tracing::info!(
            admin_user = %admin.0,
            target_id = %id,
            target_name = %display_name,
            identity = %identity.display_id,
            ban_machine = body.ban_machine,
            "admin banned client",
        );
        audit::record(
            &self.state.audit,
            "ban",
            &admin.0,
            "",
            &format!(
                "banned {display_name} ({}){}{}",
                identity.display_id,
                if body.ban_machine { " + machine" } else { "" },
                if record.reason.is_empty() {
                    String::new()
                } else {
                    format!(": {}", record.reason)
                },
            ),
        );

        // Kick the live session. It may legitimately be gone already
        // (disconnected between the lookup above and now) — the ban
        // stands either way.
        match self.remove_session(&id).await {
            Ok(_) => {}
            Err(s) if s.code() == tonic::Code::NotFound => {}
            Err(e) => return Err(e),
        }
        self.push_snapshot().await;
        Ok(Response::new(pb::BanClientResponse {}))
    }

    async fn list_bans(
        &self,
        req: Request<pb::ListBansRequest>,
    ) -> Result<Response<pb::ListBansResponse>, Status> {
        self.authenticated(&req).await?;
        let bans = self.state.bans.read().await;
        let mut out: Vec<pb::BanRecord> = bans
            .iter()
            .map(|(pubkey, b)| pb::BanRecord {
                pubkey: pubkey.clone(),
                display_id: b.display_id.clone(),
                last_callsign: b.last_callsign.clone(),
                machine_hash: b.machine_hash.clone(),
                reason: b.reason.clone(),
                banned_by: b.banned_by.clone(),
                banned_at_unix: b.banned_at.max(0) as u64,
            })
            .collect();
        drop(bans);
        // Newest-first, with the pubkey as a stable tiebreak so the UI
        // doesn't reshuffle rows banned in the same second.
        out.sort_by(|a, b| {
            b.banned_at_unix
                .cmp(&a.banned_at_unix)
                .then_with(|| a.pubkey.cmp(&b.pubkey))
        });
        Ok(Response::new(pb::ListBansResponse { bans: out }))
    }

    async fn lift_ban(
        &self,
        req: Request<pb::LiftBanRequest>,
    ) -> Result<Response<pb::LiftBanResponse>, Status> {
        let admin = self.authenticated(&req).await?;
        let pubkey = req.get_ref().pubkey.trim().to_ascii_lowercase();
        if pubkey.is_empty() {
            return Err(Status::invalid_argument("missing pubkey"));
        }

        self.state.db.delete_ban(&pubkey).await.map_err(internal)?;
        let removed = self.state.bans.write().await.remove(&pubkey);
        // Lifting an unknown ban is an idempotent no-op (a double-click
        // never errors); only a real lift is worth an audit line.
        if let Some(ban) = removed {
            tracing::info!(
                admin_user = %admin.0,
                identity = %ban.display_id,
                "admin lifted ban",
            );
            audit::record(
                &self.state.audit,
                "unban",
                &admin.0,
                "",
                &format!("lifted ban on {} ({})", ban.last_callsign, ban.display_id),
            );
        }
        Ok(Response::new(pb::LiftBanResponse {}))
    }

    async fn move_client(
        &self,
        req: Request<pb::MoveClientRequest>,
    ) -> Result<Response<pb::MoveClientResponse>, Status> {
        let admin = self.authenticated(&req).await?;
        let id = req.get_ref().id.clone();
        let new_freq = validation::frequency(&req.get_ref().frequency)
            .map_err(|s| Status::invalid_argument(s.message().to_string()))?;

        let plan = {
            let mut registry = self.state.registry.lock().await;
            let (old_freq, client_tx, display_name) = {
                let Some(client) = registry.clients.get(&id) else {
                    return Err(Status::not_found("client not found"));
                };
                (
                    client.current_frequency.clone(),
                    client.events_tx.clone(),
                    client.display_name.clone(),
                )
            };
            if old_freq.as_deref() == Some(new_freq.as_str()) {
                return Ok(Response::new(pb::MoveClientResponse {})); // no-op
            }

            let (old_recipients, old_left, old_release) = if let Some(old) = &old_freq {
                let (r, l, p, _name, _rem) = signaling::remove_from_room(&mut registry, &id, old);
                (r, Some(l), p)
            } else {
                (Vec::new(), None, None)
            };

            let (new_other_ids, new_holder) = {
                let room = registry.rooms.entry(new_freq.clone()).or_default();
                if !room.members.contains(&id) {
                    room.members.push(id.clone());
                }
                let others: Vec<String> =
                    room.members.iter().filter(|m| *m != &id).cloned().collect();
                (others, room.holder.clone())
            };
            if let Some(client) = registry.clients.get_mut(&id) {
                client.current_frequency = Some(new_freq.clone());
            }
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
                new_freq: new_freq.clone(),
                client_tx,
                old_recipients,
                old_left,
                old_release,
                new_recipients,
                new_holder,
                new_roster_for_backfill,
            }
        };

        tracing::info!(
            admin_user = %admin.0,
            target_id = %plan.client_id,
            target_name = %plan.display_name,
            from = plan.old_freq.as_deref().unwrap_or("(none)"),
            to = %plan.new_freq,
            "admin moved client",
        );
        audit::record(
            &self.state.audit,
            "move",
            &admin.0,
            &plan.new_freq,
            &format!(
                "moved {} from {} → {}",
                plan.display_name,
                plan.old_freq.as_deref().unwrap_or("lobby"),
                plan.new_freq
            ),
        );

        for tx in &plan.old_recipients {
            if let Some(ev) = &plan.old_left {
                let _ = tx.send(ev.clone()).await;
            }
            if let Some(ev) = &plan.old_release {
                let _ = tx.send(ev.clone()).await;
            }
        }
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
                                priority: false,
                            })),
                        })
                        .await;
                }
            }
        }
        let join_event = Event {
            event: Some(event::Event::Joined(MemberJoined {
                client_id: plan.client_id.clone(),
                display_name: plan.display_name.clone(),
            })),
        };
        for tx in &plan.new_recipients {
            let _ = tx.send(join_event.clone()).await;
        }
        self.push_snapshot().await;
        Ok(Response::new(pb::MoveClientResponse {}))
    }

    async fn rename_client(
        &self,
        req: Request<pb::RenameClientRequest>,
    ) -> Result<Response<pb::RenameClientResponse>, Status> {
        let admin = self.authenticated(&req).await?;
        let id = req.get_ref().id.clone();
        let new_name = validation::display_name(&req.get_ref().display_name)
            .map_err(|s| Status::invalid_argument(s.message().to_string()))?;

        let (old_name, self_tx, peer_recipients) = {
            let mut registry = self.state.registry.lock().await;
            let Some(client) = registry.clients.get_mut(&id) else {
                return Err(Status::not_found("client not found"));
            };
            let old_name = std::mem::replace(&mut client.display_name, new_name.clone());
            let frequency = client.current_frequency.clone();
            let self_tx = client.events_tx.clone();
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
            (old_name, self_tx, peer_recipients)
        };

        tracing::info!(
            admin_user = %admin.0,
            target_id = %id,
            old_name = %old_name,
            new_name = %new_name,
            "admin renamed client",
        );
        audit::record(
            &self.state.audit,
            "rename",
            &admin.0,
            "",
            &format!("renamed {old_name} → {new_name}"),
        );

        let rename_evt = Event {
            event: Some(event::Event::DisplayNameChanged(DisplayNameChanged {
                client_id: id.clone(),
                display_name: new_name.clone(),
            })),
        };
        if let Some(tx) = &self_tx {
            let _ = tx.send(rename_evt.clone()).await;
        }
        for tx in peer_recipients {
            let _ = tx.send(rename_evt.clone()).await;
        }
        self.push_snapshot().await;
        Ok(Response::new(pb::RenameClientResponse {}))
    }

    async fn set_priority(
        &self,
        req: Request<pb::SetPriorityRequest>,
    ) -> Result<Response<pb::SetPriorityResponse>, Status> {
        let admin = self.authenticated(&req).await?;
        let pb::SetPriorityRequest { id, grant } = req.into_inner();

        let bound_freq = {
            let mut registry = self.state.registry.lock().await;
            let Some(client) = registry.clients.get_mut(&id) else {
                return Err(Status::not_found("client not found"));
            };
            if grant {
                let Some(freq) = client.current_frequency.clone() else {
                    return Err(Status::failed_precondition(
                        "client is not on a channel; priority is per-channel",
                    ));
                };
                client.priority_freq = Some(freq.clone());
                Some(freq)
            } else {
                client.priority_freq = None;
                None
            }
        };

        match &bound_freq {
            Some(freq) => tracing::info!(
                admin_user = %admin.0, target_id = %id, frequency = %freq,
                "admin granted priority",
            ),
            None => tracing::info!(
                admin_user = %admin.0, target_id = %id, "admin revoked priority",
            ),
        }
        audit::record(
            &self.state.audit,
            "priority",
            &admin.0,
            bound_freq.as_deref().unwrap_or(""),
            if bound_freq.is_some() {
                "granted priority"
            } else {
                "revoked priority"
            },
        );
        self.push_snapshot().await;
        Ok(Response::new(pb::SetPriorityResponse {}))
    }

    async fn set_mute(
        &self,
        req: Request<pb::SetMuteRequest>,
    ) -> Result<Response<pb::SetMuteResponse>, Status> {
        let admin = self.authenticated(&req).await?;
        let pb::SetMuteRequest { id, muted } = req.into_inner();

        // Apply the flag and, if we're muting the current floor-holder,
        // drop the floor right here so the channel isn't left stuck on a
        // now-silenced talker. Collect the room recipients under the lock
        // for the post-release broadcast. `display_name` + `frequency`
        // feed the audit line; `was_holder` decides the PttUp broadcast.
        let (display_name, frequency, recipients, was_holder, changed) = {
            let mut registry = self.state.registry.lock().await;
            let Some(client) = registry.clients.get_mut(&id) else {
                return Err(Status::not_found("client not found"));
            };
            let changed = client.muted != muted;
            client.muted = muted;
            let display_name = client.display_name.clone();
            let frequency = client.current_frequency.clone();

            let mut was_holder = false;
            let mut recipients: Vec<mpsc::Sender<Event>> = Vec::new();
            if let Some(freq) = &frequency {
                if muted {
                    if let Some(room) = registry.rooms.get_mut(freq) {
                        if room.holder.as_deref() == Some(id.as_str()) {
                            // Floor held by the now-muted member: release
                            // it and stamp the grace window so their final
                            // in-flight UDP frames still flush cleanly
                            // (mirrors a normal PttUp).
                            room.holder = None;
                            room.last_released = Some((id.clone(), std::time::Instant::now()));
                            was_holder = true;
                        }
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
            (display_name, frequency, recipients, was_holder, changed)
        };

        // No-op when the state already matched (idempotent: a
        // double-click never double-logs or re-broadcasts).
        if !changed {
            return Ok(Response::new(pb::SetMuteResponse {}));
        }

        let mute_evt = Event {
            event: Some(event::Event::MuteChanged(MuteChanged {
                client_id: id.clone(),
                muted,
            })),
        };
        // If muting dropped the floor, tell the room the holder released
        // so rosters clear the talking indicator immediately.
        let release = was_holder.then(|| Event {
            event: Some(event::Event::Ptt(PttEvent {
                client_id: id.clone(),
                pressed: false,
                sequence: 0,
                priority: false,
            })),
        });
        for tx in recipients {
            let _ = tx.send(mute_evt.clone()).await;
            if let Some(ev) = &release {
                let _ = tx.send(ev.clone()).await;
            }
        }

        tracing::info!(
            admin_user = %admin.0,
            target_id = %id,
            target_name = %display_name,
            muted,
            "admin set mute",
        );
        audit::record(
            &self.state.audit,
            "mute",
            &admin.0,
            frequency.as_deref().unwrap_or(""),
            &format!("{} {display_name}", if muted { "muted" } else { "unmuted" }),
        );
        self.push_snapshot().await;
        Ok(Response::new(pb::SetMuteResponse {}))
    }

    async fn set_channel_name(
        &self,
        req: Request<pb::SetChannelNameRequest>,
    ) -> Result<Response<pb::SetChannelNameResponse>, Status> {
        let admin = self.authenticated(&req).await?;
        if !self.state.server_config.read().await.named_channels_enabled {
            return Err(Status::failed_precondition(
                "named channels are disabled; enable them in server settings first",
            ));
        }
        let pb::SetChannelNameRequest { frequency, name } = req.into_inner();
        let frequency = validation::frequency(&frequency)
            .map_err(|s| Status::invalid_argument(s.message().to_string()))?;
        let name = validate_channel_name(&name).map_err(Status::invalid_argument)?;

        // Persist + update the shared map in lockstep. Empty name clears
        // it (delete the row); otherwise upsert.
        if name.is_empty() {
            self.state
                .db
                .clear_channel_name(&frequency)
                .await
                .map_err(internal)?;
            self.state.channel_names.write().await.remove(&frequency);
        } else {
            self.state
                .db
                .set_channel_name(&frequency, &name)
                .await
                .map_err(internal)?;
            self.state
                .channel_names
                .write()
                .await
                .insert(frequency.clone(), name.clone());
        }

        tracing::info!(
            admin_user = %admin.0,
            frequency = %frequency,
            cleared = name.is_empty(),
            "admin set channel name",
        );
        audit::record(
            &self.state.audit,
            "channel-name",
            &admin.0,
            &frequency,
            &if name.is_empty() {
                "cleared the channel name".to_string()
            } else {
                format!("named the channel '{name}'")
            },
        );

        // Tell anyone currently on that frequency about the new label.
        let evt = Event {
            event: Some(event::Event::ChannelNameChanged(ChannelNameChanged {
                frequency: frequency.clone(),
                name,
            })),
        };
        for tx in self.room_recipients(&frequency).await {
            let _ = tx.send(evt.clone()).await;
        }
        self.push_snapshot().await;
        Ok(Response::new(pb::SetChannelNameResponse {}))
    }

    async fn clear_all_channel_names(
        &self,
        req: Request<pb::ClearAllChannelNamesRequest>,
    ) -> Result<Response<pb::ClearAllChannelNamesResponse>, Status> {
        let admin = self.authenticated(&req).await?;
        if !self.state.server_config.read().await.named_channels_enabled {
            return Err(Status::failed_precondition(
                "named channels are disabled; enable them in server settings first",
            ));
        }

        // Capture the set of previously-named frequencies (so we can tell
        // their occupants the name is gone), then clear table + map.
        let cleared_freqs: Vec<String> = {
            let mut map = self.state.channel_names.write().await;
            let freqs = map.keys().cloned().collect();
            map.clear();
            freqs
        };
        self.state
            .db
            .clear_all_channel_names()
            .await
            .map_err(internal)?;

        tracing::info!(
            admin_user = %admin.0,
            count = cleared_freqs.len(),
            "admin cleared all channel names",
        );

        // Broadcast an empty-name event to each cleared frequency's room.
        for freq in &cleared_freqs {
            let evt = Event {
                event: Some(event::Event::ChannelNameChanged(ChannelNameChanged {
                    frequency: freq.clone(),
                    name: String::new(),
                })),
            };
            for tx in self.room_recipients(freq).await {
                let _ = tx.send(evt.clone()).await;
            }
        }
        audit::record(
            &self.state.audit,
            "channel-clear",
            &admin.0,
            "",
            &format!("cleared {} channel name(s)", cleared_freqs.len()),
        );
        self.push_snapshot().await;
        Ok(Response::new(pb::ClearAllChannelNamesResponse {}))
    }

    async fn get_metrics(
        &self,
        req: Request<pb::MetricsRequest>,
    ) -> Result<Response<pb::MetricsResponse>, Status> {
        self.authenticated(&req).await?;
        let window_secs: i64 = match req.into_inner().window() {
            pb::MetricsWindow::Hour => 3600,
            pb::MetricsWindow::Day => 24 * 3600,
            pb::MetricsWindow::Week => 7 * 24 * 3600,
        };
        let since = super::db::now_unix() - window_secs;
        let rows = self.state.db.load_metrics(since).await.map_err(internal)?;
        let samples = downsample(rows, 150)
            .into_iter()
            .map(|r| pb::MetricSample {
                ts_unix: r.ts as u64,
                rx_bytes_per_sec: r.rx_bps,
                tx_bytes_per_sec: r.tx_bps,
                users: r.users,
                transmitting: r.transmitting,
            })
            .collect();
        Ok(Response::new(pb::MetricsResponse { samples }))
    }

    async fn get_server_health(
        &self,
        req: Request<pb::GetServerHealthRequest>,
    ) -> Result<Response<pb::ServerHealth>, Status> {
        self.authenticated(&req).await?;
        let h = crate::metrics::health_snapshot(&self.state.health);
        Ok(Response::new(pb::ServerHealth {
            cpu_percent: h.cpu_percent as f64,
            mem_used_bytes: h.mem_used,
            mem_total_bytes: h.mem_total,
            disk_used_bytes: h.disk_used,
            disk_total_bytes: h.disk_total,
        }))
    }

    async fn get_audit_log(
        &self,
        req: Request<pb::AuditLogRequest>,
    ) -> Result<Response<pb::AuditLogResponse>, Status> {
        self.authenticated(&req).await?;
        let r = req.into_inner();
        let kinds: &[&str] = match r.filter() {
            pb::AuditFilter::All => &[],
            pb::AuditFilter::Admin => audit::KINDS_ADMIN,
            pb::AuditFilter::Connections => audit::KINDS_CONNECTIONS,
            pb::AuditFilter::Security => audit::KINDS_SECURITY,
        };
        let limit = if r.limit == 0 { 100 } else { r.limit };
        let (rows, total) = self
            .state
            .db
            .load_audit(kinds, limit, r.before_id)
            .await
            .map_err(internal)?;
        let entries = rows
            .into_iter()
            .map(|a| pb::AuditEntry {
                id: a.id,
                ts_unix: a.ts as u64,
                kind: a.kind,
                actor: a.actor,
                frequency: a.frequency,
                detail: a.detail,
            })
            .collect();
        Ok(Response::new(pb::AuditLogResponse { entries, total }))
    }
}

/// Internal data carried out of the registry-locked section of
/// `move_client` before awaiting broadcasts.
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

fn internal<E: std::fmt::Debug>(e: E) -> Status {
    tracing::error!(error = ?e, "admin gRPC internal error");
    Status::internal("internal error")
}

/// Reduce a time-series (oldest→newest) to at most `max` points by
/// averaging consecutive equal-width buckets. Preserves the overall
/// shape while bounding the gRPC payload regardless of window length.
fn downsample(rows: Vec<super::db::MetricRow>, max: usize) -> Vec<super::db::MetricRow> {
    if max == 0 || rows.len() <= max {
        return rows;
    }
    let bucket = rows.len().div_ceil(max);
    rows.chunks(bucket)
        .map(|chunk| {
            let n = chunk.len() as u64;
            super::db::MetricRow {
                // Midpoint timestamp reads naturally on the x-axis.
                ts: chunk[chunk.len() / 2].ts,
                rx_bps: chunk.iter().map(|r| r.rx_bps).sum::<u64>() / n,
                tx_bps: chunk.iter().map(|r| r.tx_bps).sum::<u64>() / n,
                users: (chunk.iter().map(|r| r.users as u64).sum::<u64>() / n) as u32,
                transmitting: (chunk.iter().map(|r| r.transmitting as u64).sum::<u64>() / n) as u32,
            }
        })
        .collect()
}

fn validate_runtime_fields(
    name: String,
    max_peers: u32,
    idle_kick_secs: u32,
) -> Result<(String, u32, u32), String> {
    let server_name = name.trim().to_string();
    if server_name.len() > 64 {
        return Err("server_name exceeds 64 bytes".into());
    }
    if server_name.bytes().any(|b| b < 0x20 || b == 0x7f) {
        return Err("server_name contains control characters".into());
    }
    if max_peers == 0 || max_peers > 100_000 {
        return Err("max_peers must be between 1 and 100000".into());
    }
    if !(5..=86_400).contains(&idle_kick_secs) {
        return Err("idle_kick_secs must be between 5 and 86400".into());
    }
    Ok((server_name, max_peers, idle_kick_secs))
}

fn validate_grpc_password(raw: &str) -> Result<String, String> {
    let trimmed = raw.trim().to_string();
    if trimmed.len() > 128 {
        return Err("server password must be at most 128 characters".into());
    }
    if trimmed.bytes().any(|b| b < 0x20 || b == 0x7f) {
        return Err("server password contains control characters".into());
    }
    Ok(trimmed)
}

/// Validate an admin-supplied channel name. Trims surrounding
/// whitespace; an empty result is allowed and means "clear the name".
/// Caps at 16 *characters* (not bytes) to match the contract the client
/// renders against, and rejects control characters.
fn validate_channel_name(raw: &str) -> Result<String, String> {
    let trimmed = raw.trim().to_string();
    if trimmed.chars().count() > 16 {
        return Err("channel name must be at most 16 characters".into());
    }
    if trimmed.chars().any(|c| c.is_control()) {
        return Err("channel name contains control characters".into());
    }
    Ok(trimmed)
}

fn validate_new_password(pw: &str) -> Result<(), String> {
    if pw.len() < 8 {
        return Err("new password must be at least 8 characters".into());
    }
    if pw.len() > 128 {
        return Err("new password must be at most 128 characters".into());
    }
    if pw.bytes().any(|b| b < 0x20 || b == 0x7f) {
        return Err("new password contains control characters".into());
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::admin::db::AdminDb;
    use crate::server_config;
    use crate::state::{Client, Registry, Room, SharedRegistry, TOKEN_HASH_LEN};
    use crate::throttle::IpThrottle;
    use std::sync::Arc;
    use std::time::{Duration, Instant};
    use tokio::sync::Mutex;
    use tonic::Code;

    async fn test_api(toml_override: bool) -> (AdminApi, String) {
        let db = AdminDb::open_in_memory().await.unwrap();
        db.migrate().await.unwrap();
        db.insert_user("admin", &auth::hash_password("hunter2").unwrap())
            .await
            .unwrap();
        let token = auth::generate_session_token();
        let expires = super::super::db::now_unix() + 3600;
        db.create_session(&token, "admin", expires).await.unwrap();

        let (tx, _) = tokio::sync::broadcast::channel(8);
        let state = AppState {
            registry: Arc::new(Mutex::new(Registry::default())),
            db,
            broadcaster: tx,
            session_ttl: Duration::from_secs(3600),
            started_at: Instant::now(),
            admin_bind: "127.0.0.1:0".into(),
            login_throttle: Arc::new(IpThrottle::new()),
            server_config: server_config::shared_default(),
            channel_names: crate::state::shared_channel_names(Default::default()),
            bans: crate::state::shared_bans(Default::default()),
            health: crate::metrics::shared_health(),
            live_rate: crate::metrics::shared_live_rate(),
            audit: crate::audit::channel().0,
            toml_password_override: toml_override,
        };
        (AdminApi::new(state), token)
    }

    /// Variant that flips the named-channels feature on (and optionally
    /// seeds a stored name) so the channel-name RPC tests don't trip the
    /// FAILED_PRECONDITION guard.
    async fn test_api_named() -> (AdminApi, String) {
        let (api, token) = test_api(false).await;
        api.state.server_config.write().await.named_channels_enabled = true;
        (api, token)
    }

    /// Build an authenticated request: inject the session token the way
    /// the interceptor would.
    fn authed<T>(msg: T, token: &str) -> Request<T> {
        let mut req = Request::new(msg);
        req.extensions_mut()
            .insert(RawSessionToken(token.to_string()));
        req
    }

    fn mk_client(id: &str, name: &str, freq: Option<&str>) -> Client {
        Client {
            id: id.to_string(),
            identity: None,
            display_name: name.to_string(),
            audio_token_hash: [0u8; TOKEN_HASH_LEN],
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
            muted: false,
        }
    }

    async fn seed(reg: &SharedRegistry, id: &str, freq: Option<&str>) {
        let mut r = reg.lock().await;
        r.clients.insert(id.into(), mk_client(id, id, freq));
        if let Some(f) = freq {
            r.rooms
                .entry(f.into())
                .or_insert_with(Room::default)
                .members
                .push(id.into());
        }
    }

    #[tokio::test]
    async fn unauthenticated_without_token() {
        let (api, _t) = test_api(false).await;
        // No RawSessionToken in extensions → Unauthenticated.
        let err = api
            .get_server_info(Request::new(pb::GetServerInfoRequest {}))
            .await
            .unwrap_err();
        assert_eq!(err.code(), Code::Unauthenticated);
    }

    #[tokio::test]
    async fn get_server_info_ok() {
        let (api, token) = test_api(false).await;
        let info = api
            .get_server_info(authed(pb::GetServerInfoRequest {}, &token))
            .await
            .unwrap()
            .into_inner();
        assert_eq!(info.version, env!("CARGO_PKG_VERSION"));
        assert!(!info.toml_password_override);
    }

    async fn seed_with_identity(reg: &SharedRegistry, id: &str, machine_hash: &str) {
        let mut r = reg.lock().await;
        let mut c = mk_client(id, id, None);
        c.identity = Some(crate::state::ClientIdentity {
            display_id: "FLNIHQMB".into(),
            pubkey_hex: format!("{id:0<64}"),
            machine_hash: machine_hash.into(),
            first_seen: 1,
        });
        r.clients.insert(id.into(), c);
    }

    #[tokio::test]
    async fn ban_unknown_is_not_found() {
        let (api, token) = test_api(false).await;
        let err = api
            .ban_client(authed(
                pb::BanClientRequest {
                    id: "ghost".into(),
                    reason: "x".into(),
                    ban_machine: false,
                },
                &token,
            ))
            .await
            .unwrap_err();
        assert_eq!(err.code(), Code::NotFound);
    }

    #[tokio::test]
    async fn ban_identityless_member_is_failed_precondition() {
        let (api, token) = test_api(false).await;
        seed(&api.state.registry, "anon", None).await; // mk_client → identity: None
        let err = api
            .ban_client(authed(
                pb::BanClientRequest {
                    id: "anon".into(),
                    reason: String::new(),
                    ban_machine: false,
                },
                &token,
            ))
            .await
            .unwrap_err();
        assert_eq!(err.code(), Code::FailedPrecondition);
    }

    #[tokio::test]
    async fn ban_machine_without_machine_hash_is_failed_precondition() {
        let (api, token) = test_api(false).await;
        seed_with_identity(&api.state.registry, "alice", "").await;
        let err = api
            .ban_client(authed(
                pb::BanClientRequest {
                    id: "alice".into(),
                    reason: String::new(),
                    ban_machine: true,
                },
                &token,
            ))
            .await
            .unwrap_err();
        assert_eq!(err.code(), Code::FailedPrecondition);
        // The identity ban must not have landed either (all-or-nothing).
        assert!(api.state.bans.read().await.is_empty());
    }

    #[tokio::test]
    async fn ban_records_kicks_lists_and_lifts() {
        let (api, token) = test_api(false).await;
        let machine = "ab".repeat(32);
        seed_with_identity(&api.state.registry, "alice", &machine).await;
        let pubkey = format!("{:0<64}", "alice");

        api.ban_client(authed(
            pb::BanClientRequest {
                id: "alice".into(),
                reason: "spam".into(),
                ban_machine: true,
            },
            &token,
        ))
        .await
        .unwrap();

        // Session kicked, ban in the live map AND durable in the db.
        assert!(!api
            .state
            .registry
            .lock()
            .await
            .clients
            .contains_key("alice"));
        let live = api.state.bans.read().await.get(&pubkey).cloned().unwrap();
        assert_eq!(live.reason, "spam");
        assert_eq!(live.machine_hash, machine);
        assert_eq!(live.banned_by, "admin");
        assert!(api
            .state
            .db
            .load_bans()
            .await
            .unwrap()
            .contains_key(&pubkey));

        // Listed, newest-first shape.
        let bans = api
            .list_bans(authed(pb::ListBansRequest {}, &token))
            .await
            .unwrap()
            .into_inner()
            .bans;
        assert_eq!(bans.len(), 1);
        assert_eq!(bans[0].pubkey, pubkey);
        assert_eq!(bans[0].last_callsign, "alice");

        // Lift removes from both stores; a second lift is a no-op.
        api.lift_ban(authed(
            pb::LiftBanRequest {
                pubkey: pubkey.clone(),
            },
            &token,
        ))
        .await
        .unwrap();
        assert!(api.state.bans.read().await.is_empty());
        assert!(api.state.db.load_bans().await.unwrap().is_empty());
        api.lift_ban(authed(pb::LiftBanRequest { pubkey }, &token))
            .await
            .unwrap();
    }

    #[tokio::test]
    async fn ban_rejects_oversized_or_control_reason() {
        let (api, token) = test_api(false).await;
        seed_with_identity(&api.state.registry, "alice", "").await;
        for bad in ["x".repeat(257), "evil\nreason".to_string()] {
            let err = api
                .ban_client(authed(
                    pb::BanClientRequest {
                        id: "alice".into(),
                        reason: bad,
                        ban_machine: false,
                    },
                    &token,
                ))
                .await
                .unwrap_err();
            assert_eq!(err.code(), Code::InvalidArgument);
        }
    }

    #[tokio::test]
    async fn get_server_config_blanks_password() {
        let (api, token) = test_api(false).await;
        // Arm a password first.
        api.set_server_password(authed(
            pb::SetServerPasswordRequest {
                password: "s3cret!!".into(),
            },
            &token,
        ))
        .await
        .unwrap();
        let cfg = api
            .get_server_config(authed(pb::GetServerConfigRequest {}, &token))
            .await
            .unwrap()
            .into_inner();
        assert_eq!(cfg.grpc_password, ""); // never echoed
        assert!(cfg.grpc_password_set); // but reported as armed
    }

    #[tokio::test]
    async fn set_server_password_locked_by_toml() {
        let (api, token) = test_api(true).await;
        let err = api
            .set_server_password(authed(
                pb::SetServerPasswordRequest {
                    password: "x".into(),
                },
                &token,
            ))
            .await
            .unwrap_err();
        assert_eq!(err.code(), Code::FailedPrecondition);
    }

    #[tokio::test]
    async fn update_server_config_rejects_bad_max_peers() {
        let (api, token) = test_api(false).await;
        let err = api
            .update_server_config(authed(
                pb::UpdateServerConfigRequest {
                    server_name: "ok".into(),
                    max_peers: 0,
                    idle_kick_secs: 10,
                    named_channels_enabled: false,
                    audio_quality: 2,
                    require_identity: false,
                },
                &token,
            ))
            .await
            .unwrap_err();
        assert_eq!(err.code(), Code::InvalidArgument);
    }

    #[tokio::test]
    async fn kick_unknown_is_not_found() {
        let (api, token) = test_api(false).await;
        let err = api
            .kick_client(authed(pb::KickClientRequest { id: "ghost".into() }, &token))
            .await
            .unwrap_err();
        assert_eq!(err.code(), Code::NotFound);
    }

    #[tokio::test]
    async fn kick_removes_client_from_registry() {
        let (api, token) = test_api(false).await;
        seed(&api.state.registry, "alice", Some("446.05")).await;
        api.kick_client(authed(pb::KickClientRequest { id: "alice".into() }, &token))
            .await
            .unwrap();
        assert!(!api
            .state
            .registry
            .lock()
            .await
            .clients
            .contains_key("alice"));
    }

    #[tokio::test]
    async fn priority_on_lobby_member_is_failed_precondition() {
        let (api, token) = test_api(false).await;
        seed(&api.state.registry, "bob", None).await; // lobby, no channel
        let err = api
            .set_priority(authed(
                pb::SetPriorityRequest {
                    id: "bob".into(),
                    grant: true,
                },
                &token,
            ))
            .await
            .unwrap_err();
        assert_eq!(err.code(), Code::FailedPrecondition);
    }

    #[tokio::test]
    async fn priority_grant_then_revoke() {
        let (api, token) = test_api(false).await;
        seed(&api.state.registry, "cara", Some("447.00")).await;
        api.set_priority(authed(
            pb::SetPriorityRequest {
                id: "cara".into(),
                grant: true,
            },
            &token,
        ))
        .await
        .unwrap();
        assert_eq!(
            api.state.registry.lock().await.clients["cara"]
                .priority_freq
                .as_deref(),
            Some("447.00")
        );
        api.set_priority(authed(
            pb::SetPriorityRequest {
                id: "cara".into(),
                grant: false,
            },
            &token,
        ))
        .await
        .unwrap();
        assert!(api.state.registry.lock().await.clients["cara"]
            .priority_freq
            .is_none());
    }

    #[tokio::test]
    async fn mute_unknown_is_not_found() {
        let (api, token) = test_api(false).await;
        let err = api
            .set_mute(authed(
                pb::SetMuteRequest {
                    id: "ghost".into(),
                    muted: true,
                },
                &token,
            ))
            .await
            .unwrap_err();
        assert_eq!(err.code(), Code::NotFound);
    }

    #[tokio::test]
    async fn mute_sets_and_clears_flag() {
        let (api, token) = test_api(false).await;
        seed(&api.state.registry, "dora", Some("447.00")).await;
        api.set_mute(authed(
            pb::SetMuteRequest {
                id: "dora".into(),
                muted: true,
            },
            &token,
        ))
        .await
        .unwrap();
        assert!(api.state.registry.lock().await.clients["dora"].muted);
        assert!(!api.state.registry.lock().await.clients["dora"].can_speak());

        api.set_mute(authed(
            pb::SetMuteRequest {
                id: "dora".into(),
                muted: false,
            },
            &token,
        ))
        .await
        .unwrap();
        assert!(!api.state.registry.lock().await.clients["dora"].muted);
        assert!(api.state.registry.lock().await.clients["dora"].can_speak());
    }

    #[tokio::test]
    async fn muting_holder_drops_the_floor() {
        let (api, token) = test_api(false).await;
        seed(&api.state.registry, "evan", Some("447.00")).await;
        // Evan holds the floor.
        api.state
            .registry
            .lock()
            .await
            .rooms
            .get_mut("447.00")
            .unwrap()
            .holder = Some("evan".into());

        api.set_mute(authed(
            pb::SetMuteRequest {
                id: "evan".into(),
                muted: true,
            },
            &token,
        ))
        .await
        .unwrap();

        let reg = api.state.registry.lock().await;
        let room = &reg.rooms["447.00"];
        // Floor freed, and the release-grace recorded so Evan's
        // in-flight tail can still flush.
        assert!(room.holder.is_none());
        assert!(matches!(&room.last_released, Some((id, _)) if id == "evan"));
    }

    #[tokio::test]
    async fn set_channel_name_disabled_is_failed_precondition() {
        let (api, token) = test_api(false).await; // feature off
        let err = api
            .set_channel_name(authed(
                pb::SetChannelNameRequest {
                    frequency: "446.05".into(),
                    name: "Ops".into(),
                },
                &token,
            ))
            .await
            .unwrap_err();
        assert_eq!(err.code(), Code::FailedPrecondition);
    }

    #[tokio::test]
    async fn set_channel_name_rejects_overlong() {
        let (api, token) = test_api_named().await;
        let err = api
            .set_channel_name(authed(
                pb::SetChannelNameRequest {
                    frequency: "446.05".into(),
                    name: "x".repeat(17),
                },
                &token,
            ))
            .await
            .unwrap_err();
        assert_eq!(err.code(), Code::InvalidArgument);
    }

    #[tokio::test]
    async fn set_channel_name_rejects_bad_frequency() {
        let (api, token) = test_api_named().await;
        let err = api
            .set_channel_name(authed(
                pb::SetChannelNameRequest {
                    frequency: "999.99".into(),
                    name: "Nope".into(),
                },
                &token,
            ))
            .await
            .unwrap_err();
        assert_eq!(err.code(), Code::InvalidArgument);
    }

    #[tokio::test]
    async fn set_then_clear_channel_name_roundtrips_map_and_db() {
        let (api, token) = test_api_named().await;
        // Set.
        api.set_channel_name(authed(
            pb::SetChannelNameRequest {
                frequency: "446.05".into(),
                name: "  Ops Net  ".into(), // trimmed server-side
            },
            &token,
        ))
        .await
        .unwrap();
        assert_eq!(
            api.state
                .channel_names
                .read()
                .await
                .get("446.05")
                .map(String::as_str),
            Some("Ops Net")
        );
        assert_eq!(
            api.state
                .db
                .load_channel_names()
                .await
                .unwrap()
                .get("446.05")
                .map(String::as_str),
            Some("Ops Net")
        );
        // Clear via empty name.
        api.set_channel_name(authed(
            pb::SetChannelNameRequest {
                frequency: "446.05".into(),
                name: "".into(),
            },
            &token,
        ))
        .await
        .unwrap();
        assert!(!api.state.channel_names.read().await.contains_key("446.05"));
        assert!(api.state.db.load_channel_names().await.unwrap().is_empty());
    }

    #[tokio::test]
    async fn clear_all_channel_names_wipes_everything() {
        let (api, token) = test_api_named().await;
        for (f, n) in [("446.05", "A"), ("447.00", "B")] {
            api.set_channel_name(authed(
                pb::SetChannelNameRequest {
                    frequency: f.into(),
                    name: n.into(),
                },
                &token,
            ))
            .await
            .unwrap();
        }
        api.clear_all_channel_names(authed(pb::ClearAllChannelNamesRequest {}, &token))
            .await
            .unwrap();
        assert!(api.state.channel_names.read().await.is_empty());
        assert!(api.state.db.load_channel_names().await.unwrap().is_empty());
    }

    #[test]
    fn downsample_passes_through_when_under_cap() {
        let rows: Vec<super::super::db::MetricRow> = (0..10)
            .map(|i| super::super::db::MetricRow {
                ts: i,
                rx_bps: i as u64,
                tx_bps: 0,
                users: 1,
                transmitting: 0,
            })
            .collect();
        assert_eq!(downsample(rows.clone(), 150).len(), 10);
    }

    #[test]
    fn downsample_caps_and_averages() {
        let rows: Vec<super::super::db::MetricRow> = (0..1000)
            .map(|i| super::super::db::MetricRow {
                ts: i,
                rx_bps: 100,
                tx_bps: 50,
                users: 4,
                transmitting: 1,
            })
            .collect();
        let out = downsample(rows, 150);
        assert!(out.len() <= 150, "got {}", out.len());
        // Constant series → averages preserve the value exactly.
        assert!(out
            .iter()
            .all(|r| r.rx_bps == 100 && r.tx_bps == 50 && r.users == 4));
    }

    #[tokio::test]
    async fn audit_log_filters_by_category() {
        let (api, token) = test_api(false).await;
        let db = &api.state.db;
        db.insert_audit(1, "kick", "admin", "446.05", "x")
            .await
            .unwrap();
        db.insert_audit(2, "connect", "ALPHA-1", "", "from 1.2.3.4")
            .await
            .unwrap();
        db.insert_audit(3, "auth-fail", "SYSTEM", "", "bad pw")
            .await
            .unwrap();
        db.insert_audit(4, "rename", "admin", "", "A → B")
            .await
            .unwrap();

        let all = api
            .get_audit_log(authed(
                pb::AuditLogRequest {
                    filter: pb::AuditFilter::All as i32,
                    limit: 50,
                    before_id: 0,
                },
                &token,
            ))
            .await
            .unwrap()
            .into_inner();
        assert_eq!(all.total, 4);
        // Newest-first ordering.
        assert_eq!(all.entries.first().unwrap().kind, "rename");

        let admin_only = api
            .get_audit_log(authed(
                pb::AuditLogRequest {
                    filter: pb::AuditFilter::Admin as i32,
                    limit: 50,
                    before_id: 0,
                },
                &token,
            ))
            .await
            .unwrap()
            .into_inner();
        assert_eq!(admin_only.total, 2); // kick + rename

        let security = api
            .get_audit_log(authed(
                pb::AuditLogRequest {
                    filter: pb::AuditFilter::Security as i32,
                    limit: 50,
                    before_id: 0,
                },
                &token,
            ))
            .await
            .unwrap()
            .into_inner();
        assert_eq!(security.total, 1);
        assert_eq!(security.entries[0].kind, "auth-fail");
    }
}
