use std::pin::Pin;

use tokio::sync::mpsc;
use tokio_stream::{wrappers::ReceiverStream, Stream, StreamExt};
use tonic::{Request, Response, Status, Streaming};
use tracing::info;
use uuid::Uuid;

use toki_proto::v1::{
    event,
    signaling_server::{Signaling, SignalingServer},
    ChangeFrequencyRequest, ChangeFrequencyResponse, Event, FrequencyChanged, JoinRequest,
    LeaveRequest, LeaveResponse, MemberJoined, MemberLeft, PttAck, PttEvent, RegisterRequest,
    RegisterResponse,
};

use crate::server_config::SharedServerConfig;
use crate::state::{hash_token, Client, Registry, SharedRegistry};
use crate::throttle::{IpThrottle, ThrottleReject};
use crate::validation;

pub struct SignalingSvc {
    registry: SharedRegistry,
    audio_endpoint: String,
    /// Bootstrap password from `config.toml`. When `Some`, it takes
    /// precedence over the DB-stored `server_config.grpc_password`
    /// — operators who set it in TOML have explicitly opted out of
    /// runtime rotation via the admin panel. The admin UI knows
    /// this via `ServerInfo.toml_password_override` and disables
    /// its input accordingly.
    toml_password: Option<String>,
    /// Per-source-IP rate cap and auth-failure backoff. Gates the
    /// `register` RPC; other RPCs are protected indirectly because
    /// they require a `client_id` minted by a successful register.
    throttle: IpThrottle,
    /// Live handle on the runtime-mutable server settings. Read on
    /// every Register call to honor the operator's current
    /// `max_peers` ceiling and `grpc_password` (when no TOML
    /// override) without requiring a restart on change.
    server_config: SharedServerConfig,
}

impl SignalingSvc {
    pub fn new(
        registry: SharedRegistry,
        audio_endpoint: String,
        toml_password: Option<String>,
        server_config: SharedServerConfig,
    ) -> SignalingServer<Self> {
        SignalingServer::new(Self {
            registry,
            audio_endpoint,
            toml_password,
            throttle: IpThrottle::new(),
            server_config,
        })
    }
}

/// Constant-time byte comparison. Returns `true` iff `a` and `b` have
/// the same length and bytes. Short-circuits *only* on length so the
/// timing leak is "you guessed the right password length"; the byte
/// loop runs to completion regardless of mismatches. No `subtle` crate
/// dependency since the comparison is short and the property is easy.
fn ct_eq(a: &[u8], b: &[u8]) -> bool {
    if a.len() != b.len() {
        return false;
    }
    let mut diff = 0u8;
    for (x, y) in a.iter().zip(b.iter()) {
        diff |= x ^ y;
    }
    diff == 0
}

type EventStream = Pin<Box<dyn Stream<Item = Result<Event, Status>> + Send>>;

#[tonic::async_trait]
impl Signaling for SignalingSvc {
    type JoinStream = EventStream;

