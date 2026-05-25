//! Tokio runtime owning gRPC signaling + UDP audio I/O.
//!
//! The GUI thread sends `Cmd`s via an unbounded channel; the runtime owns
//! the active `Session` (if any) and updates `SharedState` so the GUI can
//! render it.

use std::net::SocketAddr;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::time::Duration;

use anyhow::{Context, Result, anyhow};
use futures::StreamExt;
use tokio::net::UdpSocket;
use tokio::sync::mpsc::{self, UnboundedReceiver, UnboundedSender};
use tokio::task::JoinHandle;
use tokio_stream::wrappers::ReceiverStream;
use tonic::transport::Channel;
use tracing::{info, warn};

use toki_proto::v1::{
    JoinChannelRequest, LeaveChannelRequest, PttEvent, RegisterRequest,
    channel_event::Event as ChEvent, signaling_client::SignalingClient,
};
use toki_proto::wire::{
    HEADER_LEN, MAX_AUDIO_PACKET, VERSION_AUDIO_PCM, VERSION_KEEPALIVE,
};

use crate::audio::{self, AudioHandle, PlaybackBuf, push_playback};
use crate::state::{ConnState, SharedState};

pub enum Cmd {
    Connect {
        server: String,
        display_name: String,
        channel: String,
    },
    Disconnect,
    PttDown,
    PttUp,
}

/// Spawn the runtime thread and return the command channel.
pub fn spawn(state: SharedState) -> UnboundedSender<Cmd> {
    let (cmd_tx, cmd_rx) = mpsc::unbounded_channel();
    std::thread::Builder::new()
        .name("toki-runtime".into())
        .spawn(move || {
            let rt = match tokio::runtime::Builder::new_multi_thread()
                .enable_all()
                .build()
            {
                Ok(rt) => rt,
                Err(e) => {
                    state.lock().unwrap().log(format!("runtime init failed: {e}"));
                    return;
                }
            };
            rt.block_on(run(cmd_rx, state));
        })
        .expect("spawn runtime thread");
    cmd_tx
}

async fn run(mut cmd_rx: UnboundedReceiver<Cmd>, state: SharedState) {
    // Audio I/O is started once and runs for the lifetime of the program;
    // captured frames are routed to UDP only while a session is active and
    // PTT is held, and incoming UDP audio is mixed into the playback ring
    // unconditionally.
    let AudioHandle {
        mut mic_rx,
        playback,
    } = match audio::spawn() {
        Ok(h) => h,
        Err(e) => {
            state.lock().unwrap().log(format!("audio init failed: {e}"));
            return;
        }
    };

    let mut session: Option<Session> = None;

    loop {
        tokio::select! {
            cmd = cmd_rx.recv() => {
                let Some(cmd) = cmd else { break; };
                handle_cmd(cmd, &mut session, &state, &playback).await;
            }
            frame = mic_rx.recv() => {
                let Some(frame) = frame else { break; };
                if let Some(s) = &session {
                    if s.ptt.load(Ordering::Relaxed) {
                        s.send_audio(&frame).await;
                    }
                }
            }
        }
    }
}

async fn handle_cmd(
    cmd: Cmd,
    session: &mut Option<Session>,
    state: &SharedState,
    playback: &PlaybackBuf,
) {
    match cmd {
        Cmd::Connect {
            server,
            display_name,
            channel,
        } => {
            if session.is_some() {
                state.lock().unwrap().log("already connected");
                return;
            }
            state.lock().unwrap().connection = ConnState::Connecting;
            match Session::open(&server, &display_name, &channel, state.clone(), playback.clone()).await {
                Ok(s) => {
                    {
                        let mut st = state.lock().unwrap();
                        st.connection = ConnState::Connected;
                        st.log(format!("connected as {display_name} in #{channel}"));
                    }
                    *session = Some(s);
                }
                Err(e) => {
                    let mut st = state.lock().unwrap();
                    st.connection = ConnState::Failed(e.to_string());
                    st.log(format!("connect failed: {e}"));
                }
            }
        }
        Cmd::Disconnect => {
            if let Some(s) = session.take() {
                s.close().await;
                let mut st = state.lock().unwrap();
                st.connection = ConnState::Disconnected;
                st.members.clear();
                st.speaking.clear();
                st.self_id = None;
                st.transmitting = false;
                st.log("disconnected");
            }
        }
        Cmd::PttDown => {
            if let Some(s) = session {
                s.set_ptt(true).await;
                state.lock().unwrap().transmitting = true;
            }
        }
        Cmd::PttUp => {
            if let Some(s) = session {
                s.set_ptt(false).await;
                state.lock().unwrap().transmitting = false;
            }
        }
    }
}

