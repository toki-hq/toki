use std::pin::Pin;

use tokio::sync::mpsc;
use tokio_stream::{Stream, StreamExt, wrappers::ReceiverStream};
use tonic::{Request, Response, Status, Streaming};
use tracing::info;
use uuid::Uuid;

use toki_proto::v1::{
    Event, JoinRequest, LeaveRequest, LeaveResponse, PttAck, PttEvent, RegisterRequest,
    RegisterResponse, event,
    signaling_server::{Signaling, SignalingServer},
};

use crate::state::{Client, SharedRegistry};

pub struct SignalingSvc {
    registry: SharedRegistry,
    audio_endpoint: String,
}

impl SignalingSvc {
    pub fn new(registry: SharedRegistry, audio_endpoint: String) -> SignalingServer<Self> {
        SignalingServer::new(Self {
            registry,
            audio_endpoint,
        })
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
        let req = request.into_inner();
        let id = Uuid::new_v4().to_string();
        let token = Uuid::new_v4().as_bytes().to_vec();

        let client = Client {
            id: id.clone(),
            display_name: req.display_name.clone(),
            audio_token: token.clone(),
            audio_addr: None,
            events_tx: None,
            joined: false,
            // Start the heartbeat clock at registration. The client will
            // refresh this within ~100 ms via its initial UDP keepalive,
            // and every 3 s thereafter.
            last_seen: std::time::Instant::now(),
        };

        let mut registry = self.registry.lock().await;
        registry.tokens.insert(token.clone(), id.clone());
        registry.clients.insert(id.clone(), client);
        let total = registry.clients.len();
        drop(registry);

        info!(
            client_id = %id,
            name = %req.display_name,
            total_clients = total,
            "client registered",
        );

        Ok(Response::new(RegisterResponse {
            client_id: id,
            audio_token: token,
            audio_endpoint: self.audio_endpoint.clone(),
        }))
    }

    async fn join(
        &self,
        request: Request<JoinRequest>,
    ) -> Result<Response<Self::JoinStream>, Status> {
        let req = request.into_inner();
        let (tx, rx) = mpsc::channel::<Event>(64);

        let mut registry = self.registry.lock().await;

        let display_name = {
            let client = registry
                .clients
                .get_mut(&req.client_id)
                .ok_or_else(|| Status::not_found("unknown client"))?;
            client.events_tx = Some(tx.clone());
            client.joined = true;
            client.display_name.clone()
        };

        let (other_ids, current_holder, total_members) = {
            let room = &mut registry.room;
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
            members = total_members,
            "client joined room",
        );

        let join_event = Event {
            event: Some(event::Event::Joined(toki_proto::v1::MemberJoined {
                client_id: req.client_id.clone(),
                display_name,
            })),
        };

        // Backfill the new joiner with the existing roster…
        for id in &other_ids {
            if let Some(existing) = registry.clients.get(id) {
                let backfill = Event {
                    event: Some(event::Event::Joined(toki_proto::v1::MemberJoined {
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

        // Announce the new joiner to existing members.
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

        let (recipients, left_event, release_event, display_name, remaining) = {
            let mut registry = self.registry.lock().await;

            // Remove the leaver from the room, and detect whether they
            // were holding the PTT lock (so we can broadcast a release).
            let was_holder = {
                let room = &mut registry.room;
                room.members.retain(|id| id != &req.client_id);
                if room.holder.as_deref() == Some(req.client_id.as_str()) {
                    room.holder = None;
                    true
                } else {
                    false
                }
            };

            let display_name = registry
                .clients
                .get(&req.client_id)
                .map(|c| c.display_name.clone())
                .unwrap_or_else(|| req.client_id.clone());

            if let Some(client) = registry.clients.get_mut(&req.client_id) {
                client.joined = false;
            }

            let member_ids: Vec<String> = registry.room.members.clone();
            let remaining = member_ids.len();

            let recipients: Vec<mpsc::Sender<Event>> = member_ids
                .iter()
                .filter_map(|id| registry.clients.get(id))
                .filter_map(|c| c.events_tx.clone())
                .collect();

            let left_event = Event {
                event: Some(event::Event::Left(toki_proto::v1::MemberLeft {
                    client_id: req.client_id.clone(),
                })),
            };

            let release_event = if was_holder {
                Some(Event {
                    event: Some(event::Event::Ptt(PttEvent {
                        client_id: req.client_id.clone(),
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
        };

        info!(
            client_id = %req.client_id,
            name = %display_name,
            members = remaining,
            "client left room",
        );

        for tx in &recipients {
            let _ = tx.send(left_event.clone()).await;
            if let Some(release) = &release_event {
                let _ = tx.send(release.clone()).await;
            }
        }

        Ok(Response::new(LeaveResponse {}))
    }

    /// Walkie-talkie arbitration. Only PTT events that change room state
    /// are broadcast:
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

                let action = {
                    let room = &mut registry.room;
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
                    let member_ids: Vec<String> = registry.room.members.clone();
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