    async fn register(
        &self,
        request: Request<RegisterRequest>,
    ) -> Result<Response<RegisterResponse>, Status> {
        // Source IP from the gRPC peer address. May be `None` on
        // exotic transports (Unix sockets) — treat that as
        // "no throttle applicable" rather than rejecting outright,
        // since the operator-facing config that picks the transport
        // is implicitly trusted.
        let peer_ip = request.remote_addr().map(|a| a.ip());
        let req = request.into_inner();

        // Throttle gate: per-IP register rate cap + auth-failure
        // backoff. Checked *before* validation so a hostile flooder
        // can't even waste validator CPU. Sockets that didn't expose
        // a peer addr skip the gate.
        if let Some(ip) = peer_ip {
            if let Err(reject) = self.throttle.try_register(ip).await {
                let msg = match reject {
                    ThrottleReject::RateLimited => "too many register attempts",
                    ThrottleReject::Backoff => "auth backoff in effect",
                };
                tracing::warn!(?ip, ?reject, "register throttled");
                return Err(Status::resource_exhausted(msg));
            }
        }

        // Validate display name *before* the password check so we
        // can't be tricked into logging control characters via the
        // "register rejected: bad password" warning. Both checks are
        // cheap; ordering them this way also means an attacker
        // probing for a password length leak has to send valid
        // names, which makes the warn-log a useful audit trail.
        let display_name = match validation::display_name(&req.display_name) {
            Ok(v) => v,
            Err(e) => {
                // Bad payload counts as a failure for backoff
                // purposes — probing the validator and probing the
                // password gate are equivalently hostile.
                if let Some(ip) = peer_ip {
                    self.throttle.record_auth_failure(ip).await;
                }
                tracing::warn!(
                    ?peer_ip,
                    reason = %e.message(),
                    "register rejected: invalid display_name"
                );
                return Err(e);
            }
        };

        // Password gate — checked before we mint a session or allocate
        // any registry state. Open-mode servers (no configured
        // password from either source) skip the check entirely.
        //
        // Resolution order: TOML override > DB grpc_password > open.
        // The DB read happens on every Register so an admin-UI
        // rotation takes effect on the very next client connect
        // without a restart. TOML acts as a "lock" — when present,
        // the admin UI disables its grpc_password input via the
        // `ServerInfo.toml_password_override` flag and the DB value
        // is ignored regardless of what it contains.
        let effective_password: Option<String> = if let Some(p) = &self.toml_password {
            Some(p.clone())
        } else {
            let p = self.server_config.read().await.grpc_password.clone();
            (!p.is_empty()).then_some(p)
        };
        if let Some(required) = effective_password {
            if !ct_eq(required.as_bytes(), req.password.as_bytes()) {
                if let Some(ip) = peer_ip {
                    self.throttle.record_auth_failure(ip).await;
                }
                tracing::warn!(
                    ?peer_ip,
                    name = %display_name,
                    "register rejected: bad password"
                );
                return Err(Status::unauthenticated("invalid password"));
            }
        }
        // Clear any in-flight backoff now that we've authenticated.
        if let Some(ip) = peer_ip {
            self.throttle.record_auth_success(ip).await;
        }

        // Capacity gate: refuse new registrations once the registry
        // has reached the operator-configured ceiling. Checked after
        // the password + throttle gates so a flooder can't burn this
        // capacity check at line rate, and so the rejection message
        // doesn't leak ceiling information to unauthenticated callers.
        // We read max_peers fresh each call — admin UI edits take
        // effect on the very next register.
        let max_peers = self.server_config.read().await.max_peers as usize;
        {
            let n = self.registry.lock().await.clients.len();
            if n >= max_peers {
                tracing::warn!(
                    ?peer_ip,
                    current = n,
                    cap = max_peers,
                    "register rejected: max_peers reached"
                );
                return Err(Status::resource_exhausted("server at peer capacity"));
            }
        }

        let id = Uuid::new_v4().to_string();
        // 16-byte token: handed to the client over gRPC (response
        // travels through the same channel that just authenticated),
        // and used by the client to identify the session on UDP. We
        // hash it before storing in the registry — see H3 in the
        // audit. The raw token exists only on this stack for the
        // duration of this handler.
        let token = Uuid::new_v4().as_bytes().to_vec();
        let token_hash = hash_token(&token);
        // 32-byte symmetric key for authenticating every UDP packet
        // the client sends. Two UUIDs concatenated gives 32 bytes of
        // CSPRNG-grade entropy — same source as the token. Avoids
        // pulling in `rand` just for this.
        let mut audio_mac_key = [0u8; toki_proto::wire::MAC_KEY_LEN];
        audio_mac_key[..16].copy_from_slice(Uuid::new_v4().as_bytes());
        audio_mac_key[16..].copy_from_slice(Uuid::new_v4().as_bytes());

        let client = Client {
            id: id.clone(),
            display_name: display_name.clone(),
            audio_token_hash: token_hash,
            audio_mac_key,
            audio_last_seq: 0,
            // Start at 1 so the first outbound packet beats the
            // peer's playback-side starting cursor of 0.
            audio_outbound_seq: 1,
            audio_addr: None,
            events_tx: None,
            current_frequency: None,
            // Start the heartbeat clock at registration. The client will
            // refresh this within ~100 ms via its initial UDP keepalive,
            // and every 3 s thereafter.
            last_seen: std::time::Instant::now(),
            // Bind this session to the IP the gRPC handshake came
            // from. The audio relay rejects UDP packets bearing this
            // token from any other IP — closes the captured-token /
            // audio-hijack path. Unix-socket transports have no IP;
            // we accept any UDP source for those.
            expected_ip: peer_ip,
        };

        let mut registry = self.registry.lock().await;
        registry.tokens.insert(token_hash, id.clone());
        registry.clients.insert(id.clone(), client);
        let total = registry.clients.len();
        drop(registry);

        info!(
            client_id = %id,
            name = %display_name,
            total_clients = total,
            "client registered",
        );

        Ok(Response::new(RegisterResponse {
            client_id: id,
            audio_token: token,
            audio_endpoint: self.audio_endpoint.clone(),
            audio_mac_key: audio_mac_key.to_vec(),
        }))
    }