struct Session {
    client_id: String,
    channel: String,
    audio_token: Vec<u8>,
    ptt: Arc<AtomicBool>,
    seq: AtomicU64,
    ptt_tx: mpsc::Sender<PttEvent>,
    udp: Arc<UdpSocket>,
    signaling: SignalingClient<Channel>,
    tasks: Vec<JoinHandle<()>>,
}

impl Session {
    async fn open(
        server: &str,
        display_name: &str,
        channel: &str,
        state: SharedState,
        playback: PlaybackBuf,
    ) -> Result<Self> {
        // Accept either "host:port" or a full URL.
        let server_url = if server.starts_with("http://") || server.starts_with("https://") {
            server.to_string()
        } else {
            format!("http://{server}")
        };

        let mut signaling = SignalingClient::connect(server_url.clone())
            .await
            .with_context(|| format!("connect {server_url}"))?;

        let reg = signaling
            .register(RegisterRequest {
                display_name: display_name.into(),
            })
            .await?
            .into_inner();

        let client_id = reg.client_id;
        let audio_token = reg.audio_token;
        let audio_addr = resolve_audio_endpoint(&reg.audio_endpoint, &server_url)?;

        {
            let mut st = state.lock().unwrap();
            st.self_id = Some(client_id.clone());
            // Show ourselves in the roster immediately — the server doesn't
            // echo our own MemberJoined back to us.
            st.members.insert(client_id.clone(), display_name.to_string());
        }

        // ── UDP socket ────────────────────────────────────────────────
        let udp = Arc::new(UdpSocket::bind("0.0.0.0:0").await?);
        udp.connect(audio_addr).await?;
        // Immediately punch a hole: server records our source addr so
        // peers' audio can find us before we've ever transmitted.
        send_keepalive(&udp, &audio_token).await?;
        info!(?audio_addr, "udp audio connected");

        // ── Channel event stream (server → us) ────────────────────────
        let events_resp = signaling
            .join_channel(JoinChannelRequest {
                client_id: client_id.clone(),
                channel: channel.to_string(),
            })
            .await?;
        let mut events = events_resp.into_inner();
        let state_for_events = state.clone();
        let self_id_for_events = client_id.clone();
        let events_task = tokio::spawn(async move {
            while let Some(evt) = events.next().await {
                match evt {
                    Ok(ce) => match ce.event {
                        Some(ChEvent::Joined(j)) => {
                            let mut st = state_for_events.lock().unwrap();
                            st.members.insert(j.client_id.clone(), j.display_name.clone());
                            st.log(format!("→ {} joined", j.display_name));
                        }
                        Some(ChEvent::Left(l)) => {
                            let mut st = state_for_events.lock().unwrap();
                            let name = st
                                .members
                                .remove(&l.client_id)
                                .unwrap_or_else(|| l.client_id.clone());
                            st.speaking.remove(&l.client_id);
                            st.log(format!("← {name} left"));
                        }
                        Some(ChEvent::Ptt(p)) => {
                            if p.client_id == self_id_for_events {
                                continue;
                            }
                            let mut st = state_for_events.lock().unwrap();
                            if p.pressed {
                                st.speaking.insert(p.client_id);
                            } else {
                                st.speaking.remove(&p.client_id);
                            }
                        }
                        None => {}
                    },
                    Err(e) => {
                        warn!(error = %e, "channel event stream error");
                        break;
                    }
                }
            }
            info!("channel event stream closed");
        });

        // ── PTT outbound stream (us → server) ─────────────────────────
        let (ptt_tx, ptt_rx) = mpsc::channel::<PttEvent>(16);
        let ptt_stream = ReceiverStream::new(ptt_rx);
        let mut signaling_for_ptt = signaling.clone();
        let ptt_task = tokio::spawn(async move {
            if let Err(e) = signaling_for_ptt.push_to_talk(ptt_stream).await {
                warn!(error = %e, "push_to_talk stream ended");
            }
        });

        // ── UDP recv → playback ───────────────────────────────────────
        let udp_for_recv = udp.clone();
        let recv_task = tokio::spawn(async move {
            let mut buf = vec![0u8; MAX_AUDIO_PACKET];
            loop {
                match udp_for_recv.recv(&mut buf).await {
                    Ok(0) => continue,
                    Ok(n) => {
                        // Server forwards just the PCM payload — decode and mix.
                        let samples = pcm_from_bytes(&buf[..n]);
                        push_playback(&playback, &samples);
                    }
                    Err(e) => {
                        warn!(error = %e, "udp recv error");
                        break;
                    }
                }
            }
        });

        // ── Keepalives ────────────────────────────────────────────────
        let udp_for_keepalive = udp.clone();
        let token_for_keepalive = audio_token.clone();
        let keepalive_task = tokio::spawn(async move {
            let mut tick = tokio::time::interval(Duration::from_secs(3));
            tick.tick().await; // consume the immediate first tick
            loop {
                tick.tick().await;
                if let Err(e) = send_keepalive(&udp_for_keepalive, &token_for_keepalive).await {
                    warn!(error = %e, "keepalive failed");
                    break;
                }
            }
        });

        Ok(Self {
            client_id,
            channel: channel.to_string(),
            audio_token,
            ptt: Arc::new(AtomicBool::new(false)),
            seq: AtomicU64::new(0),
            ptt_tx,
            udp,
            signaling,
            tasks: vec![events_task, ptt_task, recv_task, keepalive_task],
        })
    }

