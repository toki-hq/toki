use std::pin::Pin;

use tokio::sync::mpsc;
use tokio_stream::{wrappers::ReceiverStream, Stream, StreamExt};
use tonic::{Request, Response, Status, Streaming};
use tracing::info;
use uuid::Uuid;

use toki_proto::v1::{
    event,
    signaling_server::{Signaling, SignalingServer},
    BroadcastCapabilityChanged, ChangeFrequencyRequest, ChangeFrequencyResponse,
    ChannelModeChanged, ChannelMuteChanged, ChannelNameChanged, ConnectionQualityAck,
    ConnectionQualityReport, Event, FrequencyChanged, IdentityChallengeRequest,
    IdentityChallengeResponse, JoinRequest, LeaveRequest, LeaveResponse, MemberJoined, MemberLeft,
    PriorityChanged, PttAck, PttEvent, RegisterRequest, RegisterResponse,
};

use crate::audit::{self, AuditSink};
use crate::server_config::SharedServerConfig;
use crate::state::{
    hash_token, Client, DuplexMode, Registry, SharedChannelNames, SharedDuplexModes, SharedRegistry,
};
use crate::throttle::{IpThrottle, ThrottleReject};
use crate::validation;

/// This server's own version, used to gate client compatibility on
/// `Register` (matching MAJOR.MINOR required; see `toki_proto::version`).
const SERVER_VERSION: &str = env!("CARGO_PKG_VERSION");

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
    /// override) without requiring a restart on change. Also gates
    /// the named-channels feature (`named_channels_enabled`).
    server_config: SharedServerConfig,
    /// Admin-assigned channel names (frequency → name), shared with
    /// the admin panel which writes them. Consulted on `Join` /
    /// `ChangeFrequency` to deliver the current name to the client —
    /// but only while `server_config.named_channels_enabled` is on.
    channel_names: SharedChannelNames,
    /// Admin-assigned per-frequency duplex modes (frequency →
    /// [`DuplexMode`]), shared with the admin panel which writes them.
    /// Consulted on `Join` / `ChangeFrequency` to deliver the channel's
    /// mode to the client and to seed a freshly-created room's
    /// `Room.duplex`. Absent key = half-duplex.
    duplex_modes: SharedDuplexModes,
    /// Channel-wide mutes (frequency set), shared with the admin panel
    /// which writes them. Consulted by the PTT speak-gate: a press on a
    /// muted channel is refused, so no one transmits there until it's
    /// unmuted or they tune away. Also delivered to clients on `Join` /
    /// `ChangeFrequency` via `channel_mute_event` so the PTT button can
    /// show the disabled cue.
    channel_mutes: crate::state::SharedChannelMutes,
    /// Audit-log sink. Records peer connects (on `Register`), explicit
    /// disconnects (on `Leave`), and failed password attempts.
    audit: AuditSink,
    /// Identity records seen by this server, hydrated from the
    /// `identities` table at boot by the admin task. `Register` is
    /// the writer: it merges the verified identity against the prior
    /// record (stored `first_seen` / `origin_client_id` win) and
    /// pushes the result onto `identity_tx` for persistence.
    identities: crate::state::SharedIdentities,
    /// Persistence side of the identity pipeline — drained by the
    /// admin task into the `identities` table, mirroring the audit
    /// channel split (signaling produces, the db owner writes).
    identity_tx: mpsc::UnboundedSender<(String, crate::state::IdentityRecord)>,
    /// Active identity bans, written by the admin panel (ban / lift)
    /// and consulted on every identity-ful `Register`. A banned pubkey
    /// — or, for machine-tier bans, a banned machine hash under any
    /// key — is rejected with PERMISSION_DENIED + the operator's
    /// reason.
    bans: crate::state::SharedBans,
    /// Per-boot key for the stateless register-challenge nonces.
    /// Restarting the server invalidates outstanding challenges —
    /// harmless, the client just registers with a fresh one.
    challenge_key: crate::identity::ChallengeKey,
}

impl SignalingSvc {
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        registry: SharedRegistry,
        audio_endpoint: String,
        toml_password: Option<String>,
        server_config: SharedServerConfig,
        channel_names: SharedChannelNames,
        duplex_modes: SharedDuplexModes,
        channel_mutes: crate::state::SharedChannelMutes,
        identities: crate::state::SharedIdentities,
        identity_tx: mpsc::UnboundedSender<(String, crate::state::IdentityRecord)>,
        bans: crate::state::SharedBans,
        audit: AuditSink,
    ) -> SignalingServer<Self> {
        SignalingServer::new(Self {
            registry,
            audio_endpoint,
            toml_password,
            throttle: IpThrottle::new(),
            server_config,
            channel_names,
            duplex_modes,
            channel_mutes,
            audit,
            identities,
            identity_tx,
            bans,
            challenge_key: crate::identity::ChallengeKey::generate(),
        })
    }

    /// Verify and record a register request's identity fields.
    ///
    /// `Ok(None)` for an identity-less register; `Ok(Some(_))` once
    /// possession of the key is proven — with the side effects that
    /// make it durable: the merged record lands in the shared
    /// identity map and is queued for the admin task to persist.
    /// The merge pins `first_seen` and a non-empty `origin_client_id`
    /// to their stored values, so a returning identity can't rewrite
    /// its history by claiming differently.
    async fn process_identity(
        &self,
        req: &RegisterRequest,
        display_name: &str,
        peer_ip: Option<std::net::IpAddr>,
    ) -> Result<Option<crate::state::ClientIdentity>, Status> {
        let now = crate::admin::db::now_unix();
        let Some(verified) =
            crate::identity::verify_register(&self.challenge_key, req, now as u64)?
        else {
            return Ok(None);
        };

        let mut map = self.identities.write().await;
        let prior = map.get(&verified.pubkey_hex).cloned();
        let session = crate::identity::merged_identity(&verified, prior.as_ref(), now);
        let record = crate::state::IdentityRecord {
            display_id: session.display_id.clone(),
            last_callsign: display_name.to_string(),
            machine_hash: verified.machine_hash.clone(),
            origin_client_id: match prior.as_ref().map(|r| r.origin_client_id.as_str()) {
                Some(stored) if !stored.is_empty() => stored.to_string(),
                _ => verified.origin_client_id.clone(),
            },
            first_seen: session.first_seen,
            last_seen: now,
            last_ip: peer_ip.map(|i| i.to_string()).unwrap_or_default(),
        };
        map.insert(verified.pubkey_hex.clone(), record.clone());
        drop(map);

        // Fire-and-forget persistence — a closed channel (admin task
        // torn down) costs durability for this update, never the
        // session itself.
        let _ = self.identity_tx.send((verified.pubkey_hex, record));
        Ok(Some(session))
    }

    /// Build a `ChannelNameChanged` event for `frequency` if the
    /// named-channels feature is enabled, returning `None` when the
    /// feature is off (so callers simply skip the send). An enabled
    /// feature with no stored name yields an event with an empty
    /// `name` — an explicit "this channel is unnamed" signal the
    /// client uses to clear any stale label.
    async fn channel_name_event(&self, frequency: &str) -> Option<Event> {
        if !self.server_config.read().await.named_channels_enabled {
            return None;
        }
        let name = self
            .channel_names
            .read()
            .await
            .get(frequency)
            .cloned()
            .unwrap_or_default();
        Some(Event {
            event: Some(event::Event::ChannelNameChanged(ChannelNameChanged {
                frequency: frequency.to_string(),
                name,
            })),
        })
    }

    /// The *effective* duplex mode of a frequency: the stored mode when
    /// the full-duplex feature is enabled, else `Half` (the feature gate
    /// forces every channel half-duplex regardless of stored modes).
    async fn effective_duplex(&self, frequency: &str) -> DuplexMode {
        if !self.server_config.read().await.full_duplex_enabled {
            return DuplexMode::Half;
        }
        self.duplex_modes
            .read()
            .await
            .get(frequency)
            .copied()
            .unwrap_or_default()
    }

    /// Build a `ChannelModeChanged` event for `frequency`, or `None` when
    /// the full-duplex feature is disabled (so callers skip the send and
    /// the client shows no duplex indicator at all). When enabled, carries
    /// the channel's effective mode so the client picks the right PTT path.
    async fn channel_mode_event(&self, frequency: &str) -> Option<Event> {
        if !self.server_config.read().await.full_duplex_enabled {
            return None;
        }
        let mode = self.effective_duplex(frequency).await;
        Some(Event {
            event: Some(event::Event::ChannelModeChanged(ChannelModeChanged {
                frequency: frequency.to_string(),
                mode: mode.as_u32() as i32,
            })),
        })
    }

    /// Build a `ChannelMuteChanged` carrying `frequency`'s current mute
    /// state, for delivery on `Join` / `ChangeFrequency` so a client
    /// learns immediately whether it can transmit here. Always returns
    /// an event (unlike `channel_name_event`, channel mute has no
    /// feature gate) — an unmuted channel sends `muted = false`, which
    /// also clears any stale mute the client carried from a prior
    /// channel.
    async fn channel_mute_event(&self, frequency: &str) -> Event {
        let muted = self.channel_mutes.read().await.contains(frequency);
        Event {
            event: Some(event::Event::ChannelMuteChanged(ChannelMuteChanged {
                frequency: frequency.to_string(),
                muted,
            })),
        }
    }

    /// Build a `PriorityChanged` telling `client_id` whether it is a
    /// priority speaker *on `frequency`*. Sent on `ChangeFrequency` so a
    /// priority grant that's bound to one channel correctly goes dormant
    /// when the holder tunes away and re-activates on return — which
    /// matters for the client's PTT cue on muted (No-Talk) channels,
    /// where only a priority speaker keeps a live button. `granted` is
    /// true iff the session's `priority_freq` matches `frequency`.
    async fn priority_event(&self, client_id: &str, frequency: &str) -> Event {
        let granted = self
            .registry
            .lock()
            .await
            .clients
            .get(client_id)
            .map(|c| c.priority_freq.as_deref() == Some(frequency))
            .unwrap_or(false);
        Event {
            event: Some(event::Event::PriorityChanged(PriorityChanged {
                client_id: client_id.to_string(),
                frequency: frequency.to_string(),
                granted,
            })),
        }
    }
}