    async fn join(
        &self,
        request: Request<JoinRequest>,
    ) -> Result<Response<Self::JoinStream>, Status> {
        let req = request.into_inner();
        // Canonicalise the frequency — both rejects out-of-band /
        // non-step-aligned values and collapses equivalent string
        // forms ("446.05", "446.050") onto a single room key so
        // hand-crafted clients can't squat fresh rooms by varying
        // the formatting.
        let frequency = validation::frequency(&req.frequency)?;
        let (tx, rx) = mpsc::channel::<Event>(64);

        let mut registry = self.registry.lock().await;

        // Stash the event sender + initial frequency on the client.
        let display_name = {
            let client = registry
                .clients
                .get_mut(&req.client_id)
                .ok_or_else(|| Status::not_found("unknown client"))?;
            client.events_tx = Some(tx.clone());
            client.current_frequency = Some(frequency.clone());
            client.display_name.clone()
        };

        // Add to the room, snapshot the roster + holder for backfill.
        let (other_ids, current_holder, total_members) = {
            let room = registry.rooms.entry(frequency.clone()).or_default();
            if !room.members.contains(&req.client_id) {
                room.members.push(req.client_id.clone());
            }
            let others: Vec<String> = room
                .members
                .iter()
                .filter(|id| *id != &req.client_id)
                .cloned()
                .collect();
            (others, room.holder.clone(), room.members.len())
        };

        info!(
            client_id = %req.client_id,
            name = %display_name,
            frequency = %frequency,
            members = total_members,
            "client joined frequency",
        );

        let join_event = Event {
            event: Some(event::Event::Joined(MemberJoined {
                client_id: req.client_id.clone(),
                display_name,
            })),
        };

        // Backfill the new joiner with the existing roster of this freq.
        for id in &other_ids {
            if let Some(existing) = registry.clients.get(id) {
                let backfill = Event {
                    event: Some(event::Event::Joined(MemberJoined {
                        client_id: existing.id.clone(),
                        display_name: existing.display_name.clone(),
                    })),
                };
                let _ = tx.send(backfill).await;
            }
        }

        // …and with the current PTT lock if anyone holds it, so the joiner's
        // UI starts in the correct state (button disabled, "X is talking").
        if let Some(holder_id) = current_holder {
            if holder_id != req.client_id {
                let backfill = Event {
                    event: Some(event::Event::Ptt(PttEvent {
                        client_id: holder_id,
                        pressed: true,
                        sequence: 0,
                    })),
                };
                let _ = tx.send(backfill).await;
            }
        }

        // Announce the new joiner to existing members of this freq.
        for id in other_ids {
            if let Some(other) = registry.clients.get(&id) {
                if let Some(other_tx) = &other.events_tx {
                    let _ = other_tx.send(join_event.clone()).await;
                }
            }
        }

        let stream = ReceiverStream::new(rx).map(Ok);
        Ok(Response::new(Box::pin(stream) as Self::JoinStream))
    }