    async fn set_ptt(&self, pressed: bool) {
        self.ptt.store(pressed, Ordering::Relaxed);
        let seq = self.seq.fetch_add(1, Ordering::Relaxed);
        let evt = PttEvent {
            client_id: self.client_id.clone(),
            channel: self.channel.clone(),
            pressed,
            sequence: seq,
        };
        if let Err(e) = self.ptt_tx.send(evt).await {
            warn!(error = %e, "ptt send failed");
        }
    }

    async fn send_audio(&self, samples: &[i16]) {
        let mut pkt = Vec::with_capacity(HEADER_LEN + samples.len() * 2);
        pkt.extend_from_slice(&self.audio_token);
        pkt.push(VERSION_AUDIO_PCM);
        for &s in samples {
            pkt.extend_from_slice(&s.to_le_bytes());
        }
        if let Err(e) = self.udp.send(&pkt).await {
            warn!(error = %e, "udp send failed");
        }
    }

    async fn close(mut self) {
        let _ = self
            .signaling
            .leave_channel(LeaveChannelRequest {
                client_id: self.client_id.clone(),
                channel: self.channel.clone(),
            })
            .await;
        for t in &self.tasks {
            t.abort();
        }
    }
}

async fn send_keepalive(udp: &UdpSocket, token: &[u8]) -> Result<()> {
    let mut pkt = Vec::with_capacity(HEADER_LEN);
    pkt.extend_from_slice(token);
    pkt.push(VERSION_KEEPALIVE);
    udp.send(&pkt).await?;
    Ok(())
}

fn pcm_from_bytes(bytes: &[u8]) -> Vec<i16> {
    bytes
        .chunks_exact(2)
        .map(|c| i16::from_le_bytes([c[0], c[1]]))
        .collect()
}

/// The server may advertise its audio endpoint as `0.0.0.0:port`, which
/// isn't routable from a client. When that happens, substitute the host
/// portion of the signaling URL.
fn resolve_audio_endpoint(advertised: &str, signaling_url: &str) -> Result<SocketAddr> {
    let parsed: SocketAddr = advertised
        .parse()
        .with_context(|| format!("parse audio endpoint {advertised:?}"))?;
    if !parsed.ip().is_unspecified() {
        return Ok(parsed);
    }
    let host = signaling_url
        .trim_start_matches("http://")
        .trim_start_matches("https://")
        .split('/')
        .next()
        .ok_or_else(|| anyhow!("empty signaling url"))?
        .rsplit_once(':')
        .map(|(host, _port)| host)
        .ok_or_else(|| anyhow!("signaling url missing port"))?;
    format!("{host}:{}", parsed.port())
        .parse()
        .with_context(|| format!("resolve {host}:{}", parsed.port()))
}
