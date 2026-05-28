//! Tokio runtime owning gRPC signaling + UDP audio I/O.
//!
//! The GUI thread sends `Cmd`s via an unbounded channel; the runtime owns
//! the active `Session` (if any) and updates `SharedState` so the GUI can
//! render it.

use std::net::SocketAddr;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::Arc;
use std::sync::Mutex as StdMutex;
use std::time::{Duration, Instant};

use anyhow::{anyhow, Context, Result};
use futures::StreamExt;
use tokio::net::UdpSocket;
use tokio::sync::mpsc::{self, UnboundedReceiver, UnboundedSender};
use tokio::task::JoinHandle;
use tokio_stream::wrappers::ReceiverStream;
use tonic::transport::Channel;
use tracing::{info, warn};

use chacha20poly1305::{
    aead::{generic_array::GenericArray, AeadInPlace},
    ChaCha20Poly1305, Key, KeyInit, Nonce, Tag,
};
use toki_proto::v1::{
    event::Event as Ev, signaling_client::SignalingClient, ChangeFrequencyRequest, JoinRequest,
    LeaveRequest, PttEvent, RegisterRequest,
};
use toki_proto::wire::{
    build_nonce, HEADER_LEN_C2S, HEADER_LEN_S2C, MAX_AUDIO_PACKET, SEQ_LEN, TAG_LEN,
    VERSION_AUDIO_PCM, VERSION_KEEPALIVE,
};

use crate::audio::{self, push_playback, BeepParams, PlaybackBuf};
use crate::state::{ConnState, SharedState};

pub enum Cmd {
    Connect {
        server: String,
        display_name: String,
        frequency: String,
        /// Shared-secret password for servers running in
        /// password-gated mode. Empty string when the user hasn't
        /// configured one; the server ignores it in open mode.
        password: String,
    },
    Disconnect,
    /// Graceful-shutdown variant of [`Cmd::Disconnect`]. Same effect
    /// (sends `Leave` and aborts session tasks), but signals
    /// completion back to the caller through an `mpsc::Sender` so
    /// the GUI thread's `on_exit` hook can block until the Leave
    /// RPC has actually landed before tearing the process down.
    /// Without this, closing the app while connected leaves the
    /// server's reaper to time us out ~10s later — peers see a
    /// silent ghost instead of an immediate `MemberLeft`.
    Shutdown(std::sync::mpsc::Sender<()>),
    /// Leave the current frequency room *without* dropping the session.
    /// Used as the first half of a debounced frequency change — the
    /// UI fires this on the user's first chevron click so they "go off
    /// the air" immediately, then sends [`Cmd::ChangeFrequency`] once
    /// the chevron clicks settle.
    LeaveRoom,
    /// Move to a different frequency room. Server emits MemberLeft on
    /// the old room and MemberJoined backfill on the new room, all on
    /// the existing event stream. Safe to call when the client is
    /// currently roomless (the post-`LeaveRoom` state).
    ChangeFrequency(String),
    PttDown,
    PttUp,
    /// Preview a beep with the current [`BeepParams`] values. No gRPC
    /// traffic and no session required — used by the Settings TEST
    /// buttons so the user can audition tone tweaks without having
    /// to actually press PTT.
    TestBeep(BeepKind),
}

/// Discriminator for which beep variant a request applies to. The
/// runtime maps this to the matching pair of (frequency, duration,
/// volume) values from [`BeepParams`] before synthesising the tone.
#[derive(Clone, Copy, Debug)]
pub enum BeepKind {
    /// Tone played when *someone* takes the floor (including us).
    /// Defaults to the "up" cue at 1200 Hz.
    Acquire,
    /// Tone played when the holder releases the floor. Defaults to
    /// the "down" cue at 800 Hz.
    Release,
}

/// Spawn the runtime thread and return the command channel. The caller
/// has already spawned the audio thread; we just receive the mic frames
/// and write into the playback ring as voice arrives.
pub fn spawn(
    state: SharedState,
    mic_rx: UnboundedReceiver<Vec<i16>>,
    playback: PlaybackBuf,
    beeps: BeepParams,
) -> UnboundedSender<Cmd> {
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
                    state
                        .lock()
                        .unwrap()
                        .log(format!("runtime init failed: {e}"));
                    return;
                }
            };
            rt.block_on(run(cmd_rx, state, mic_rx, playback, beeps));
        })
        .expect("spawn runtime thread");
    cmd_tx
}