    async fn leave(
        &self,
        request: Request<LeaveRequest>,
    ) -> Result<Response<LeaveResponse>, Status> {
        let req = request.into_inner();

        let (recipients, left_event, release_event, display_name, frequency, remaining) = {
            let mut registry = self.registry.lock().await;
            let frequency = match registry
                .clients
                .get(&req.client_id)
                .and_then(|c| c.current_frequency.clone())
            {
                Some(f) => f,
                None => {
                    // Already not in any room — nothing to do.
                    return Ok(Response::new(LeaveResponse {}));
                }
            };
            let (recipients, left_event, release_event, display_name, remaining) =
                remove_from_room(&mut registry, &req.client_id, &frequency);
            if let Some(client) = registry.clients.get_mut(&req.client_id) {
                client.current_frequency = None;
            }
            (
                recipients,
                left_event,
                release_event,
                display_name,
                frequency,
                remaining,
            )
        };

        info!(
            client_id = %req.client_id,
            name = %display_name,
            frequency = %frequency,
            members = remaining,
            "client left frequency",
        );

        for tx in &recipients {
            let _ = tx.send(left_event.clone()).await;
            if let Some(release) = &release_event {
                let _ = tx.send(release.clone()).await;
            }
        }

        Ok(Response::new(LeaveResponse {}))
    }

