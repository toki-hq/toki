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

        let members = registry.channels.entry(req.channel.clone()).or_default();
        if !members.contains(&req.client_id) {
            members.push(req.client_id.clone());
        }
        let other_ids: Vec<String> = members.iter().filter(|id| *id != &req.client_id).cloned().collect();

        let join_event = ChannelEvent {
            event: Some(channel_event::Event::Joined(toki_proto::v1::MemberJoined {
                client_id: req.client_id.clone(),
                display_name,
            })),
        };

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
        let mut registry = self.registry.lock().await;

        if let Some(members) = registry.channels.get_mut(&req.channel) {
            members.retain(|id| id != &req.client_id);
        }
        if let Some(client) = registry.clients.get_mut(&req.client_id) {
            client.channels.retain(|c| c != &req.channel);
        }

        let left_event = ChannelEvent {
            event: Some(channel_event::Event::Left(toki_proto::v1::MemberLeft {
                client_id: req.client_id.clone(),
            })),
        };
        let member_ids: Vec<String> = registry
            .channels
            .get(&req.channel)
            .cloned()
            .unwrap_or_default();
        for id in member_ids {
            if let Some(other) = registry.clients.get(&id) {
                if let Some(tx) = &other.events_tx {
                    let _ = tx.send(left_event.clone()).await;
                }
            }
        }

        Ok(Response::new(LeaveChannelResponse {}))
    }

    async fn push_to_talk(
        &self,
        request: Request<Streaming<PttEvent>>,
    ) -> Result<Response<PttAck>, Status> {
        let mut stream = request.into_inner();
        while let Some(evt) = stream.next().await {
            let evt = evt?;
            let event = ChannelEvent {
                event: Some(channel_event::Event::Ptt(evt.clone())),
            };
            let registry = self.registry.lock().await;
            if let Some(members) = registry.channels.get(&evt.channel) {
                for id in members {
                    if id == &evt.client_id {
                        continue;
                    }
                    if let Some(other) = registry.clients.get(id) {
                        if let Some(tx) = &other.events_tx {
                            let _ = tx.send(event.clone()).await;
                        }
                    }
                }
            }
        }
        Ok(Response::new(PttAck {}))
    }
}