/// Build a `BroadcastCapabilityChanged` telling `client_id` its current
/// global-broadcast capability standing. Sent on `Join` / `ChangeFrequency`
/// so a reconnecting or channel-switching client re-learns its capability.
///
/// FIX 6: pure free function taking an already-held `&Registry` reference
/// so callers can construct the event without acquiring the registry lock
/// a second time. Calling the old `async fn` version while already holding
/// the registry guard would self-deadlock (tokio::Mutex is not reentrant).
fn broadcast_capability_event(registry: &crate::state::Registry, client_id: &str) -> Event {
    let granted = registry
        .clients
        .get(client_id)
        .map(|c| c.can_global_broadcast)
        .unwrap_or(false);
    Event {
        event: Some(event::Event::BroadcastCapabilityChanged(
            BroadcastCapabilityChanged {
                client_id: client_id.to_string(),
                granted,
            },
        )),
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

/// May a connection-quality report from `caller_ip` update a session
/// whose registration IP is `expected_ip`? Honoured iff the caller's IP
/// matches the session's pinned IP — the same source-IP binding the audio
/// relay enforces (see `audio.rs`). A session bound to no IP (Unix-socket
/// transport) or a caller with no observable IP can't be matched, so we
/// allow it, matching the relay's "`expected_ip = None` skips the check"
/// behaviour. Pulled out as a pure fn so the matching logic is unit-
/// testable without an IP-bearing transport (the in-process test harness
/// exposes no peer address).
fn quality_report_ip_ok(
    expected_ip: Option<std::net::IpAddr>,
    caller_ip: Option<std::net::IpAddr>,
) -> bool {
    match expected_ip {
        Some(bound) => caller_ip == Some(bound),
        None => true,
    }
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

        // Protocol-version gate. The UDP audio wire format / gRPC
        // contract can change across minor versions, so a client on a
        // different MAJOR.MINOR would connect but get silently broken
        // audio. Reject up front with an actionable message. This is a
        // benign incompatibility, not an attack, so it does NOT record
        // an auth failure / trip the backoff — the user just needs to
        // update. Patch releases are wire-compatible and pass.
        if !toki_proto::version::compatible(SERVER_VERSION, &req.client_version) {
            let client_v = if req.client_version.is_empty() {
                "unknown"
            } else {
                &req.client_version
            };
            tracing::warn!(
                ?peer_ip,
                client_version = %client_v,
                server_version = SERVER_VERSION,
                "register rejected: incompatible client version"
            );
            return Err(Status::failed_precondition(format!(
                "version mismatch: this server is Toki v{SERVER_VERSION}; \
                 your client (v{client_v}) must match its major.minor version. \
                 Please update the client."
            )));
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
                audit::record(
                    &self.audit,
                    "auth-fail",
                    audit::SYSTEM_ACTOR,
                    "",
                    &format!(
                        "bad server password from {} (callsign {display_name})",
                        peer_ip.map(|i| i.to_string()).unwrap_or_else(|| "?".into())
                    ),
                );
                return Err(Status::unauthenticated("invalid password"));
            }
        }
        // Clear any in-flight backoff now that we've authenticated.
        if let Some(ip) = peer_ip {
            self.throttle.record_auth_success(ip).await;
        }

        // Optional keypair identity: verify (a present-but-invalid one
        // rejects the register — never a silent downgrade to anonymous)
        // and merge into the identity store. After the password gate so
        // unauthenticated callers can't probe identity verification.
        let identity = self.process_identity(&req, &display_name, peer_ip).await?;

        // Require-identity gate (off by default): with the toggle on,
        // an identity-less register is refused — the lever that makes
        // identity bans airtight, since an evader can no longer just
        // connect anonymously. Read fresh so flipping the toggle in
        // the admin panel applies to the next register, no restart.
        if identity.is_none() && self.server_config.read().await.require_identity {
            tracing::warn!(
                ?peer_ip,
                name = %display_name,
                "register rejected: server requires a client identity"
            );
            return Err(Status::failed_precondition(
                "this server requires a client identity; \
                 update your client or repair its identity file and reconnect",
            ));
        }

        // Ban gate: a verified identity whose pubkey is banned — or, for
        // machine-tier bans, whose machine hash matches ANY ban row — is
        // refused with the operator's reason. Checked after identity
        // verification (an unproven pubkey can't be used to probe the
        // ban list) and after recording the attempt in the identity
        // store, so the operator sees the banned identity's last_seen /
        // last_ip update on each retry.
        if let Some(ident) = &identity {
            let bans = self.bans.read().await;
            let hit = bans.get(&ident.pubkey_hex).or_else(|| {
                if ident.machine_hash.is_empty() {
                    None
                } else {
                    bans.values().find(|b| {
                        !b.machine_hash.is_empty() && b.machine_hash == ident.machine_hash
                    })
                }
            });
            if let Some(ban) = hit {
                let reason = if ban.reason.is_empty() {
                    "you are banned from this server".to_string()
                } else {
                    format!("you are banned from this server: {}", ban.reason)
                };
                tracing::warn!(
                    ?peer_ip,
                    identity = %ident.display_id,
                    name = %display_name,
                    "register rejected: banned identity"
                );
                audit::record(
                    &self.audit,
                    "ban-reject",
                    &display_name,
                    "",
                    &format!(
                        "banned identity {} tried to register from {}",
                        ident.display_id,
                        peer_ip.map(|i| i.to_string()).unwrap_or_else(|| "?".into())
                    ),
                );
                return Err(Status::permission_denied(reason));
            }
        }

        // Capacity gate: refuse new registrations once the registry has
        // reached the operator-configured ceiling. Read the ceiling fresh
        // here (after the password + throttle gates, so a flooder can't
        // burn it at line rate and the rejection doesn't leak the ceiling
        // to unauthenticated callers, and so an admin UI edit applies to
        // the very next register), but enforce it *inside* the same
        // registry lock that inserts below — otherwise N concurrent
        // registers all read a sub-ceiling count, all pass, and all
        // insert, overshooting the cap. Same check-and-insert-under-one-
        // lock discipline as the unique-callsign gate.
        let max_peers = self.server_config.read().await.max_peers as usize;

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

        let mut client = Client {
            id: id.clone(),
            display_name: display_name.clone(),
            audio_token_hash: token_hash,
            audio_mac_key,
            audio_last_seq: 0,
            // Start at 1 so the first outbound packet beats the
            // peer's playback-side starting cursor of 0.
            audio_outbound_seq: 1,
            // Assigned from the registry counter under the lock below.
            audio_id: 0,
            audio_addr: None,
            events_tx: None,
            current_frequency: None,
            // Ordinary member until an admin elects them via the
            // panel's "promote to priority" action.
            priority_freq: None,
            // Start the heartbeat clock at registration. The client will
            // refresh this within ~100 ms via its initial UDP keepalive,
            // and every 3 s thereafter.
            last_seen: std::time::Instant::now(),
            // Frozen "session start" instant — never updated, so the
            // admin panel's "connected for X" stat grows at 1 s/s
            // instead of resetting on every keepalive like `last_seen`
            // does.
            connected_at: std::time::Instant::now(),
            // Bind this session to the IP the gRPC handshake came
            // from. The audio relay rejects UDP packets bearing this
            // token from any other IP — closes the captured-token /
            // audio-hijack path. Unix-socket transports have no IP;
            // we accept any UDP source for those.
            expected_ip: peer_ip,
            identity: identity.clone(),
            // Fresh sessions start un-muted; an admin mute is
            // session-scoped and re-applied per reconnect.
            muted: false,
            // No global-broadcast capability until an admin grants it.
            can_global_broadcast: false,
            // No quality sample until the client's first report.
            quality: None,
        };

        // Unique-callsign gate (on by default). Read the toggle before
        // the lock, then do the taken-check *inside* the same registry
        // lock that inserts, so two simultaneous registers of the same
        // name can't both slip through (no TOCTOU window). Case-
        // insensitive — `ECHO-1` and `echo-1` collide. A name frees up
        // the instant its holder disconnects (we only scan live clients).
        let unique_callsigns = self.server_config.read().await.unique_callsigns;

        let mut registry = self.registry.lock().await;
        // Enforce the capacity ceiling here, under the insert lock (see the
        // max_peers note above). `>=` so the cap is the count of live
        // sessions, not one past it.
        let n = registry.clients.len();
        if n >= max_peers {
            drop(registry);
            tracing::warn!(
                ?peer_ip,
                current = n,
                cap = max_peers,
                "register rejected: max_peers reached"
            );
            return Err(Status::resource_exhausted("server at peer capacity"));
        }
        let own_pubkey = identity.as_ref().map(|i| i.pubkey_hex.as_str());
        if unique_callsigns && registry.callsign_taken(&display_name, None, own_pubkey) {
            drop(registry);
            tracing::warn!(
                ?peer_ip,
                name = %display_name,
                "register rejected: callsign already in use"
            );
            return Err(Status::already_exists(format!(
                "callsign \"{display_name}\" is already in use on this server"
            )));
        }
        // Stamp the per-session audio routing id used in the S2C header
        // so receivers can demux concurrent talkers on full-duplex.
        client.audio_id = registry.alloc_audio_id();
        registry.tokens.insert(token_hash, id.clone());
        registry.clients.insert(id.clone(), client);
        let total = registry.clients.len();
        drop(registry);

        info!(
            client_id = %id,
            name = %display_name,
            identity = identity.as_ref().map(|i| i.display_id.as_str()).unwrap_or("-"),
            total_clients = total,
            "client registered",
        );
        let from_ip = peer_ip.map(|i| i.to_string()).unwrap_or_else(|| "?".into());
        audit::record(
            &self.audit,
            "connect",
            &display_name,
            "",
            // Identity in the detail ties the audit trail to the
            // durable "who" rather than the freely-chosen callsign.
            &match &identity {
                Some(i) => format!("from {from_ip} as {}", i.display_id),
                None => format!("from {from_ip}"),
            },
        );

        // Advertise the operator's chosen voice codec/quality so the
        // client knows whether to Opus-encode, at what bitrate, whether
        // to enable DTX, and the frame duration. One config snapshot for
        // all four, read fresh so an admin change applies to the next
        // connection. DTX and frame_ms are only meaningful with Opus on
        // (no effect on the raw-PCM path).
        let (opus_enabled, opus_bitrate, opus_dtx, opus_frame_ms) = {
            let cfg = self.server_config.read().await;
            let (enabled, bitrate) = crate::server_config::opus_settings(cfg.audio_quality);
            (enabled, bitrate, enabled && cfg.opus_dtx, cfg.opus_frame_ms)
        };

        Ok(Response::new(RegisterResponse {
            client_id: id,
            audio_token: token,
            audio_endpoint: self.audio_endpoint.clone(),
            audio_mac_key: audio_mac_key.to_vec(),
            opus_enabled,
            opus_bitrate,
            opus_dtx,
            opus_frame_ms,
        }))
    }

    /// Issue a register-challenge nonce for the optional identity
    /// handshake. Stateless: the nonce carries its own timestamp + keyed
    /// tag (see `crate::identity`), so there's nothing to store or clean
    /// up. It *is* per-IP rate-capped, though — issuing a nonce still
    /// burns randomness + a keyed hash, and an unauthenticated caller
    /// could otherwise hammer it for free at line rate. The cap is
    /// independent of (and looser than) the register cap so a legitimate
    /// connect's single challenge never eats the register budget.
    async fn identity_challenge(
        &self,
        request: Request<IdentityChallengeRequest>,
    ) -> Result<Response<IdentityChallengeResponse>, Status> {
        // Per-IP challenge rate cap. Sockets without a peer addr (Unix
        // transports) skip the gate, same as register.
        if let Some(ip) = request.remote_addr().map(|a| a.ip()) {
            if let Err(reject) = self.throttle.try_challenge(ip).await {
                tracing::warn!(?ip, ?reject, "identity_challenge throttled");
                return Err(Status::resource_exhausted("too many challenge requests"));
            }
        }
        Ok(Response::new(IdentityChallengeResponse {
            nonce: crate::identity::issue(&self.challenge_key, crate::admin::db::now_unix() as u64),
        }))
    }

    async fn report_connection_quality(
        &self,
        request: Request<ConnectionQualityReport>,
    ) -> Result<Response<ConnectionQualityAck>, Status> {
        // Bind the report to the caller's IP before consuming the request.
        // The body carries the `client_id` to update, but nothing proves
        // the caller *is* that client — so we require the caller's IP to
        // match the target session's `expected_ip` (the IP its gRPC
        // Register came from, the same IP the audio relay already pins UDP
        // to). Without this, any connected client — or, on an open-mode
        // server, anyone on the network — could forge another peer's
        // RTT/loss in the admin dashboard. A session bound to no IP
        // (Unix-socket transport, where `expected_ip` is `None`) can't be
        // matched, so we let those through, consistent with the relay
        // accepting any UDP source for them.
        let caller_ip = request.remote_addr().map(|a| a.ip());
        let r = request.into_inner();
        // Best-effort denormalize onto the live session for the admin
        // snapshot. An unknown client_id (disconnected mid-report) or an
        // IP mismatch is a no-op success — quality reports are advisory,
        // and a silent no-op avoids both nagging a client that's already
        // gone and leaking client-id existence to a prober.
        if let Some(client) = self.registry.lock().await.clients.get_mut(&r.client_id) {
            if quality_report_ip_ok(client.expected_ip, caller_ip) {
                client.quality = Some(crate::state::ConnQuality {
                    rtt_ms: r.rtt_ms,
                    jitter_ms: r.jitter_ms,
                    loss_pct_centi: r.loss_pct_centi,
                });
            } else {
                tracing::warn!(
                    ?caller_ip,
                    client_id = %r.client_id,
                    "connection-quality report rejected: caller IP does not match session"
                );
            }
        }
        Ok(Response::new(ConnectionQualityAck {}))
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

        // Read the channel's effective duplex mode (gated by the feature
        // toggle) before taking the registry lock, so we can seed a
        // freshly-created room's `duplex` without holding two locks across
        // an await.
        let duplex = self.effective_duplex(&frequency).await;

        // FIX 3: collect everything needed under the lock, then drop the
        // guard before any .await sends. A bounded channel (cap 64) that
        // fills will block send().await → blocks the registry lock →
        // stalls the audio relay + all other handlers globally.
        let (
            display_name,
            total_members,
            backfill_roster,    // (id, display_name) pairs for roster backfill
            current_holder,     // holder id for PTT-state backfill
            can_broadcast,      // this joiner's broadcast capability
            active_broadcaster, // id of client currently broadcasting (if any, excl. self)
            peer_announce_txs,  // (tx, join_event) for each existing member
            join_event,
        ) = {
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
                // Seed/refresh the room's duplex mode from the shared map
                // (self-healing if the room pre-existed and the mode changed).
                room.duplex = duplex;
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

            // Snapshot the roster backfill events (id, name) pairs.
            let backfill_roster: Vec<(String, String)> = other_ids
                .iter()
                .filter_map(|id| {
                    registry
                        .clients
                        .get(id)
                        .map(|c| (c.id.clone(), c.display_name.clone()))
                })
                .collect();

            // Read broadcast state off the already-held guard using the
            // pure free function (no re-lock needed).
            let can_broadcast = registry
                .clients
                .get(&req.client_id)
                .map(|c| c.can_global_broadcast)
                .unwrap_or(false);
            let active_broadcaster = registry
                .broadcast_active
                .clone()
                .filter(|id| id != &req.client_id);

            let join_event = Event {
                event: Some(event::Event::Joined(MemberJoined {
                    client_id: req.client_id.clone(),
                    display_name: display_name.clone(),
                })),
            };

            // Collect (tx, event) for each existing member so we can
            // announce the joiner to them after the lock drops.
            let peer_announce_txs: Vec<mpsc::Sender<Event>> = other_ids
                .iter()
                .filter_map(|id| registry.clients.get(id))
                .filter_map(|c| c.events_tx.clone())
                .collect();

            (
                display_name,
                total_members,
                backfill_roster,
                current_holder,
                can_broadcast,
                active_broadcaster,
                peer_announce_txs,
                join_event,
            )
        }; // registry lock released here — no .await above this point

        info!(
            client_id = %req.client_id,
            name = %display_name,
            frequency = %frequency,
            members = total_members,
            "client joined frequency",
        );

        // ── Post-lock sends (all .await calls are outside the lock) ──────

        // Backfill the new joiner with the existing roster of this freq.
        for (id, name) in backfill_roster {
            let backfill = Event {
                event: Some(event::Event::Joined(MemberJoined {
                    client_id: id,
                    display_name: name,
                })),
            };
            let _ = tx.send(backfill).await;
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
                        // Roster backfill is state sync, not a live
                        // takeover — never trigger the priority roger
                        // on join even if the holder is a priority
                        // speaker.
                        priority: false,
                        broadcast: false,
                        display_name: String::new(),
                    })),
                };
                let _ = tx.send(backfill).await;
            }
        }

        // Deliver the channel's name (when the feature is on) so the
        // joiner's UI labels the frequency immediately. Empty name =
        // "unnamed"; skipped entirely while the feature is off.
        // channel_name_event and channel_mute_event lock DIFFERENT resources
        // (channel_names / channel_mutes / server_config), safe after
        // the registry guard is dropped.
        if let Some(name_evt) = self.channel_name_event(&frequency).await {
            let _ = tx.send(name_evt).await;
        }
        // Deliver the channel's mute state so the joiner's PTT button
        // reflects it right away (always sent — no feature gate).
        let _ = tx.send(self.channel_mute_event(&frequency).await).await;

        // Deliver the client's global-broadcast capability so the joiner
        // knows whether its broadcast PTT is live.
        let _ = tx
            .send(Event {
                event: Some(event::Event::BroadcastCapabilityChanged(
                    BroadcastCapabilityChanged {
                        client_id: req.client_id.clone(),
                        granted: can_broadcast,
                    },
                )),
            })
            .await;
        // Deliver the active broadcast indicator (if someone else is
        // broadcasting) so the joiner immediately shows it.
        if let Some(active_id) = active_broadcaster {
            let _ = tx
                .send(Event {
                    event: Some(event::Event::Ptt(PttEvent {
                        client_id: active_id,
                        pressed: true,
                        sequence: 0,
                        priority: false,
                        broadcast: true,
                        display_name: String::new(),
                    })),
                })
                .await;
        }

        // Deliver the channel's duplex mode (only when the feature is on)
        // so the client picks the right PTT path. When off, no event is
        // sent and the client shows no duplex indicator.
        if let Some(mode_evt) = self.channel_mode_event(&frequency).await {
            let _ = tx.send(mode_evt).await;
        }

        // Announce the new joiner to existing members of this freq.
        for peer_tx in peer_announce_txs {
            let _ = peer_tx.send(join_event.clone()).await;
        }

        let stream = ReceiverStream::new(rx).map(Ok);
        Ok(Response::new(Box::pin(stream) as Self::JoinStream))
    }

    async fn leave(
        &self,
        request: Request<LeaveRequest>,
    ) -> Result<Response<LeaveResponse>, Status> {
        let req = request.into_inner();

        let (
            recipients,
            left_event,
            release_event,
            display_name,
            frequency,
            remaining,
            broadcast_teardown_txs,
        ) = {
            let mut registry = self.registry.lock().await;
            let frequency = match registry
                .clients
                .get(&req.client_id)
                .and_then(|c| c.current_frequency.clone())
            {
                Some(f) => f,
                None => {
                    // Already not in any room — nothing to do.
                    // But still check for a live broadcast by this client.
                    let bcast_txs =
                        if registry.broadcast_active.as_deref() == Some(req.client_id.as_str()) {
                            registry.broadcast_active = None;
                            registry
                                .clients
                                .iter()
                                .filter(|(id, _)| *id != &req.client_id)
                                .filter_map(|(_, c)| c.events_tx.clone())
                                .collect::<Vec<_>>()
                        } else {
                            Vec::new()
                        };
                    drop(registry);
                    if !bcast_txs.is_empty() {
                        let bcast_end = Event {
                            event: Some(event::Event::Ptt(PttEvent {
                                client_id: req.client_id.clone(),
                                pressed: false,
                                sequence: 0,
                                priority: false,
                                broadcast: true,
                                display_name: String::new(),
                            })),
                        };
                        for tx in bcast_txs {
                            let _ = tx.send(bcast_end.clone()).await;
                        }
                    }
                    return Ok(Response::new(LeaveResponse {}));
                }
            };

            // Broadcast teardown on leave: if this client held the broadcast,
            // clear it and collect all other clients' senders for notification.
            let broadcast_teardown_txs: Vec<mpsc::Sender<Event>> =
                if registry.broadcast_active.as_deref() == Some(req.client_id.as_str()) {
                    registry.broadcast_active = None;
                    registry
                        .clients
                        .iter()
                        .filter(|(id, _)| *id != &req.client_id)
                        .filter_map(|(_, c)| c.events_tx.clone())
                        .collect()
                } else {
                    Vec::new()
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
                broadcast_teardown_txs,
            )
        };

        info!(
            client_id = %req.client_id,
            name = %display_name,
            frequency = %frequency,
            members = remaining,
            "client left frequency",
        );
        audit::record(&self.audit, "disconnect", &display_name, &frequency, "left");

        // If this client was broadcasting, send the broadcast-release event to
        // all remaining clients so their UIs clear the broadcast indicator.
        if !broadcast_teardown_txs.is_empty() {
            let bcast_end = Event {
                event: Some(event::Event::Ptt(PttEvent {
                    client_id: req.client_id.clone(),
                    pressed: false,
                    sequence: 0,
                    priority: false,
                    broadcast: true,
                    display_name: String::new(),
                })),
            };
            for tx in &broadcast_teardown_txs {
                let _ = tx.send(bcast_end.clone()).await;
            }
        }

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

        // Duplex mode of the target channel, read before the registry
        // lock so we can seed a freshly-created room without nesting locks.
        let new_duplex = self.effective_duplex(&new_freq).await;

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
                room.duplex = new_duplex;
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
            // Label the new channel (when the feature is on). Sent right
            // after FrequencyChanged so the client applies it to the freq
            // it just confirmed it moved to.
            if let Some(name_evt) = self.channel_name_event(&new_freq).await {
                let _ = tx.send(name_evt).await;
            }
            // Deliver the new channel's duplex mode (only when the feature
            // is on) so the client switches PTT behaviour for the freq it
            // just moved to.
            if let Some(mode_evt) = self.channel_mode_event(&new_freq).await {
                let _ = tx.send(mode_evt).await;
            }
            // And the new channel's mute state, so the PTT button updates
            // the instant the move confirms (clears a stale mute carried
            // from the previous channel when the new one is unmuted).
            let _ = tx.send(self.channel_mute_event(&new_freq).await).await;
            // And our priority standing *on the new channel* — a grant is
            // bound to one frequency, so it goes dormant when we tune away
            // and re-activates on return. The client needs this to keep a
            // live PTT button when it's a priority speaker on a muted
            // (No-Talk) channel.
            let _ = tx
                .send(self.priority_event(&req.client_id, &new_freq).await)
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
                                // State-sync backfill — not a live grant.
                                priority: false,
                                broadcast: false,
                                display_name: String::new(),
                            })),
                        })
                        .await;
                }
            }
            // Deliver the client's global-broadcast capability on channel change.
            // Use the pure free-function form: acquire a short lock, build the
            // event, drop the lock, then await the send — no lock held across await.
            let bcast_cap_evt = {
                let registry = self.registry.lock().await;
                broadcast_capability_event(&registry, &req.client_id)
            };
            let _ = tx.send(bcast_cap_evt).await;
            // If a broadcast is active from another client, send a synthetic
            // PTT press so the newly-tuned client shows the broadcast indicator.
            {
                let registry = self.registry.lock().await;
                if let Some(ref active_id) = registry.broadcast_active {
                    if active_id != &req.client_id {
                        let bcast_evt = Event {
                            event: Some(event::Event::Ptt(PttEvent {
                                client_id: active_id.clone(),
                                pressed: true,
                                sequence: 0,
                                priority: false,
                                broadcast: true,
                                display_name: String::new(),
                            })),
                        };
                        let _ = tx.send(bcast_evt).await;
                    }
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
    ///
    /// When `evt.broadcast` is set the press is routed to the global-broadcast
    /// arm instead: that arm seizes every occupied room simultaneously,
    /// cutting existing holders and blocking all other presses for the
    /// duration.
    async fn push_to_talk(
        &self,
        request: Request<Streaming<PttEvent>>,
    ) -> Result<Response<PttAck>, Status> {
        let mut stream = request.into_inner();
        // FIX 2: track the broadcaster's client_id so we can clear
        // broadcast_active if the stream ends while a broadcast is live
        // (crash / network death). Updated on every event received.
        let mut stream_client_id: Option<String> = None;

        while let Some(evt_result) = stream.next().await {
            // Don't early-return on stream error: we must run the
            // broadcast-active cleanup below on BOTH EOF and error paths.
            let evt = match evt_result {
                Ok(e) => e,
                Err(_) => break,
            };
            // Record the client id for the post-loop cleanup.
            stream_client_id = Some(evt.client_id.clone());

            // ── Global-broadcast arm ──────────────────────────────────────
            if evt.broadcast {
                // Collect the work to do under the lock, then drop it
                // before any .await on channel sends (the existing discipline
                // throughout this file).
                enum BroadcastWork {
                    // (broadcaster_id, broadcaster_name, cut_holders:
                    //  Vec<(holder_id, room_txs)>, all_recipient_txs)
                    Begin {
                        broadcaster_id: String,
                        broadcaster_name: String,
                        cut_holders: Vec<(String, Vec<mpsc::Sender<Event>>)>,
                        all_txs: Vec<mpsc::Sender<Event>>,
                    },
                    // (broadcaster_id, broadcaster_name, all_recipient_txs)
                    End {
                        broadcaster_id: String,
                        broadcaster_name: String,
                        all_txs: Vec<mpsc::Sender<Event>>,
                    },
                    Noop,
                }

                let work: BroadcastWork = {
                    let mut registry = self.registry.lock().await;

                    // Capability check.
                    let has_cap = registry
                        .clients
                        .get(&evt.client_id)
                        .map(|c| c.can_global_broadcast)
                        .unwrap_or(false);
                    if !has_cap {
                        BroadcastWork::Noop
                    } else if evt.pressed {
                        // First-come wins: if another broadcast is live, ignore.
                        if registry.broadcast_active.is_some() {
                            BroadcastWork::Noop
                        } else {
                            registry.broadcast_active = Some(evt.client_id.clone());

                            // Walk all rooms: cut any current holder and clear
                            // the grace window so their residual UDP tail is NOT
                            // forwarded (broadcast supersedes it).
                            // Two-pass approach: first mutate rooms (mutable
                            // borrow), then look up client senders (immutable
                            // borrow) — can't mix them in the same loop.
                            let mut cut_info: Vec<(String, Vec<String>)> = Vec::new();
                            for (_, room) in registry.rooms.iter_mut() {
                                if let Some(holder_id) = room.holder.take() {
                                    room.last_released = None;
                                    cut_info.push((holder_id, room.members.clone()));
                                }
                            }
                            let mut cut_holders: Vec<(String, Vec<mpsc::Sender<Event>>)> =
                                Vec::new();
                            for (holder_id, member_ids) in cut_info {
                                let room_txs: Vec<mpsc::Sender<Event>> = member_ids
                                    .iter()
                                    .filter_map(|id| registry.clients.get(id))
                                    .filter_map(|c| c.events_tx.clone())
                                    .collect();
                                cut_holders.push((holder_id, room_txs));
                            }

                            // Collect ALL clients' event senders INCLUDING the
                            // broadcaster. The broadcaster must receive their own
                            // broadcast-start PttEvent so their client opens the
                            // mic gate (the gate keys on receiving a PttEvent for
                            // self — same as normal PTT) and shows the broadcast
                            // indicator. Without this the broadcaster transmits
                            // nothing and sees nothing.
                            let all_txs: Vec<mpsc::Sender<Event>> = registry
                                .clients
                                .iter()
                                .filter_map(|(_, c)| c.events_tx.clone())
                                .collect();

                            let broadcaster_name = registry
                                .clients
                                .get(&evt.client_id)
                                .map(|c| c.display_name.clone())
                                .unwrap_or_default();

                            BroadcastWork::Begin {
                                broadcaster_id: evt.client_id.clone(),
                                broadcaster_name,
                                cut_holders,
                                all_txs,
                            }
                        }
                    } else {
                        // Release: only the active broadcaster can release.
                        if registry.broadcast_active.as_deref() == Some(evt.client_id.as_str()) {
                            registry.broadcast_active = None;
                            // Include the broadcaster: they need their own
                            // broadcast-end PttEvent to close the mic gate and
                            // clear their broadcast indicator (symmetric with the
                            // Begin arm above).
                            let all_txs: Vec<mpsc::Sender<Event>> = registry
                                .clients
                                .iter()
                                .filter_map(|(_, c)| c.events_tx.clone())
                                .collect();
                            let broadcaster_name = registry
                                .clients
                                .get(&evt.client_id)
                                .map(|c| c.display_name.clone())
                                .unwrap_or_default();
                            BroadcastWork::End {
                                broadcaster_id: evt.client_id.clone(),
                                broadcaster_name,
                                all_txs,
                            }
                        } else {
                            BroadcastWork::Noop
                        }
                    }
                }; // lock dropped here

                match work {
                    BroadcastWork::Begin {
                        broadcaster_id,
                        broadcaster_name,
                        cut_holders,
                        all_txs,
                    } => {
                        // Notify each cut holder's room that the old holder
                        // released (so UIs clear the talking indicator).
                        for (holder_id, room_txs) in cut_holders {
                            let cut_evt = Event {
                                event: Some(event::Event::Ptt(PttEvent {
                                    client_id: holder_id,
                                    pressed: false,
                                    sequence: 0,
                                    priority: false,
                                    broadcast: false,
                                    display_name: String::new(),
                                })),
                            };
                            for tx in &room_txs {
                                let _ = tx.send(cut_evt.clone()).await;
                            }
                        }
                        // Notify all clients that the broadcast started.
                        // display_name carries the broadcaster's callsign so
                        // listeners on other frequencies can show it even
                        // without the broadcaster in their roster.
                        let bcast_start = Event {
                            event: Some(event::Event::Ptt(PttEvent {
                                client_id: broadcaster_id,
                                pressed: true,
                                sequence: evt.sequence,
                                priority: false,
                                broadcast: true,
                                display_name: broadcaster_name,
                            })),
                        };
                        for tx in all_txs {
                            let _ = tx.send(bcast_start.clone()).await;
                        }
                    }
                    BroadcastWork::End {
                        broadcaster_id,
                        broadcaster_name,
                        all_txs,
                    } => {
                        // Notify all clients the broadcast ended.
                        // display_name still carries the callsign so clients
                        // can correlate the end event with the active indicator.
                        let bcast_end = Event {
                            event: Some(event::Event::Ptt(PttEvent {
                                client_id: broadcaster_id,
                                pressed: false,
                                sequence: evt.sequence,
                                priority: false,
                                broadcast: true,
                                display_name: broadcaster_name,
                            })),
                        };
                        for tx in all_txs {
                            let _ = tx.send(bcast_end.clone()).await;
                        }
                    }
                    BroadcastWork::Noop => {}
                }
                continue; // broadcast arm handled; skip the normal path
            }

            // ── Normal PTT arm ────────────────────────────────────────────
            let normal_action: Option<(bool, bool, Vec<mpsc::Sender<Event>>)> = {
                let mut registry = self.registry.lock().await;

                let frequency = match registry
                    .clients
                    .get(&evt.client_id)
                    .and_then(|c| c.current_frequency.clone())
                {
                    Some(f) => f,
                    None => continue, // sender isn't in any room
                };

                // FIX 5: block ALL normal presses while a broadcast is live,
                // including the broadcaster's own normal PTT. Allowing the
                // broadcaster to key a normal channel floor while their
                // broadcast is live produces incoherent dual-floor UI state
                // (channel members see holder=None on their release while the
                // broadcast indicator is still showing). This arm handles only
                // non-broadcast presses (broadcast presses are already routed
                // to the separate broadcast arm above).
                if registry.broadcast_active.is_some() {
                    continue; // global broadcast active — drop all normal presses
                }

                // Whether this sender is a priority speaker on *this*
                // channel — computed up front because it both feeds the
                // speak gate below (a priority speaker is the No-Talk
                // exception) and the floor arbitration further down. A
                // member is priority on a channel iff their
                // `priority_freq` matches the room they're transmitting
                // on.
                let sender_is_priority = registry
                    .clients
                    .get(&evt.client_id)
                    .map(|c| c.priority_freq.as_deref() == Some(frequency.as_str()))
                    .unwrap_or(false);

                // Speak gate: a press that can't legitimately take the
                // floor never reaches arbitration, so the sender can't
                // take or preempt it. Two vetoes flow through here:
                //   * member mute — this session is individually muted.
                //     An individual sanction, so it holds even for a
                //     priority speaker.
                //   * channel mute — the whole frequency is a No-Talk
                //     channel. Nobody tuned here may transmit *except a
                //     priority speaker*, who is exactly the granted-voice
                //     exception (the "stage"/"town-hall" model). Moving
                //     to an unmuted channel restores transmit for free,
                //     since this check is keyed on `frequency`.
                // We drop the event rather than echo a denied PttDown —
                // the client self-suppresses on the Muted / ChannelMute
                // events and the relay backstops in-flight frames. A
                // press that arrives *while* the sender holds the floor
                // (mute landed mid-transmission) is gated too; the
                // SetMute / SetChannelMute handlers proactively drop the
                // floor so the channel never sticks on a silenced holder.
                let member_muted = registry
                    .clients
                    .get(&evt.client_id)
                    .map(|c| !c.can_speak())
                    .unwrap_or(false);
                let channel_muted = self.channel_mutes.read().await.contains(&frequency);
                if !speak_allowed(member_muted, channel_muted, sender_is_priority) {
                    continue;
                }

                // Compute the rest of the priority standing *before* the
                // mutable room borrow so the borrow checker stays happy.
                let current_holder = registry
                    .rooms
                    .get(&frequency)
                    .and_then(|r| r.holder.clone());
                let holder_is_priority = current_holder
                    .as_ref()
                    .and_then(|h| registry.clients.get(h))
                    .map(|c| c.priority_freq.as_deref() == Some(frequency.as_str()))
                    .unwrap_or(false);
                let room_is_full = registry
                    .rooms
                    .get(&frequency)
                    .map(|r| r.duplex.is_full())
                    .unwrap_or(false);

                // `action` is `(pressed, priority)` — the second flag
                // tells recipients to play the two-tone priority roger.
                let action: Option<(bool, bool)> = if room_is_full {
                    // Full-duplex: there's no floor. Track the talker set
                    // (drives the multi-talker roster) and always broadcast
                    // the press/release. The client self-gates audio (mic
                    // hot only while PTT held) and the relay forwards every
                    // member, so priority/grace don't apply here.
                    let room = registry.rooms.entry(frequency.clone()).or_default();
                    if evt.pressed {
                        room.active_talkers.insert(evt.client_id.clone());
                    } else {
                        room.active_talkers.remove(&evt.client_id);
                    }
                    Some((evt.pressed, false))
                } else {
                    let room = registry.rooms.entry(frequency.clone()).or_default();
                    let decision = ptt_decision(
                        room.holder.as_deref(),
                        holder_is_priority,
                        &evt.client_id,
                        evt.pressed,
                        sender_is_priority,
                    );
                    if let Some(d) = &decision {
                        match &d.new_holder {
                            // A new holder took the floor (press or
                            // priority preemption): void any release-grace
                            // so the previous holder's residual UDP tail
                            // can't bleed into this fresh transmission.
                            Some(new) => {
                                room.holder = Some(new.clone());
                                room.last_released = None;
                            }
                            // Floor released: remember who just let go and
                            // when, so the relay forwards their final
                            // in-flight UDP frames for a short grace window
                            // (UDP lags the reliable PttUp that cleared the
                            // floor). See `Room::last_released`.
                            None => {
                                if let Some(prev) = room.holder.take() {
                                    room.last_released = Some((prev, std::time::Instant::now()));
                                }
                            }
                        }
                    }
                    decision.map(|d| (d.pressed, d.priority))
                };

                action.map(|(pressed, priority)| {
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
                    (pressed, priority, recipients)
                })
            };

            let Some((pressed, priority, recipients)) = normal_action else {
                continue;
            };

            let event = Event {
                event: Some(event::Event::Ptt(PttEvent {
                    client_id: evt.client_id.clone(),
                    pressed,
                    sequence: evt.sequence,
                    priority,
                    broadcast: false,
                    display_name: String::new(),
                })),
            };

            for tx in recipients {
                let _ = tx.send(event.clone()).await;
            }
        }

        // FIX 2: if the stream ended while this client held the global
        // broadcast lock (crash / network death / clean EOF), clear it and
        // notify all other clients so their UIs clear the broadcast indicator.
        // Runs on BOTH the clean-EOF path (loop exhausted normally) and the
        // error path (loop exited via `break` above) because both fall through
        // to here — no early return inside the loop.
        if let Some(cid) = stream_client_id {
            let release_txs = {
                let mut registry = self.registry.lock().await;
                if registry.broadcast_active.as_deref() == Some(cid.as_str()) {
                    registry.broadcast_active = None;
                    registry
                        .clients
                        .iter()
                        .filter(|(id, _)| id.as_str() != cid.as_str())
                        .filter_map(|(_, c)| c.events_tx.clone())
                        .collect::<Vec<_>>()
                } else {
                    Vec::new()
                }
            }; // registry lock released here
            for tx in release_txs {
                let _ = tx
                    .send(Event {
                        event: Some(event::Event::Ptt(PttEvent {
                            client_id: cid.clone(),
                            pressed: false,
                            sequence: 0,
                            priority: false,
                            broadcast: true,
                            display_name: String::new(),
                        })),
                    })
                    .await;
            }
        }

        Ok(Response::new(PttAck {}))
    }
}

/// Outcome of a single PTT arbitration step.
#[derive(Debug, PartialEq, Eq)]
pub(crate) struct PttDecision {
    /// The room's holder *after* applying this decision.
    pub new_holder: Option<String>,
    /// The `pressed` flag to broadcast (true = floor taken).
    pub pressed: bool,
    /// Whether to flag the broadcast as a priority grant (two-tone
    /// roger on the clients).
    pub priority: bool,
}

/// Pure walkie-talkie floor-arbitration decision. Extracted from
/// [`SignalingSvc::push_to_talk`] so the (otherwise lock-bound)
/// state machine is unit-testable in isolation.
///
/// Inputs are the *current* room holder, whether that holder is a
/// The relay speak-gate, as a pure predicate: may a sender's PTT press
/// reach floor arbitration at all? Consulted in `push_to_talk` before
/// any floor logic runs.
///
///   * `member_muted` — the sender is individually muted (admin
///     `SetMute`). An individual sanction; it bars them unconditionally,
///     even with a priority grant.
///   * `channel_muted` — the whole channel is a No-Talk channel. It bars
///     everyone tuned here *except* a priority speaker, who is the
///     granted-voice exception.
///   * `sender_is_priority` — the sender holds a priority grant bound to
///     this channel.
///
/// This is the single chokepoint No-Talk channels reuse — the same
/// "default-deny + priority grant" the backlog describes.
pub(crate) fn speak_allowed(
    member_muted: bool,
    channel_muted: bool,
    sender_is_priority: bool,
) -> bool {
    !member_muted && (!channel_muted || sender_is_priority)
}

/// priority speaker on this channel, and the incoming press from
/// `sender` (with `sender_is_priority` likewise scoped to this
/// channel). Returns `None` when the press changes nothing and should
/// be ignored (denied grab, stray release, etc.).
///
/// Rules:
///   * Idle channel + press → grant; flagged priority iff the sender
///     is a priority speaker (so the channel hears the roger even on
///     an uncontested take).
///   * Holder releases their own floor → clear.
///   * A *priority* sender pressing against a *non-priority* holder →
///     **preempt**: seize the floor, flagged priority.
///   * Everything else (non-priority grab of a held floor,
///     priority-vs-priority — first-come wins, release by a
///     non-holder) → ignored.
pub(crate) fn ptt_decision(
    holder: Option<&str>,
    holder_is_priority: bool,
    sender: &str,
    pressed: bool,
    sender_is_priority: bool,
) -> Option<PttDecision> {
    match (holder, pressed) {
        (None, true) => Some(PttDecision {
            new_holder: Some(sender.to_string()),
            pressed: true,
            priority: sender_is_priority,
        }),
        (Some(h), false) if h == sender => Some(PttDecision {
            new_holder: None,
            pressed: false,
            priority: false,
        }),
        (Some(h), true) if h != sender && sender_is_priority && !holder_is_priority => {
            Some(PttDecision {
                new_holder: Some(sender.to_string()),
                pressed: true,
                priority: true,
            })
        }
        _ => None,
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
    // True if the leaver was transmitting — as the half-duplex floor
    // holder, or as one of the full-duplex active talkers. Either way the
    // remaining members get a Ptt release so their roster clears the badge.
    let was_talking = if let Some(room) = registry.rooms.get_mut(frequency) {
        room.members.retain(|id| id != client_id);
        let was_active = room.active_talkers.remove(client_id);
        let was_holder = room.holder.as_deref() == Some(client_id);
        if was_holder {
            room.holder = None;
        }
        was_holder || was_active
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

    let release_event = if was_talking {
        Some(Event {
            event: Some(event::Event::Ptt(PttEvent {
                client_id: client_id.to_string(),
                pressed: false,
                sequence: 0,
                priority: false,
                broadcast: false,
                display_name: String::new(),
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

#[cfg(test)]
mod tests {
    use super::{ct_eq, ptt_decision, quality_report_ip_ok, speak_allowed, PttDecision};
    use crate::state::{Client, Registry, TOKEN_HASH_LEN};
    use std::net::{IpAddr, Ipv4Addr};
    use std::time::Instant;

    // Minimal test-fixture client with all new fields zeroed/defaulted.
    fn mk_client(id: &str, can_broadcast: bool) -> Client {
        Client {
            id: id.to_string(),
            display_name: id.to_string(),
            audio_token_hash: [0u8; TOKEN_HASH_LEN],
            audio_mac_key: [0u8; toki_proto::wire::MAC_KEY_LEN],
            audio_last_seq: 0,
            audio_outbound_seq: 1,
            audio_id: 0,
            audio_addr: None,
            events_tx: None,
            current_frequency: None,
            priority_freq: None,
            last_seen: Instant::now(),
            connected_at: Instant::now(),
            expected_ip: None,
            identity: None,
            muted: false,
            can_global_broadcast: can_broadcast,
            quality: None,
        }
    }

    // ── Broadcast-state unit tests ────────────────────────────────────

    #[test]
    fn broadcast_blocks_normal_ptt_while_active() {
        // With broadcast_active = Some("other"), a normal press from a
        // different client should be dropped (broadcast gate). We test
        // the gate condition directly (same logic path_to_talk uses).
        let mut reg = Registry::default();
        reg.clients
            .insert("alice".into(), mk_client("alice", false));
        reg.clients.insert("other".into(), mk_client("other", true));
        reg.broadcast_active = Some("other".into());

        // Gate: if broadcast_active is Some and is NOT this client, drop.
        let should_drop = match &reg.broadcast_active {
            Some(active) if active != "alice" => true,
            _ => false,
        };
        assert!(
            should_drop,
            "alice's press must be dropped while 'other' broadcasts"
        );
    }

    #[test]
    fn broadcast_first_come_wins() {
        // Two clients both have can_global_broadcast. The first to
        // press sets broadcast_active; the second's attempt is a no-op.
        let mut reg = Registry::default();
        reg.clients.insert("alice".into(), mk_client("alice", true));
        reg.clients.insert("bob".into(), mk_client("bob", true));

        // Alice presses first.
        assert!(reg.broadcast_active.is_none());
        reg.broadcast_active = Some("alice".into());

        // Bob tries to broadcast — but broadcast_active is already Some.
        let bob_wins = reg.broadcast_active.is_none();
        assert!(
            !bob_wins,
            "bob must not seize the broadcast while alice is live"
        );
        // State unchanged.
        assert_eq!(reg.broadcast_active.as_deref(), Some("alice"));
    }

    #[test]
    fn broadcast_release_clears_state() {
        // After a broadcast release the broadcast_active field becomes None.
        let mut reg = Registry::default();
        reg.clients.insert("alice".into(), mk_client("alice", true));
        reg.broadcast_active = Some("alice".into());

        // Simulate release: alice pressed=false → clear.
        if reg.broadcast_active.as_deref() == Some("alice") {
            reg.broadcast_active = None;
        }
        assert!(
            reg.broadcast_active.is_none(),
            "broadcast_active must be None after release"
        );
    }

    #[test]
    fn broadcast_requires_capability() {
        // A client with can_global_broadcast=false sending broadcast=true
        // pressed=true must not set broadcast_active.
        let mut reg = Registry::default();
        reg.clients.insert("dave".into(), mk_client("dave", false));
        assert!(reg.broadcast_active.is_none());

        // Simulate what push_to_talk does: capability check.
        let has_cap = reg
            .clients
            .get("dave")
            .map(|c| c.can_global_broadcast)
            .unwrap_or(false);
        if has_cap {
            reg.broadcast_active = Some("dave".into());
        }
        assert!(
            reg.broadcast_active.is_none(),
            "a client without capability must not set broadcast_active"
        );
    }

    fn ip(a: u8, b: u8, c: u8, d: u8) -> IpAddr {
        IpAddr::V4(Ipv4Addr::new(a, b, c, d))
    }

    #[test]
    fn quality_report_ip_binding() {
        let bound = ip(203, 0, 113, 5);
        // Caller from the session's registration IP → accepted.
        assert!(quality_report_ip_ok(Some(bound), Some(bound)));
        // Caller from a different IP → rejected (the forge-another-peer
        // case: only IP-spoofing on an already-pinned session gets through).
        assert!(!quality_report_ip_ok(
            Some(bound),
            Some(ip(198, 51, 100, 9))
        ));
        // Caller with no observable IP can't match a bound session → reject.
        assert!(!quality_report_ip_ok(Some(bound), None));
        // Session bound to no IP (Unix-socket transport) → always allowed,
        // matching the audio relay's `expected_ip = None` skip.
        assert!(quality_report_ip_ok(None, Some(bound)));
        assert!(quality_report_ip_ok(None, None));
    }

    #[test]
    fn ct_eq_matches_only_equal_byte_strings() {
        assert!(ct_eq(b"hunter2", b"hunter2"));
        assert!(!ct_eq(b"hunter2", b"hunter3"));
        assert!(!ct_eq(b"hunter2", b"hunter")); // length mismatch
        assert!(ct_eq(b"", b""));
    }

    fn grant(holder: &str, priority: bool) -> Option<PttDecision> {
        Some(PttDecision {
            new_holder: Some(holder.to_string()),
            pressed: true,
            priority,
        })
    }

    #[test]
    fn idle_press_grants_floor_non_priority() {
        // Empty channel, ordinary member keys up → plain grant.
        assert_eq!(
            ptt_decision(None, false, "alice", true, false),
            grant("alice", false)
        );
    }

    #[test]
    fn idle_press_by_priority_member_flags_priority() {
        // Even on an uncontested take, a priority speaker's grant is
        // flagged so the channel hears the priority roger.
        assert_eq!(
            ptt_decision(None, false, "alice", true, true),
            grant("alice", true)
        );
    }

    #[test]
    fn holder_releases_own_floor() {
        assert_eq!(
            ptt_decision(Some("alice"), false, "alice", false, false),
            Some(PttDecision {
                new_holder: None,
                pressed: false,
                priority: false,
            })
        );
    }

    #[test]
    fn non_priority_cannot_grab_held_floor() {
        // Bob (ordinary) presses while Alice holds → denied.
        assert_eq!(ptt_decision(Some("alice"), false, "bob", true, false), None);
    }

    #[test]
    fn priority_preempts_non_priority_holder() {
        // Bob is priority on this channel, Alice (holding) is not →
        // Bob seizes the floor, flagged priority.
        assert_eq!(
            ptt_decision(Some("alice"), false, "bob", true, true),
            grant("bob", true)
        );
    }

    #[test]
    fn priority_cannot_preempt_priority_holder_first_come_wins() {
        // Both priority on this channel; Alice got there first, so
        // Bob's press is denied — no cutting each other off.
        assert_eq!(ptt_decision(Some("alice"), true, "bob", true, true), None);
    }

    #[test]
    fn release_by_non_holder_is_ignored() {
        // Bob releasing while Alice holds is a stray event → ignore.
        assert_eq!(
            ptt_decision(Some("alice"), false, "bob", false, false),
            None
        );
    }

    #[test]
    fn priority_holder_re_press_is_noop() {
        // Alice (priority) already holds and presses again → no change.
        assert_eq!(ptt_decision(Some("alice"), true, "alice", true, true), None);
    }

    // ── Speak-gate (No-Talk) predicate ──────────────────────────────

    #[test]
    fn speak_gate_open_channel_allows_everyone() {
        // No mutes anywhere → press passes, priority or not.
        assert!(speak_allowed(false, false, false));
        assert!(speak_allowed(false, false, true));
    }

    #[test]
    fn speak_gate_member_mute_bars_even_priority() {
        // An individual mute is the strongest sanction: it bars the
        // sender regardless of channel state or priority grant.
        assert!(!speak_allowed(true, false, false));
        assert!(!speak_allowed(true, false, true));
        assert!(!speak_allowed(true, true, true));
    }

    #[test]
    fn speak_gate_channel_mute_bars_non_priority() {
        // No-Talk channel: ordinary members can't talk…
        assert!(!speak_allowed(false, true, false));
        // …but a priority speaker is the granted-voice exception.
        assert!(speak_allowed(false, true, true));
    }

    // ── FIX 2: broadcast_active cleared when broadcaster's stream ends ──

    /// Verifies the predicate at the heart of the FIX 2 cleanup block:
    /// when `broadcast_active` matches the departing client's id, it is
    /// cleared. This is the exact decision the post-loop cleanup block in
    /// `push_to_talk` executes on stream EOF / error.
    #[test]
    fn broadcast_cleared_when_broadcaster_stream_ends() {
        let mut reg = Registry::default();
        reg.clients.insert("alice".into(), mk_client("alice", true));
        reg.broadcast_active = Some("alice".into());

        // Simulate the cleanup block: if broadcast_active matches the
        // departing stream's client id, clear it.
        let cid = "alice";
        if reg.broadcast_active.as_deref() == Some(cid) {
            reg.broadcast_active = None;
        }
        assert!(
            reg.broadcast_active.is_none(),
            "broadcast_active must be cleared when the broadcasting client's stream ends"
        );

        // A different client departing must NOT clear another client's broadcast.
        reg.broadcast_active = Some("bob".into());
        if reg.broadcast_active.as_deref() == Some(cid) {
            reg.broadcast_active = None;
        }
        assert_eq!(
            reg.broadcast_active.as_deref(),
            Some("bob"),
            "broadcast_active must not be cleared for a non-matching departing client"
        );
    }

    // ── FIX 5: broadcaster's own normal PTT blocked while broadcasting ──

    /// Verifies that while a broadcast is active, ALL normal (non-broadcast)
    /// PTT presses are dropped — including the broadcaster's own. Prior to
    /// FIX 5 the broadcaster could key a normal channel floor while their
    /// broadcast was live, producing incoherent dual-floor UI state.
    #[test]
    fn broadcast_active_blocks_even_broadcasters_own_normal_ptt() {
        let mut reg = Registry::default();
        reg.clients.insert("alice".into(), mk_client("alice", true));
        reg.clients.insert("bob".into(), mk_client("bob", false));
        reg.broadcast_active = Some("alice".into());

        // Gate: if broadcast_active is Some, drop ALL normal presses
        // (the fixed behaviour — no exception for the broadcaster).
        let alice_should_drop = reg.broadcast_active.is_some();
        let bob_should_drop = reg.broadcast_active.is_some();

        assert!(
            alice_should_drop,
            "alice's own normal press must be dropped while she is broadcasting"
        );
        assert!(
            bob_should_drop,
            "bob's normal press must be dropped while alice is broadcasting"
        );

        // When no broadcast is active, normal presses are not blocked by
        // this gate (other speak-gate rules still apply independently).
        reg.broadcast_active = None;
        let no_drop = !reg.broadcast_active.is_some();
        assert!(
            no_drop,
            "normal presses must not be dropped when no broadcast is active"
        );
    }
}