    async fn change_frequency(
        &self,
        request: Request<ChangeFrequencyRequest>,
    ) -> Result<Response<ChangeFrequencyResponse>, Status> {
        let req = request.into_inner();
        // Canonicalise + validate the target frequency. Same rules as
        // `join`: out-of-band / non-step-aligned / malformed strings
        // are rejected with INVALID_ARGUMENT.
        let new_freq = validation::frequency(&req.frequency)?;

        let (
            old_recipients,
            old_left_event,
            old_release_event,
            new_other_ids,
            new_holder,
            new_join_event,
            client_tx,
            display_name,
            old_freq,
            new_freq,
        ) = {
            let mut registry = self.registry.lock().await;

            // Look up the client + their current room.
            let (old_freq_opt, client_tx, display_name) = {
                let client = registry
                    .clients
                    .get(&req.client_id)
                    .ok_or_else(|| Status::not_found("unknown client"))?;
                (
                    client.current_frequency.clone(),
                    client.events_tx.clone(),
                    client.display_name.clone(),
                )
            };

            // If they're already on the requested frequency, no-op.
            // Compare against the canonical form so a client that
            // sends "446.05" then "446.050" doesn't trigger a leave-
            // and-rejoin cycle.
            if old_freq_opt.as_deref() == Some(new_freq.as_str()) {
                return Ok(Response::new(ChangeFrequencyResponse {}));
            }

            // Remove from old room (if any) and queue old-room broadcasts.
            let (old_recipients, old_left_event, old_release_event) =
                if let Some(old_freq) = &old_freq_opt {
                    let (r, l, p, _name, _rem) =
                        remove_from_room(&mut registry, &req.client_id, old_freq);
                    (r, Some(l), p)
                } else {
                    (Vec::new(), None, None)
                };

            // Add to new room.
            let (new_other_ids, new_holder, new_members) = {
                let room = registry.rooms.entry(new_freq.clone()).or_default();
                if !room.members.contains(&req.client_id) {
                    room.members.push(req.client_id.clone());
                }
                let others: Vec<String> = room
                    .members
                    .iter()
                    .filter(|id| *id != &req.client_id)
                    .cloned()
                    .collect();
                (others, room.holder.clone(), room.members.len())
            };

            // Update the client's tracked frequency.
            if let Some(client) = registry.clients.get_mut(&req.client_id) {
                client.current_frequency = Some(new_freq.clone());
            }

            info!(
                client_id = %req.client_id,
                name = %display_name,
                from = old_freq_opt.as_deref().unwrap_or("(none)"),
                to = %new_freq,
                new_members,
                "client changed frequency",
            );

            let new_join_event = Event {
                event: Some(event::Event::Joined(MemberJoined {
                    client_id: req.client_id.clone(),
                    display_name: display_name.clone(),
                })),
            };

            (
                old_recipients,
                old_left_event,
                old_release_event,
                new_other_ids,
                new_holder,
                new_join_event,
                client_tx,
                display_name,
                old_freq_opt,
                new_freq,
            )
        };
        let _ = display_name;

        // Notify the old room that we're gone (and release any lock).
        for tx in &old_recipients {
            if let Some(ev) = &old_left_event {
                let _ = tx.send(ev.clone()).await;
            }
            if let Some(ev) = &old_release_event {
                let _ = tx.send(ev.clone()).await;
            }
        }

        // Backfill the moving client's own stream with the new room's
        // state: the FrequencyChanged confirmation, the existing
        // roster, and the current holder if any.
        if let Some(tx) = &client_tx {
            let _ = tx
                .send(Event {
                    event: Some(event::Event::FrequencyChanged(FrequencyChanged {
                        frequency: new_freq.clone(),
                    })),
                })
                .await;
            // Snapshot the new roster's members + names without holding
            // the lock across awaits.
            let new_members: Vec<(String, String)> = {
                let registry = self.registry.lock().await;
                new_other_ids
                    .iter()
                    .filter_map(|id| {
                        registry
                            .clients
                            .get(id)
                            .map(|c| (c.id.clone(), c.display_name.clone()))
                    })
                    .collect()
            };
            for (id, name) in new_members {
                let _ = tx
                    .send(Event {
                        event: Some(event::Event::Joined(MemberJoined {
                            client_id: id,
                            display_name: name,
                        })),
                    })
                    .await;
            }
            if let Some(holder_id) = new_holder {
                if holder_id != req.client_id {
                    let _ = tx
                        .send(Event {
                            event: Some(event::Event::Ptt(PttEvent {
                                client_id: holder_id,
                                pressed: true,
                                sequence: 0,
                            })),
                        })
                        .await;
                }
            }
        }

        // Announce ourselves to the rest of the new room.
        let new_recipient_txs: Vec<mpsc::Sender<Event>> = {
            let registry = self.registry.lock().await;
            new_other_ids
                .iter()
                .filter_map(|id| registry.clients.get(id))
                .filter_map(|c| c.events_tx.clone())
                .collect()
        };
        for tx in new_recipient_txs {
            let _ = tx.send(new_join_event.clone()).await;
        }
        let _ = old_freq;

        Ok(Response::new(ChangeFrequencyResponse {}))
    }

