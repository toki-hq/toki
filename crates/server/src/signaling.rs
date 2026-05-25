use std::pin::Pin;

use tokio::sync::mpsc;
use tokio_stream::{Stream, StreamExt, wrappers::ReceiverStream};
use tonic::{Request, Response, Status, Streaming};
use uuid::Uuid;

use toki_proto::v1::{
    ChannelEvent, JoinChannelRequest, LeaveChannelRequest, LeaveChannelResponse, PttAck, PttEvent,
    RegisterRequest, RegisterResponse, channel_event,
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

type EventStream = Pin<Box<dyn Stream<Item = Result<ChannelEvent, Status>> + Send>>;

#[tonic::async_trait]
impl Signaling for SignalingSvc {
    type JoinChannelStream = EventStream;

    async fn register(
        &self,
        request: Request<RegisterRequest>,
    ) -> Result<Response<RegisterResponse>, Status> {
        let req = request.into_inner();
        let id = Uuid::new_v4().to_string();
        let token = Uuid::new_v4().as_bytes().to_vec();

        let client = Client {
            id: id.clone(),
            display_name: req.display_name,
            audio_token: token.clone(),
            audio_addr: None,
            events_tx: None,
            channels: Vec::new(),
            // Start the heartbeat clock at registration. The client will
            // refresh this within ~100 ms via its initial UDP keepalive,
            // and every 3 s thereafter.
            last_seen: std::time::Instant::now(),
        };

        let mut registry = self.registry.lock().await;
        registry.tokens.insert(token.clone(), id.clone());
        registry.clients.insert(id.clone(), client);

        Ok(Response::new(RegisterResponse {
            client_id: id,
            audio_token: token,
            audio_endpoint: self.audio_endpoint.clone(),
        }))
    }

    async fn join_channel(
        &self,
        request: Request<JoinChannelRequest>,
    ) -> Result<Response<Self::JoinChannelStream>, Status> {
        let req = request.into_inner();
        let (tx, rx) = mpsc::channel::<ChannelEvent>(64);

        let mut registry = self.registry.lock().await;

        let display_name = {
            let client = registry
                .clients
                .get_mut(&req.client_id)
                .ok_or_else(|| Status::not_found("unknown client"))?;
            client.events_tx = Some(tx.clone());
            if !client.channels.contains(&req.channel) {
                client.channels.push(req.channel.clone());
            }
            client.display_name.clone()
        };

        let (other_ids, current_holder) = {
            let channel = registry.channels.entry(req.channel.clone()).or_default();
            if !channel.members.contains(&req.client_id) {
                channel.members.push(req.client_id.clone());
            }
            let others: Vec<String> = channel
                .members
                .iter()
                .filter(|id| *id != &req.client_id)
                .cloned()
                .collect();
            (others, channel.holder.clone())
        };

        let join_event = ChannelEvent {
            event: Some(channel_event::Event::Joined(toki_proto::v1::MemberJoined {
                client_id: req.client_id.clone(),
                display_name,
            })),
        };

        // Backfill the new joiner with the existing roster…
        for id in &other_ids {
            if let Some(existing) = registry.clients.get(id) {
                let backfill = ChannelEvent {
                    event: Some(channel_event::Event::Joined(toki_proto::v1::MemberJoined {
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
                let backfill = ChannelEvent {
                    event: Some(channel_event::Event::Ptt(PttEvent {
                        client_id: holder_id,
                        channel: req.channel.clone(),
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
        Ok(Response::new(Box::pin(stream) as Self::JoinChannelStream))
    }

    async fn leave_channel(
        &self,
        request: Request<LeaveChannelRequest>,
    ) -> Result<Response<LeaveChannelResponse>, Status> {
        let req = request.into_inner();

        let (recipients, left_event, release_event) = {
            let mut registry = self.registry.lock().await;

            // Remove the leaver from the channel, and detect whether they
            // were holding the PTT lock (so we can broadcast a release).
            let was_holder = if let Some(ch) = registry.channels.get_mut(&req.channel) {
                ch.members.retain(|id| id != &req.client_id);
                if ch.holder.as_deref() == Some(req.client_id.as_str()) {
                    ch.holder = None;
                    true
                } else {
                    false
                }
            } else {
                false
            };

            if let Some(client) = registry.clients.get_mut(&req.client_id) {
                client.channels.retain(|c| c != &req.channel);
            }

            let member_ids: Vec<String> = registry
                .channels
                .get(&req.channel)
                .map(|c| c.members.clone())
                .unwrap_or_default();

            let recipients: Vec<mpsc::Sender<ChannelEvent>> = member_ids
                .iter()
                .filter_map(|id| registry.clients.get(id))
                .filter_map(|c| c.events_tx.clone())
                .collect();

            let left_event = ChannelEvent {
                event: Some(channel_event::Event::Left(toki_proto::v1::MemberLeft {
                    client_id: req.client_id.clone(),
                })),
            };

            let release_event = if was_holder {
                Some(ChannelEvent {
                    event: Some(channel_event::Event::Ptt(PttEvent {
                        client_id: req.client_id.clone(),
                        channel: req.channel.clone(),
                        pressed: false,
                        sequence: 0,
                    })),
                })
            } else {
                None
            };

            (recipients, left_event, release_event)
        };

        for tx in &recipients {
            let _ = tx.send(left_event.clone()).await;
            if let Some(release) = &release_event {
                let _ = tx.send(release.clone()).await;
            }
        }

        Ok(Response::new(LeaveChannelResponse {}))
    }

    /// Walkie-talkie arbitration. Only PTT events that change channel state
    /// are broadcast:
    ///   - `pressed = true` is granted iff no one currently holds the channel.
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

            let broadcast: Option<(bool, Vec<mpsc::Sender<ChannelEvent>>)> = {
                let mut registry = self.registry.lock().await;

                let action = {
                    let channel = registry.channels.entry(evt.channel.clone()).or_default();
                    match (channel.holder.as_deref(), evt.pressed) {
                        (None, true) => {
                            channel.holder = Some(evt.client_id.clone());
                            Some(true)
                        }
                        (Some(h), false) if h == evt.client_id => {
                            channel.holder = None;
                            Some(false)
                        }
                        _ => None,
                    }
                };

                action.map(|pressed| {
                    let member_ids: Vec<String> = registry
                        .channels
                        .get(&evt.channel)
                        .map(|c| c.members.clone())
                        .unwrap_or_default();
                    let recipients: Vec<mpsc::Sender<ChannelEvent>> = member_ids
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

            let event = ChannelEvent {
                event: Some(channel_event::Event::Ptt(PttEvent {
                    client_id: evt.client_id.clone(),
                    channel: evt.channel.clone(),
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