async fn run(
    mut cmd_rx: UnboundedReceiver<Cmd>,
    state: SharedState,
    mut mic_rx: UnboundedReceiver<Vec<i16>>,
    playback: PlaybackBuf,
    beeps: BeepParams,
) {
    let mut session: Option<Session> = None;

    loop {
        tokio::select! {
            cmd = cmd_rx.recv() => {
                let Some(cmd) = cmd else { break; };
                // handle_cmd returns false on a clean shutdown command
                // (e.g. Cmd::Shutdown). We break out of the select loop
                // then so the runtime thread can exit promptly rather
                // than sit waiting on a dead UI.
                if !handle_cmd(cmd, &mut session, &state, &playback, &beeps).await {
                    break;
                }
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

/// Dispatch a single command. Returns `true` to keep the runtime
/// loop going, `false` to terminate it cleanly (used by
/// [`Cmd::Shutdown`] so the egui thread can guarantee a graceful
/// goodbye before the process exits).
async fn handle_cmd(
    cmd: Cmd,
    session: &mut Option<Session>,
    state: &SharedState,
    playback: &PlaybackBuf,
    beeps: &BeepParams,
) -> bool {
    match cmd {
        Cmd::Connect {
            server,
            display_name,
            frequency,
            password,
        } => {
            if session.is_some() {
                state.lock().unwrap().log("already connected");
                // Already-connected is a soft no-op, not a shutdown
                // signal — keep the runtime loop running.
                return true;
            }
            state.lock().unwrap().connection = ConnState::Connecting;
            match Session::open(
                &server,
                &display_name,
                &frequency,
                &password,
                state.clone(),
                playback.clone(),
                beeps.clone(),
            )
            .await
            {
                Ok(s) => {
                    {
                        let mut st = state.lock().unwrap();
                        st.connection = ConnState::Connected;
                        st.frequency = Some(frequency.clone());
                        st.log(format!("connected as {display_name} on {frequency} MHz"));
                    }
                    *session = Some(s);
                }
                Err(e) => {
                    let mut st = state.lock().unwrap();
                    // `{:#}` walks the full anyhow error chain so the
                    // root cause lands in the on-screen log instead
                    // of just the top-level "transport error" wrapper.
                    let full = format!("{e:#}");
                    st.connection = ConnState::Failed(full.clone());
                    st.log(format!("connect failed: {full}"));
                    tracing::error!(error = ?e, "connect attempt failed");
                }
            }
        }
        Cmd::Disconnect => {
            if let Some(s) = session.take() {
                s.close().await;
                let mut st = state.lock().unwrap();
                st.connection = ConnState::Disconnected;
                st.members.clear();
                st.holder = None;
                st.self_id = None;
                st.frequency = None;
                st.log("disconnected");
            }
        }
        Cmd::Shutdown(ack) => {
            // App is quitting. Close the session (sends Leave +
            // aborts tasks) if there is one, then signal the egui
            // thread so it knows the Leave RPC has landed and the
            // process can tear down. We don't touch `state` here
            // because the UI thread is already on its way out — any
            // log line we'd append would never get rendered.
            if let Some(s) = session.take() {
                s.close().await;
            }
            let _ = ack.send(());
            // Returning `false` tells run()'s loop to exit. Any
            // further commands queued behind Shutdown are by
            // definition post-quit garbage; processing them would
            // just delay the runtime thread's teardown.
            return false;
        }
        Cmd::LeaveRoom => {
            if let Some(s) = session {
                s.leave_room(state).await;
            }
        }
        Cmd::ChangeFrequency(freq) => {
            if let Some(s) = session {
                s.change_frequency(&freq, state).await;
            }
        }
        Cmd::PttDown => {
            if let Some(s) = session {
                s.request_ptt(true).await;
            }
        }
        Cmd::PttUp => {
            if let Some(s) = session {
                s.request_ptt(false).await;
            }
        }
        Cmd::TestBeep(kind) => {
            // Preview tones don't require an active session — they
            // just synthesise audio with the current BeepParams and
            // push it onto the playback ring. The audio thread takes
            // it from there.
            let preset = beeps.current_preset();
            let steps = match kind {
                BeepKind::Acquire => preset.acquire.steps,
                BeepKind::Release => preset.release.steps,
            };
            let tone = audio::beep_pattern(steps, beeps.volume());
            push_playback(playback, &tone);
        }
    }
    // Default: every command except Shutdown leaves the runtime running.
    true
}

/// How long we ignore a fresh `PttDown` after the previous `PttUp`.
/// Stops accidental double-presses, bouncy keys, and deliberate
/// spam from producing a join/leave storm on the server (and a
/// "TOKI-0 took the floor / TOKI-0 cleared / TOKI-0 took the floor"
/// log spam for every other client in the room).
///
/// 250 ms is below the human "rapid tap" threshold (~120 ms cycle)
/// but well above any plausible mechanical bounce, so legitimate
/// "quick acknowledgment" presses still work back-to-back.
const PTT_COOLDOWN: Duration = Duration::from_millis(250);

struct Session {
    client_id: String,
    audio_token: Vec<u8>,
    /// Symmetric BLAKE3-keyed-hash key handed back by the server in
    /// `RegisterResponse.audio_mac_key`. Used to MAC every outbound
    /// UDP packet (audio + keepalives) so the server can reject
    /// forged / off-path injections. See `toki_proto::wire` for the
    /// header layout.
    audio_mac_key: [u8; toki_proto::wire::MAC_KEY_LEN],
    /// Monotonic per-session UDP sequence counter. Used by the
    /// server for strict-monotonic replay protection. We start at
    /// 1 so the initial keepalive beats the server's initial
    /// audio_last_seq = 0. Wrapped in Arc so the keepalive task can
    /// share ownership without inheriting the whole Session.
    udp_seq: Arc<AtomicU64>,
    ptt: Arc<AtomicBool>,
    seq: AtomicU64,
    ptt_tx: mpsc::Sender<PttEvent>,
    /// Client-side PTT-spam guard. `local_pressed` tracks whether
    /// we've already sent a `PttDown` that hasn't been matched by a
    /// `PttUp` yet — used to drop duplicate downs and orphan ups.
    /// `cooldown_until` is the earliest `Instant` at which a fresh
    /// `PttDown` is allowed; set whenever we send a `PttUp`.
    local_pressed: AtomicBool,
    cooldown_until: StdMutex<Option<Instant>>,
    udp: Arc<UdpSocket>,
    signaling: SignalingClient<Channel>,
    tasks: Vec<JoinHandle<()>>,
}

impl Session {
    async fn open(
        server: &str,
        display_name: &str,
        frequency: &str,
        password: &str,
        state: SharedState,
        playback: PlaybackBuf,
        beeps: BeepParams,
    ) -> Result<Self> {
        // gRPC is always TLS. Any URL scheme other than https:// is
        // rewritten so plaintext can't sneak back in via a stale
        // config or a bare "host:port" string. Tonic 0.13's
        // `ClientTlsConfig` doesn't expose a way to install a custom
        // rustls verifier, so we build a TLS-aware connector
        // ourselves and hand it to `connect_with_connector` — see
        // `insecure_tls_config` for what the verifier does and why
        // it's safe in this threat model.
        //
        // Two URLs in play deliberately:
        //   * `display_url` is `https://…` — what we log and what the
        //     user sees; reflects reality.
        //   * `endpoint_url` is `http://…` — what we hand to Tonic's
        //     `Endpoint::from_shared`. Tonic 0.13's
        //     `connect_with_connector` rejects `https://` URIs unless
        //     the endpoint *also* has a `ClientTlsConfig` set, and
        //     setting one would make Tonic try to wrap the stream
        //     our connector returns in a *second* TLS handshake. By
        //     passing `http://`, Tonic skips its own TLS layer
        //     entirely; our connector still does the real handshake.
        let display_url = normalise_to_https(server);
        let endpoint_url = display_url.replace("https://", "http://");
        info!(server = %display_url, "gRPC connect: starting");
        let endpoint = tonic::transport::Endpoint::from_shared(endpoint_url.clone())
            .with_context(|| format!("parse endpoint {display_url}"))?;
        let channel = match endpoint
            .connect_with_connector(custom_tls_connector())
            .await
        {
            Ok(c) => {
                info!(server = %display_url, "gRPC connect: channel established");
                c
            }
            Err(e) => {
                // Print the full anyhow / tonic chain so the actual
                // root cause (DNS resolution failed, peer reset
                // mid-handshake, TLS protocol error, …) reaches the
                // operator's logs instead of just "transport error".
                tracing::error!(server = %display_url, error = ?e, "gRPC connect failed");
                return Err(anyhow::Error::new(e))
                    .with_context(|| format!("connect {display_url}"));
            }
        };
        let mut signaling = SignalingClient::new(channel);

        let reg = signaling
            .register(RegisterRequest {
                display_name: display_name.into(),
                password: password.into(),
            })
            .await?
            .into_inner();

        let client_id = reg.client_id;
        let audio_token = reg.audio_token;
        // Server must return exactly MAC_KEY_LEN bytes; treat any
        // other length as a protocol violation rather than silently
        // truncating / padding and producing useless MACs.
        let audio_mac_key: [u8; toki_proto::wire::MAC_KEY_LEN] =
            reg.audio_mac_key.as_slice().try_into().map_err(|_| {
                anyhow!(
                    "server sent audio_mac_key with wrong length ({} bytes, expected {})",
                    reg.audio_mac_key.len(),
                    toki_proto::wire::MAC_KEY_LEN,
                )
            })?;
        let audio_addr = resolve_audio_endpoint(&reg.audio_endpoint, &display_url)?;
        // Session-local sequence counter. Starts at 1 because the
        // server's `audio_last_seq` initialises to 0 and we require
        // strict monotonicity (seq > last_seq) to accept the first
        // packet.
        let udp_seq = Arc::new(AtomicU64::new(1));

        {
            let mut st = state.lock().unwrap();
            st.self_id = Some(client_id.clone());
            st.display_name = display_name.to_string();
            st.frequency = Some(frequency.to_string());
            // Show ourselves in the roster immediately — the server doesn't
            // echo our own MemberJoined back to us.
            st.members
                .insert(client_id.clone(), display_name.to_string());
        }

        // ── UDP socket ────────────────────────────────────────────────
        let udp = Arc::new(UdpSocket::bind("0.0.0.0:0").await?);
        udp.connect(audio_addr).await?;
        // Immediately punch a hole: server records our source addr so
        // peers' audio can find us before we've ever transmitted.
        send_keepalive(&udp, &audio_token, &audio_mac_key, &udp_seq).await?;
        info!(?audio_addr, "udp audio connected");

        // ── Event stream (server → us) ────────────────────────────────
        let events_resp = signaling
            .join(JoinRequest {
                client_id: client_id.clone(),
                frequency: frequency.to_string(),
            })
            .await?;
        let mut events = events_resp.into_inner();
        let state_for_events = state.clone();
        let self_id_for_events = client_id.clone();
        let ptt_atomic = Arc::new(AtomicBool::new(false));
        let ptt_for_events = ptt_atomic.clone();
        let playback_for_events = playback.clone();
        let beeps_for_events = beeps.clone();
        let events_task = tokio::spawn(async move {
            while let Some(evt) = events.next().await {
                match evt {
                    Ok(ce) => match ce.event {
                        Some(Ev::Joined(j)) => {
                            let mut st = state_for_events.lock().unwrap();
                            st.members
                                .insert(j.client_id.clone(), j.display_name.clone());
                            st.log(format!("→ {} joined", j.display_name));
                        }
                        Some(Ev::Left(l)) => {
                            let mut st = state_for_events.lock().unwrap();
                            let name = st
                                .members
                                .remove(&l.client_id)
                                .unwrap_or_else(|| l.client_id.clone());
                            // If the leaver was holding, the server also
                            // sends a Ptt release; clear locally as belt &
                            // braces in case events arrive out of order.
                            if st.holder.as_deref() == Some(l.client_id.as_str()) {
                                st.holder = None;
                            }
                            st.log(format!("← {name} left"));
                        }
                        Some(Ev::Ptt(p)) => {
                            // Update holder state and detect transitions in one
                            // critical section, then play beeps / flip the audio
                            // gate outside the lock.
                            //
                            // Priority adds a third transition beyond the usual
                            // acquire/release: a *takeover*, where the holder
                            // changes from one member directly to another
                            // without an intervening release. The server only
                            // emits that for a priority preemption, flagged via
                            // `p.priority`.
                            let (acquired, released, took_over, prev_holder, talker_name) = {
                                let mut st = state_for_events.lock().unwrap();
                                let prev_holder = st.holder.clone();
                                let was_held = prev_holder.is_some();
                                let new_holder = if p.pressed {
                                    Some(p.client_id.clone())
                                } else {
                                    None
                                };
                                st.holder = new_holder.clone();
                                let acquired = !was_held && new_holder.is_some();
                                let released = was_held && new_holder.is_none();
                                // Takeover: a different member seized a floor
                                // that was already held (preemption).
                                let took_over =
                                    was_held && new_holder.is_some() && new_holder != prev_holder;
                                let name = st
                                    .members
                                    .get(&p.client_id)
                                    .cloned()
                                    .unwrap_or_else(|| p.client_id.clone());
                                (acquired, released, took_over, prev_holder, name)
                            };

                            // Were *we* just bumped off the floor by a priority
                            // speaker? True iff we held it a moment ago and a
                            // different member now holds it. The relay already
                            // stopped forwarding our audio when the server
                            // flipped the holder; closing the gate here stops us
                            // from uselessly uploading and clears our TX state.
                            let self_preempted = took_over
                                && prev_holder.as_deref() == Some(self_id_for_events.as_str())
                                && p.client_id != self_id_for_events;

                            // Flip our own audio gate. Normally we only open it
                            // when the server confirms US as the holder. The
                            // preemption case also forces it *closed* on the
                            // bumped speaker.
                            if p.client_id == self_id_for_events {
                                ptt_for_events.store(p.pressed, Ordering::Relaxed);
                            } else if self_preempted {
                                ptt_for_events.store(false, Ordering::Relaxed);
                            }

                            if self_preempted {
                                // Distinct cue + message for the cut-off
                                // speaker; suppress the priority roger for them
                                // so they hear only the "you lost it" bump.
                                let tone = audio::beep_pattern(
                                    audio::PREEMPTED_BUMP,
                                    beeps_for_events.volume(),
                                );
                                push_playback(&playback_for_events, &tone);
                                state_for_events
                                    .lock()
                                    .unwrap()
                                    .log(format!("⚡ Preempted by {talker_name}"));
                            } else if p.priority && (acquired || took_over) {
                                // A priority speaker took the floor (idle-grant
                                // or preemption). Everyone still listening hears
                                // the fixed two-tone priority roger.
                                let tone = audio::beep_pattern(
                                    audio::PRIORITY_ROGER,
                                    beeps_for_events.volume(),
                                );
                                push_playback(&playback_for_events, &tone);
                                state_for_events
                                    .lock()
                                    .unwrap()
                                    .log(format!("⚡ {talker_name} took priority"));
                            } else if acquired {
                                // Look up the active preset live so a
                                // change in Settings takes effect on
                                // the very next take-floor event,
                                // without a reconnect.
                                let preset = beeps_for_events.current_preset();
                                let tone = audio::beep_pattern(
                                    preset.acquire.steps,
                                    beeps_for_events.volume(),
                                );
                                push_playback(&playback_for_events, &tone);
                                state_for_events
                                    .lock()
                                    .unwrap()
                                    .log(format!("🔒 {talker_name} took the floor"));
                            } else if released {
                                let preset = beeps_for_events.current_preset();
                                let tone = audio::beep_pattern(
                                    preset.release.steps,
                                    beeps_for_events.volume(),
                                );
                                push_playback(&playback_for_events, &tone);
                                state_for_events.lock().unwrap().log("🔓 floor cleared");
                            }
                        }
                        Some(Ev::FrequencyChanged(fc)) => {
                            // Server acknowledged our move. Clear the
                            // old roster (we're about to receive the new
                            // room's MemberJoined backfill) and re-seed
                            // ourselves so we don't vanish from our own
                            // member list.
                            let mut st = state_for_events.lock().unwrap();
                            st.members.clear();
                            if let Some(self_id) = st.self_id.clone() {
                                let our_name = st.display_name.clone();
                                st.members.insert(self_id, our_name);
                            }
                            st.holder = None;
                            st.frequency = Some(fc.frequency.clone());
                            st.log(format!("→ frequency {} MHz", fc.frequency));
                        }
                        Some(Ev::DisplayNameChanged(dnc)) => {
                            // Either someone in our room was renamed
                            // (peer case) or *we* were renamed (subject
                            // case). In both, we rebind the roster
                            // entry; in the subject case we also
                            // refresh our own `display_name` so the
                            // topbar callsign re-renders this frame.
                            let mut st = state_for_events.lock().unwrap();
                            let is_self = st.self_id.as_deref() == Some(dnc.client_id.as_str());
                            // Only update the roster entry if the
                            // client is actually in our current room
                            // (we may receive a self-rename while in
                            // the lobby, with no roster to update).
                            if st.members.contains_key(&dnc.client_id) {
                                st.members
                                    .insert(dnc.client_id.clone(), dnc.display_name.clone());
                            }
                            if is_self {
                                let old = std::mem::replace(
                                    &mut st.display_name,
                                    dnc.display_name.clone(),
                                );
                                st.log(format!("✏️ renamed: {old} → {}", dnc.display_name));
                            } else {
                                st.log(format!("✏️ peer renamed to {}", dnc.display_name));
                            }
                        }
                        None => {}
                    },
                    Err(e) => {
                        warn!(error = %e, "event stream error");
                        break;
                    }
                }
            }
            // Stream closed cleanly — either the server shut down or
            // an admin kicked us. Either way, the events_tx on the
            // server side is gone and no further events will arrive.
            // Surface this to the GUI: flip the connection state back
            // to Disconnected, drop the roster, and log a friendly
            // line so the operator sees the cause rather than just
            // a silently-stuck "Connected" status bar.
            //
            // We can't tell server-shutdown apart from admin-kick
            // here — both look like a graceful EOF — so the log
            // message is deliberately generic.
            info!("event stream closed; transitioning to Disconnected");
            let mut st = state_for_events.lock().unwrap();
            st.connection = crate::state::ConnState::Disconnected;
            st.members.clear();
            st.holder = None;
            st.frequency = None;
            st.log("⚠ disconnected by server");
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
        let key_for_recv = audio_mac_key;
        let recv_task = tokio::spawn(async move {
            let mut buf = vec![0u8; MAX_AUDIO_PACKET];
            let cipher = ChaCha20Poly1305::new(Key::from_slice(&key_for_recv));
            // Server→peer seq counter for strict-monotonic replay
            // protection on the inbound path. Server starts at 1,
            // matching the client→server direction.
            let mut server_last_seq: u64 = 0;
            loop {
                match udp_for_recv.recv(&mut buf).await {
                    Ok(0) => continue,
                    Ok(n) => {
                        if n < HEADER_LEN_S2C {
                            warn!(n, "server packet too small, dropping");
                            continue;
                        }
                        // S2C layout: seq (8) | tag (16) | ciphertext
                        let seq_bytes: [u8; SEQ_LEN] =
                            buf[..SEQ_LEN].try_into().expect("slice has SEQ_LEN bytes");
                        let seq = u64::from_le_bytes(seq_bytes);
                        let tag_bytes: [u8; TAG_LEN] = buf[SEQ_LEN..SEQ_LEN + TAG_LEN]
                            .try_into()
                            .expect("slice has TAG_LEN bytes");
                        let mut plaintext = buf[HEADER_LEN_S2C..n].to_vec();
                        let nonce_bytes = build_nonce(seq);
                        let nonce = Nonce::from_slice(&nonce_bytes);
                        let tag = Tag::from_slice(&tag_bytes);
                        if cipher
                            .decrypt_in_place_detached(
                                nonce,
                                &[VERSION_AUDIO_PCM],
                                &mut plaintext,
                                tag,
                            )
                            .is_err()
                        {
                            warn!("server audio AEAD verify failed, dropping");
                            continue;
                        }
                        if seq <= server_last_seq {
                            // Replay or stale reorder. Strict
                            // monotonic; playback tolerates this as
                            // ordinary loss.
                            continue;
                        }
                        server_last_seq = seq;
                        let samples = pcm_from_bytes(&plaintext);
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
        let key_for_keepalive = audio_mac_key;
        let seq_for_keepalive = udp_seq.clone();
        let keepalive_task = tokio::spawn(async move {
            let mut tick = tokio::time::interval(Duration::from_secs(3));
            tick.tick().await; // consume the immediate first tick
            loop {
                tick.tick().await;
                if let Err(e) = send_keepalive(
                    &udp_for_keepalive,
                    &token_for_keepalive,
                    &key_for_keepalive,
                    &seq_for_keepalive,
                )
                .await
                {
                    warn!(error = %e, "keepalive failed");
                    break;
                }
            }
        });

        Ok(Self {
            client_id,
            audio_token,
            audio_mac_key,
            udp_seq,
            // Shared with events_task — it's the only writer. Flipped to
            // `true` only when the server's broadcast confirms us as holder.
            ptt: ptt_atomic,
            seq: AtomicU64::new(0),
            ptt_tx,
            local_pressed: AtomicBool::new(false),
            cooldown_until: StdMutex::new(None),
            udp,
            signaling,
            tasks: vec![events_task, ptt_task, recv_task, keepalive_task],
        })
    }

    /// Leave the current frequency room without dropping the gRPC
    /// session. Used by the UI's debounced channel selector: the
    /// chevron's first click "takes us off the air" immediately, then
    /// the actual join lands once the user settles on a frequency.
    ///
    /// We also flip the local PTT atomic off and clear the roster so
    /// the UI doesn't show stale members from the room we just left.
    async fn leave_room(&self, state: &SharedState) {
        self.ptt.store(false, Ordering::Relaxed);
        let mut signaling = self.signaling.clone();
        let req = LeaveRequest {
            client_id: self.client_id.clone(),
        };
        if let Err(e) = signaling.leave(req).await {
            warn!(error = %e, "leave_room failed");
        }
        // Optimistically clear local room state. The server has either
        // honored the Leave (in which case its state already matches
        // ours) or errored — either way, painting an empty roster is
        // the right thing to show the user.
        let mut st = state.lock().unwrap();
        st.members.clear();
        if let Some(self_id) = st.self_id.clone() {
            let our_name = st.display_name.clone();
            st.members.insert(self_id, our_name);
        }
        st.holder = None;
        st.frequency = None;
    }

    /// Ask the server to move us to a new frequency. The server emits
    /// `FrequencyChanged` on our event stream once the move is done;
    /// our event handler clears the local roster on receipt and waits
    /// for the new room's MemberJoined backfill.
    async fn change_frequency(&self, frequency: &str, state: &SharedState) {
        // Drop any local PTT state immediately — the old room's lock
        // will be released by the server, but our audio gate must not
        // leak between rooms.
        self.ptt.store(false, Ordering::Relaxed);
        let mut signaling = self.signaling.clone();
        let req = ChangeFrequencyRequest {
            client_id: self.client_id.clone(),
            frequency: frequency.to_string(),
        };
        if let Err(e) = signaling.change_frequency(req).await {
            warn!(error = %e, "change_frequency failed");
            state
                .lock()
                .unwrap()
                .log(format!("frequency change failed: {e}"));
        }
    }

    /// Request a PTT state change. The actual audio gate is not flipped
    /// here — we wait for the server's broadcast to confirm whether the
    /// request was granted (walkie-talkie arbitration). If denied, the
    /// server stays silent and our atomic stays `false`.
    ///
    /// Spam debounce: we drop
    ///   * duplicate `PttDown`s while already locally pressed;
    ///   * `PttDown`s that arrive within `PTT_COOLDOWN` of the last
    ///     `PttUp` we sent;
    ///   * orphan `PttUp`s that don't have a matching `PttDown`.
    ///
    /// These cuts happen at the runtime boundary so both the global
    /// hotkey and the on-screen button get the same protection
    /// without each call site duplicating the logic.
    async fn request_ptt(&self, pressed: bool) {
        if pressed {
            // Already pressed? Quietly drop — the OS / global hotkey
            // poller occasionally emits a redundant down on bouncy
            // keys, and the server doesn't want to see it.
            if self.local_pressed.load(Ordering::Relaxed) {
                return;
            }
            // Inside the post-release cooldown? Drop.
            let until = *self.cooldown_until.lock().unwrap();
            if let Some(t) = until {
                if Instant::now() < t {
                    return;
                }
            }
            self.local_pressed.store(true, Ordering::Relaxed);
        } else {
            // No matching down? Nothing to release — orphan up.
            // Could come from a denied first press whose release
            // still hit the wire, or from a connection-drop reset.
            if !self.local_pressed.swap(false, Ordering::Relaxed) {
                return;
            }
            // Open the cooldown gate now so the *next* fresh press
            // has to wait at least `PTT_COOLDOWN`.
            *self.cooldown_until.lock().unwrap() = Some(Instant::now() + PTT_COOLDOWN);
        }

        let seq = self.seq.fetch_add(1, Ordering::Relaxed);
        let evt = PttEvent {
            client_id: self.client_id.clone(),
            pressed,
            sequence: seq,
            // Client never self-declares priority — the server is the
            // sole arbiter. This field is only meaningful on the
            // server→client grant broadcast.
            priority: false,
        };
        if let Err(e) = self.ptt_tx.send(evt).await {
            warn!(error = %e, "ptt send failed");
        }
    }

    async fn send_audio(&self, samples: &[i16]) {
        let mut payload = Vec::with_capacity(samples.len() * 2);
        for &s in samples {
            payload.extend_from_slice(&s.to_le_bytes());
        }
        let pkt = build_authenticated_packet(
            &self.audio_token,
            VERSION_AUDIO_PCM,
            &self.audio_mac_key,
            &self.udp_seq,
            &payload,
        );
        if let Err(e) = self.udp.send(&pkt).await {
            warn!(error = %e, "udp send failed");
        }
    }

    async fn close(mut self) {
        let _ = self
            .signaling
            .leave(LeaveRequest {
                client_id: self.client_id.clone(),
            })
            .await;
        for t in &self.tasks {
            t.abort();
        }
    }
}

async fn send_keepalive(
    udp: &UdpSocket,
    token: &[u8],
    mac_key: &[u8; toki_proto::wire::MAC_KEY_LEN],
    udp_seq: &Arc<AtomicU64>,
) -> Result<()> {
    let pkt = build_authenticated_packet(token, VERSION_KEEPALIVE, mac_key, udp_seq, &[]);
    udp.send(&pkt).await?;
    Ok(())
}

/// Assemble an outbound UDP packet with the AEAD-encrypted header
/// layout the server expects (see `toki_proto::wire` docs). Bumps
/// the session's monotonic sequence atomically; the seq doubles as
/// the ChaCha20-Poly1305 nonce, AAD is the single-byte version so
/// an attacker can't repurpose a tag from one version onto another.
fn build_authenticated_packet(
    token: &[u8],
    version: u8,
    session_key: &[u8; toki_proto::wire::MAC_KEY_LEN],
    udp_seq: &Arc<AtomicU64>,
    payload: &[u8],
) -> Vec<u8> {
    let seq = udp_seq.fetch_add(1, Ordering::Relaxed);
    let seq_bytes = seq.to_le_bytes();

    let cipher = ChaCha20Poly1305::new(GenericArray::from_slice(session_key));
    let nonce_bytes = build_nonce(seq);
    let nonce = Nonce::from_slice(&nonce_bytes);
    let mut ciphertext = payload.to_vec();
    let tag = cipher
        .encrypt_in_place_detached(nonce, &[version], &mut ciphertext)
        .expect("ChaCha20-Poly1305 encrypt never fails for in-memory plaintext");

    let mut pkt = Vec::with_capacity(HEADER_LEN_C2S + ciphertext.len());
    pkt.extend_from_slice(token);
    pkt.push(version);
    pkt.extend_from_slice(&seq_bytes);
    pkt.extend_from_slice(tag.as_slice());
    pkt.extend_from_slice(&ciphertext);
    pkt
}

/// Coerce whatever the user wrote in the Connect dialog into an
/// `https://host:port` string. Plain `http://` is auto-upgraded
/// (gRPC has no plaintext mode any more); bare `host:port` gets the
/// scheme prefixed; anything already https:// passes through.
fn normalise_to_https(server: &str) -> String {
    if let Some(rest) = server.strip_prefix("http://") {
        format!("https://{rest}")
    } else if server.starts_with("https://") {
        server.to_string()
    } else {
        format!("https://{server}")
    }
}

/// Build a `rustls::ClientConfig` that accepts *any* server
/// certificate. Required because the server's default is an
/// auto-generated self-signed cert, which wouldn't chain to a
/// system trust root; forcing operators to provision a real cert
/// (or install a CA on every client) would defeat the "TLS just
/// works" goal.
///
/// What we lose by skipping cert validation:
///   * Server identity is no longer authenticated by TLS itself.
///     An active on-path attacker could substitute their own cert
///     and terminate the TLS session, becoming a MITM.
///
/// What still protects the session:
///   * The shared-secret password gate. An MITM that captures
///     `RegisterRequest.password` once can replay it, but can't
///     impersonate the real server's *audio relay* without also
///     possessing the per-session ChaCha20-Poly1305 keys for
///     every other live participant — and those are minted server-
///     side per session, never travel out of the registry, and
///     would have to be exfiltrated separately.
///   * UDP audio is AEAD'd under per-session keys minted at
///     register time. An MITM who fakes a Register exchange
///     learns one session's key, but can't decrypt the streams of
///     other participants in the same room (each peer has its
///     own session key).
///
/// Stronger options (TOFU pinning via `~/.config/toki/known_servers.toml`,
/// or operator-provided pinned cert) are tracked in
/// `notes/security-followups.md`; this is the v1 simplicity vs.
/// authenticity trade-off.
fn insecure_tls_config() -> rustls::ClientConfig {
    use rustls::client::danger::{HandshakeSignatureValid, ServerCertVerified, ServerCertVerifier};
    use rustls::pki_types::{CertificateDer, ServerName, UnixTime};

    #[derive(Debug)]
    struct AcceptAny;

    impl ServerCertVerifier for AcceptAny {
        fn verify_server_cert(
            &self,
            _end_entity: &CertificateDer<'_>,
            _intermediates: &[CertificateDer<'_>],
            _server_name: &ServerName<'_>,
            _ocsp: &[u8],
            _now: UnixTime,
        ) -> Result<ServerCertVerified, rustls::Error> {
            Ok(ServerCertVerified::assertion())
        }

        fn verify_tls12_signature(
            &self,
            _message: &[u8],
            _cert: &CertificateDer<'_>,
            _dss: &rustls::DigitallySignedStruct,
        ) -> Result<HandshakeSignatureValid, rustls::Error> {
            Ok(HandshakeSignatureValid::assertion())
        }

        fn verify_tls13_signature(
            &self,
            _message: &[u8],
            _cert: &CertificateDer<'_>,
            _dss: &rustls::DigitallySignedStruct,
        ) -> Result<HandshakeSignatureValid, rustls::Error> {
            Ok(HandshakeSignatureValid::assertion())
        }

        fn supported_verify_schemes(&self) -> Vec<rustls::SignatureScheme> {
            // Mirror rustls's default set so the handshake doesn't
            // fail by accidentally pruning a signature scheme the
            // server picks.
            vec![
                rustls::SignatureScheme::RSA_PKCS1_SHA256,
                rustls::SignatureScheme::RSA_PKCS1_SHA384,
                rustls::SignatureScheme::RSA_PKCS1_SHA512,
                rustls::SignatureScheme::ECDSA_NISTP256_SHA256,
                rustls::SignatureScheme::ECDSA_NISTP384_SHA384,
                rustls::SignatureScheme::ECDSA_NISTP521_SHA512,
                rustls::SignatureScheme::RSA_PSS_SHA256,
                rustls::SignatureScheme::RSA_PSS_SHA384,
                rustls::SignatureScheme::RSA_PSS_SHA512,
                rustls::SignatureScheme::ED25519,
                rustls::SignatureScheme::ED448,
            ]
        }
    }

    rustls::ClientConfig::builder()
        .dangerous()
        .with_custom_certificate_verifier(std::sync::Arc::new(AcceptAny))
        .with_no_client_auth()
}

/// Build a `tower::Service<http::Uri>` that connects via TCP and
/// performs a TLS handshake using [`insecure_tls_config`] (custom
/// verifier that accepts any cert). Returned wrapped in
/// `hyper_util::rt::TokioIo` so it satisfies the
/// `hyper::rt::Read + Write` bounds `Endpoint::connect_with_connector`
/// expects.
///
/// The return-type signature is unavoidably verbose because
/// `connect_with_connector` requires us to spell out the response
/// stream and future types explicitly; clippy's complexity warning
/// is genuine but not actionable here.
#[allow(clippy::type_complexity)]
fn custom_tls_connector() -> impl tower::Service<
    http::Uri,
    Response = hyper_util::rt::TokioIo<tokio_rustls::client::TlsStream<tokio::net::TcpStream>>,
    Error = anyhow::Error,
    Future = std::pin::Pin<
        Box<
            dyn std::future::Future<
                    Output = Result<
                        hyper_util::rt::TokioIo<
                            tokio_rustls::client::TlsStream<tokio::net::TcpStream>,
                        >,
                        anyhow::Error,
                    >,
                > + Send,
        >,
    >,
> + Clone {
    let tls = std::sync::Arc::new(insecure_tls_config());
    tower::service_fn(move |uri: http::Uri| {
        let tls = tls.clone();
        Box::pin(async move {
            let host = uri
                .host()
                .ok_or_else(|| anyhow!("connect uri missing host"))?
                .to_string();
            let port = uri.port_u16().unwrap_or(443);
            tracing::debug!(%host, port, "tcp connect");
            let tcp = match tokio::net::TcpStream::connect((host.as_str(), port)).await {
                Ok(s) => s,
                Err(e) => {
                    tracing::error!(%host, port, error = %e, "tcp connect failed");
                    return Err(anyhow::Error::new(e))
                        .with_context(|| format!("tcp connect {host}:{port}"));
                }
            };
            // ServerName here is what rustls will (would, if our
            // verifier weren't a no-op) compare against the SAN in
            // the server's cert. The verifier ignores it; we still
            // pass the host so the SNI extension goes out
            // correctly, which some servers gate on.
            let server_name = tokio_rustls::rustls::pki_types::ServerName::try_from(host.clone())
                .with_context(|| format!("invalid TLS server name {host}"))?;
            tracing::debug!(%host, port, "tls handshake");
            let tls_stream = match tokio_rustls::TlsConnector::from(tls)
                .connect(server_name, tcp)
                .await
            {
                Ok(s) => s,
                Err(e) => {
                    tracing::error!(%host, port, error = %e, "tls handshake failed");
                    return Err(anyhow::Error::new(e))
                        .with_context(|| format!("tls handshake {host}:{port}"));
                }
            };
            Ok(hyper_util::rt::TokioIo::new(tls_stream))
        })
            as std::pin::Pin<
                Box<
                    dyn std::future::Future<
                            Output = Result<
                                hyper_util::rt::TokioIo<
                                    tokio_rustls::client::TlsStream<tokio::net::TcpStream>,
                                >,
                                anyhow::Error,
                            >,
                        > + Send,
                >,
            >
    })
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn normalise_to_https_adds_scheme_to_bare_hostport() {
        assert_eq!(
            normalise_to_https("127.0.0.1:50051"),
            "https://127.0.0.1:50051"
        );
    }

    #[test]
    fn normalise_to_https_upgrades_http() {
        assert_eq!(
            normalise_to_https("http://server:1234"),
            "https://server:1234"
        );
    }

    #[test]
    fn normalise_to_https_passes_through_https() {
        assert_eq!(
            normalise_to_https("https://server:1234"),
            "https://server:1234"
        );
    }

    #[test]
    fn pcm_from_bytes_round_trips_with_to_le_bytes() {
        // i16::to_le_bytes / from_le_bytes are the wire format; if
        // an endianness assumption ever slips, every audio session
        // turns to garbage. Lock the round-trip down.
        let samples: Vec<i16> = vec![0, 1, -1, i16::MAX, i16::MIN, 12345, -12345];
        let bytes: Vec<u8> = samples.iter().flat_map(|s| s.to_le_bytes()).collect();
        assert_eq!(pcm_from_bytes(&bytes), samples);
    }

    #[test]
    fn pcm_from_bytes_ignores_trailing_partial_sample() {
        // chunks_exact drops the trailing byte, which is the
        // desired behavior for a UDP frame that got truncated.
        let bytes: Vec<u8> = vec![0x00, 0x01, 0x02, 0x03, 0x04];
        let result = pcm_from_bytes(&bytes);
        assert_eq!(result.len(), 2);
    }

    #[test]
    fn resolve_audio_endpoint_passes_through_routable_addr() {
        let resolved = resolve_audio_endpoint("203.0.113.5:50052", "https://server:50051").unwrap();
        assert_eq!(resolved.to_string(), "203.0.113.5:50052");
    }

    #[test]
    fn resolve_audio_endpoint_substitutes_signaling_host_for_unspecified() {
        // Server commonly advertises 0.0.0.0:port; rewrite to the
        // host portion of the gRPC URL.
        let resolved =
            resolve_audio_endpoint("0.0.0.0:50052", "https://192.168.1.50:50051").unwrap();
        assert_eq!(resolved.to_string(), "192.168.1.50:50052");
    }

    #[test]
    fn resolve_audio_endpoint_rejects_malformed_input() {
        assert!(resolve_audio_endpoint("nope", "https://server:50051").is_err());
    }
}