    /// Walkie-talkie arbitration. Only PTT events that change room state
    /// are broadcast within the sender's current frequency room:
    ///   - `pressed = true` is granted iff no one currently holds the room.
    ///     Denied requests are silently dropped — the requester's UI already
    ///     reflects the actual holder via the broadcast they received (or
    ///     the join-time backfill).
    ///   - `pressed = false` is honored only if the sender is the current
    ///     holder; otherwise ignored.
    async fn push_to_talk(
        &self,
        request: Request<Streaming<PttEvent>>,
    ) -> Result<Response<PttAck>, Status> {
        let mut stream = request.into_inner();
        while let Some(evt) = stream.next().await {
            let evt = evt?;

            let broadcast: Option<(bool, Vec<mpsc::Sender<Event>>)> = {
                let mut registry = self.registry.lock().await;

                let frequency = match registry
                    .clients
                    .get(&evt.client_id)
                    .and_then(|c| c.current_frequency.clone())
                {
                    Some(f) => f,
                    None => continue, // sender isn't in any room
                };

                let action = {
                    let room = registry.rooms.entry(frequency.clone()).or_default();
                    match (room.holder.as_deref(), evt.pressed) {
                        (None, true) => {
                            room.holder = Some(evt.client_id.clone());
                            Some(true)
                        }
                        (Some(h), false) if h == evt.client_id => {
                            room.holder = None;
                            Some(false)
                        }
                        _ => None,
                    }
                };

                action.map(|pressed| {
                    let member_ids: Vec<String> = registry
                        .rooms
                        .get(&frequency)
                        .map(|r| r.members.clone())
                        .unwrap_or_default();
                    let recipients: Vec<mpsc::Sender<Event>> = member_ids
                        .iter()
                        .filter_map(|id| registry.clients.get(id))
                        .filter_map(|c| c.events_tx.clone())
                        .collect();
                    (pressed, recipients)
                })
            };

            let Some((pressed, recipients)) = broadcast else {
                continue;
            };

            let event = Event {
                event: Some(event::Event::Ptt(PttEvent {
                    client_id: evt.client_id.clone(),
                    pressed,
                    sequence: evt.sequence,
                })),
            };

            for tx in recipients {
                let _ = tx.send(event.clone()).await;
            }
        }
        Ok(Response::new(PttAck {}))
    }
}

/// Strip a client out of a specific frequency room. Returns:
///   - the event senders of the *remaining* members (the people who
///     should hear the MemberLeft / Ptt release),
///   - the MemberLeft event ready to broadcast,
///   - an optional Ptt release event (only if the leaver held PTT),
///   - the leaver's display name (for logging),
///   - the remaining member count.
///
/// The caller is responsible for clearing the leaver's
/// `current_frequency` if appropriate and for actually awaiting the
/// broadcasts outside the registry lock.
///
/// `pub(crate)` so the admin module can reuse this for its
/// "move to frequency" operation — keeps the leave-side semantics
/// identical between gRPC ChangeFrequency and admin-driven moves.
pub(crate) fn remove_from_room(
    registry: &mut Registry,
    client_id: &str,
    frequency: &str,
) -> (
    Vec<mpsc::Sender<Event>>,
    Event,
    Option<Event>,
    String,
    usize,
) {
    let was_holder = if let Some(room) = registry.rooms.get_mut(frequency) {
        room.members.retain(|id| id != client_id);
        if room.holder.as_deref() == Some(client_id) {
            room.holder = None;
            true
        } else {
            false
        }
    } else {
        false
    };

    let display_name = registry
        .clients
        .get(client_id)
        .map(|c| c.display_name.clone())
        .unwrap_or_else(|| client_id.to_string());

    // Drop empty rooms so the registry doesn't grow unbounded as
    // people hop frequencies — they'll be lazily recreated by the
    // next Join into them.
    let remaining = if let Some(room) = registry.rooms.get(frequency) {
        if room.members.is_empty() && room.holder.is_none() {
            registry.rooms.remove(frequency);
            0
        } else {
            room.members.len()
        }
    } else {
        0
    };

    let recipients: Vec<mpsc::Sender<Event>> = registry
        .rooms
        .get(frequency)
        .map(|r| r.members.clone())
        .unwrap_or_default()
        .iter()
        .filter_map(|id| registry.clients.get(id))
        .filter_map(|c| c.events_tx.clone())
        .collect();

    let left_event = Event {
        event: Some(event::Event::Left(MemberLeft {
            client_id: client_id.to_string(),
        })),
    };

    let release_event = if was_holder {
        Some(Event {
            event: Some(event::Event::Ptt(PttEvent {
                client_id: client_id.to_string(),
                pressed: false,
                sequence: 0,
            })),
        })
    } else {
        None
    };

    (
        recipients,
        left_event,
        release_event,
        display_name,
        remaining,
    )
}
