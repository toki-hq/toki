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
use tokio::net::{lookup_host, UdpSocket};
use tokio::sync::mpsc::{self, UnboundedReceiver, UnboundedSender};
use tokio::task::JoinHandle;
use tokio_stream::wrappers::ReceiverStream;
use tonic::transport::Channel;
use tracing::{debug, info, warn};

use chacha20poly1305::{
    aead::{generic_array::GenericArray, AeadInPlace},
    ChaCha20Poly1305, Key, KeyInit, Nonce, Tag,
};
use toki_proto::v1::{
    event::Event as Ev, signaling_client::SignalingClient, ChangeFrequencyRequest,
    IdentityChallengeRequest, JoinRequest, LeaveRequest, PttEvent, RegisterRequest,
};
use toki_proto::wire::{
    build_nonce, decode_ping, encode_ping, HEADER_LEN_C2S, HEADER_LEN_S2C, MAX_AUDIO_PACKET,
    MAX_OPUS_PAYLOAD, OPUS_FRAME_SAMPLES, PING_LEN, SEQ_LEN, TAG_LEN, VERSION_AUDIO_OPUS,
    VERSION_AUDIO_PCM, VERSION_KEEPALIVE, VERSION_PONG,
};

use crate::audio::{self, push_playback, push_voice, BeepParams, PlaybackBuf};
use crate::dsp::{Dsp, DspParams, MonitorParams, OutputDsp, OutputDspParams};
use crate::state::{ConnState, SharedState};
use crate::telemetry::QualityTracker;

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
    /// Press or release the dedicated global-broadcast PTT. Only effective
    /// when the server has granted us broadcast capability (`can_broadcast`).
    /// Sends `PttEvent { broadcast: true }` on the existing PushToTalk
    /// stream; the server fans the audio out to every frequency room.
    BroadcastPttDown,
    BroadcastPttUp,
    /// Preview a beep with the current [`BeepParams`] values. No gRPC
    /// traffic and no session required — used by the Settings TEST
    /// buttons so the user can audition tone tweaks without having
    /// to actually press PTT.
    TestBeep(BeepKind),
    /// Play a short test chime through the selected output device. Like
    /// [`Cmd::TestBeep`] it needs no session — it synthesises a fixed
    /// rising two-tone and pushes it onto the playback ring. Used by the
    /// Settings → AUDIO "TEST TONE" button to confirm the right
    /// speaker/earpiece (and balance) before connecting.
    TestTone,
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
#[allow(clippy::too_many_arguments)]
pub fn spawn(
    state: SharedState,
    mic_rx: UnboundedReceiver<Vec<i16>>,
    playback: PlaybackBuf,
    effects: PlaybackBuf,
    beeps: BeepParams,
    dsp_params: DspParams,
    output_dsp_params: OutputDspParams,
    monitor_params: MonitorParams,
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
            rt.block_on(run(
                cmd_rx,
                state,
                mic_rx,
                playback,
                effects,
                beeps,
                dsp_params,
                output_dsp_params,
                monitor_params,
            ));
        })
        .expect("spawn runtime thread");
    cmd_tx
}

#[allow(clippy::too_many_arguments)]
async fn run(
    mut cmd_rx: UnboundedReceiver<Cmd>,
    state: SharedState,
    mut mic_rx: UnboundedReceiver<Vec<i16>>,
    playback: PlaybackBuf,
    effects: PlaybackBuf,
    beeps: BeepParams,
    dsp_params: DspParams,
    output_dsp_params: OutputDspParams,
    monitor_params: MonitorParams,
) {
    let mut session: Option<Session> = None;
    // Capture-side DSP (noise suppression + AGC), applied to every mic
    // frame while a session is live — see the dsp module docs for why
    // it runs outside the PTT gate too (keeps the denoiser/AGC state
    // warm so a fresh transmission doesn't open with a settling burst).
    // Toggles arrive live through `dsp_params`; with both stages off,
    // `process` is a bit-exact passthrough.
    let mut dsp = Dsp::new(dsp_params);
    // Radio FX (band-pass + saturation + static) is a *transmit-side*
    // effect: the sender dirties their own outgoing voice so every peer
    // hears their chosen walkie-talkie character baked in. `tx_fx` runs on
    // the frame we send (after capture DSP, only while transmitting);
    // `monitor_fx` is an independent instance over the self-monitor copy
    // so the operator hears the same chain when auditioning. Both read the
    // same `OutputDspParams` (one toggle/amount drives both) but keep
    // separate filter/noise/envelope state — they process different
    // streams and must not share continuity. Off by default → passthrough.
    let mut tx_fx = OutputDsp::new(output_dsp_params.clone());
    let mut monitor_fx = OutputDsp::new(output_dsp_params.clone());
    // Tracks the confirmed-talking state across mic frames so we can
    // detect the talk→silent edge (PTT release) and flush the encoder's
    // trailing partial frame exactly once. The mic stream runs
    // continuously regardless of PTT, so a frame always arrives shortly
    // after release to carry the flush.
    let mut was_talking = false;

    loop {
        tokio::select! {
            cmd = cmd_rx.recv() => {
                let Some(cmd) = cmd else { break; };
                // handle_cmd returns false on a clean shutdown command
                // (e.g. Cmd::Shutdown). We break out of the select loop
                // then so the runtime thread can exit promptly rather
                // than sit waiting on a dead UI.
                if !handle_cmd(cmd, &mut session, &state, &playback, &effects, &beeps).await {
                    break;
                }
            }
            frame = mic_rx.recv() => {
                let Some(mut frame) = frame else { break; };
                let monitoring = monitor_params.enabled();
                // Run the capture DSP whenever a session is live *or* the
                // self-monitor is on. Keeping it running while monitoring
                // — even in raw mode — keeps the denoiser RNN / AGC gain
                // warm, so flipping the monitor's RAW↔PROCESSED toggle is
                // an instant A/B instead of a half-second of the AGC
                // re-settling. With both stages off, `process` is a
                // bit-exact passthrough, so this costs nothing then.
                let want_dsp = session.is_some() || monitoring;
                // Snapshot the raw mic *before* DSP only when the monitor
                // is in RAW mode — that's the buffer it'll loop back. The
                // common connected-not-monitoring path skips the clone and
                // keeps its zero-copy in-place processing.
                let monitor_raw = if monitoring && !monitor_params.processed() {
                    Some(frame.clone())
                } else {
                    None
                };
                if want_dsp {
                    dsp.process(&mut frame);
                }

                // Self-monitor (sidetone): loop our own mic to the
                // speakers so the operator can hear themselves and audibly
                // confirm the chain. Works with no session — a local test
                // path. We preview the *full* outgoing-to-peer chain:
                //   RAW       → mic → (Radio FX)
                //   PROCESSED → mic → capture DSP → (Radio FX)
                // so Radio FX is heard here too when it's on (its recv-task
                // instance only colours *incoming* network voice). We dirty
                // an owned copy, never `frame` itself — `frame` still goes
                // to peers untouched, and applying our local Radio FX to it
                // would wrongly pre-dirty their audio. push_voice keeps the
                // sidetone backlog tight like incoming voice.
                if monitoring {
                    // RAW reuses the pre-DSP snapshot; PROCESSED copies the
                    // post-DSP frame so the peer-bound `frame` stays clean.
                    let mut monitor_buf = monitor_raw.unwrap_or_else(|| frame.clone());
                    monitor_fx.process(&mut monitor_buf);
                    push_voice(&playback, &monitor_buf);
                }

                if let Some(s) = &session {
                    // Server-side mute is a hard local gate: even if a PTT
                    // grant is somehow still set, a muted session uploads
                    // nothing (the relay would drop it anyway). The
                    // `was_talking` edge below then fires the encoder flush.
                    let talking = s.ptt.load(Ordering::Relaxed)
                        && !s.self_muted.load(Ordering::Relaxed);
                    if talking {
                        // Bake Radio FX into the outgoing voice (no-op when
                        // disabled). Applied only while transmitting — there's
                        // no point dirtying frames we won't send; the filter
                        // settles within a frame, so a cold start per-PTT is
                        // inaudible. Peers therefore hear the sender's FX.
                        tx_fx.process(&mut frame);
                        s.send_audio(&frame).await;
                    } else if was_talking {
                        // Just released: flush the encoder tail so the
                        // end of speech isn't clipped and the buffer is
                        // clear for the next transmission.
                        s.flush_audio().await;
                    }
                    was_talking = talking;
                } else {
                    was_talking = false;
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
    effects: &PlaybackBuf,
    beeps: &BeepParams,
) -> bool {
    match cmd {
        Cmd::Connect {
            server,
            display_name,
            frequency,
            password,
        } => {
            // Reap a session whose event stream has already died (server
            // shutdown or admin kick): the GUI shows us offline, but the
            // stale `Session` is still here. Without this, the guard below
            // would treat the reconnect as a redundant "already connected"
            // and the user would be stuck on the offline screen until they
            // restart the app.
            if session
                .as_ref()
                .is_some_and(|s| !s.alive.load(Ordering::Relaxed))
            {
                if let Some(s) = session.take() {
                    s.close().await;
                }
            }
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
                effects.clone(),
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
                st.broadcast_active = false;
                st.self_id = None;
                st.frequency = None;
                st.channel_name = None;
                st.conn_quality = None;
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
        Cmd::BroadcastPttDown => {
            if let Some(s) = session.as_ref() {
                s.request_broadcast_ptt(true, state).await;
            }
        }
        Cmd::BroadcastPttUp => {
            if let Some(s) = session.as_ref() {
                s.request_broadcast_ptt(false, state).await;
            }
        }
        Cmd::TestBeep(kind) => {
            // Preview tones don't require an active session — they
            // just synthesise audio with the current BeepParams and
            // push it onto the effects ring (the dedicated beep/cue
            // path, separate from voice). The audio thread takes it
            // from there.
            let preset = beeps.current_preset();
            let steps = match kind {
                BeepKind::Acquire => preset.acquire.steps,
                BeepKind::Release => preset.release.steps,
            };
            let tone = audio::beep_pattern(steps, beeps.volume());
            push_playback(effects, &tone);
        }
        Cmd::TestTone => {
            // Self-generated output check — no session, no roger preset.
            // A short rising two-tone (660 → 880 Hz) is unmistakably
            // "sound is coming out of this device", and the rise is easy
            // to localise across L/R so it doubles as a balance check.
            //
            // Synthesised at a fixed comfortable amplitude rather than
            // `beeps.volume()` on purpose: this is an *output test*, so
            // it must be audible even for a user who's turned roger beeps
            // down to 0%. The SPK VOL knob still scales it at the output
            // callback, so the user hears it at their normal listening
            // level.
            const TEST_TONE_AMPLITUDE: f32 = 0.5;
            let chime = audio::beep_pattern(
                &[
                    audio::BeepStep::Tone {
                        freq_hz: 659.25,
                        duration_ms: 180,
                    },
                    audio::BeepStep::Tone {
                        freq_hz: 880.0,
                        duration_ms: 220,
                    },
                ],
                TEST_TONE_AMPLITUDE,
            );
            push_playback(effects, &chime);
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

/// Outbound voice encoder. `Pcm` passes each 10 ms mic frame straight
/// through (raw i16 LE, the legacy path). `Opus` encodes samples into
/// one variable-length packet per `frame_samples` window — 480 (10 ms),
/// 960 (20 ms), or 1920 (40 ms) as advertised by the server — just
/// ~20× less bandwidth than PCM. Codec and frame size are chosen at
/// connect time from the server's `RegisterResponse` advertisement.
enum AudioEncoder {
    Pcm,
    Opus {
        enc: audiopus::coder::Encoder,
        /// Carries any samples short of a full frame.
        /// Empty in steady state when the mic delivers frames that are
        /// an exact multiple of `frame_samples`, but tolerates odd-sized
        /// inputs without losing audio.
        buf: Vec<i16>,
        /// DTX is on. When set, a frame the encoder collapses to a ≤2-byte
        /// "no transmission" DTX indicator is **dropped** rather than sent
        /// — that drop is what actually removes the packet (and its IP/UDP
        /// overhead) during silence; the receiver fills the gap with PLC /
        /// comfort noise, which the playback path already tolerates as loss.
        dtx: bool,
        /// Number of samples per encoded Opus frame: 480 = 10 ms,
        /// 960 = 20 ms, 1920 = 40 ms at 48 kHz.
        frame_samples: usize,
    },
}

impl AudioEncoder {
    fn new(opus_enabled: bool, bitrate: u32, dtx: bool, opus_frame_ms: u32) -> Self {
        if !opus_enabled {
            return Self::Pcm;
        }
        // Compute the per-frame sample count from the server-advertised
        // frame duration: 480 × (ms / 10), clamped to at least 10 ms.
        // Valid values are 10, 20, or 40 ms; anything else falls back to
        // 10 ms (480 samples) so a mis-configured server can't panic us.
        let frame_samples = (480 * (opus_frame_ms / 10).max(1)) as usize;
        match Self::make_opus(bitrate, dtx) {
            Ok(enc) => Self::Opus {
                enc,
                buf: Vec::with_capacity(frame_samples * 2),
                dtx,
                frame_samples,
            },
            Err(e) => {
                warn!(error = %e, "Opus encoder init failed; falling back to raw PCM");
                Self::Pcm
            }
        }
    }

    fn make_opus(bitrate: u32, dtx: bool) -> Result<audiopus::coder::Encoder, audiopus::Error> {
        use audiopus::{coder::Encoder, Application, Bitrate, Channels, SampleRate};
        let mut enc = Encoder::new(SampleRate::Hz48000, Channels::Mono, Application::Voip)?;
        enc.set_bitrate(Bitrate::BitsPerSecond(bitrate as i32))?;
        if dtx {
            // Enable Opus DTX. Two settings are required and order matters:
            //
            //  1. **VBR on.** DTX only drops/shrinks frames in variable-
            //     bitrate mode — in the default CBR the encoder must hit
            //     the target bitrate every frame, so silence still costs a
            //     full ~24-byte frame and DTX is silently a no-op. (This is
            //     the trap: setting DTX alone does nothing — measured.)
            //  2. **DTX on**, via the raw CTL passthrough — `audiopus` 0.2
            //     has no typed setter — with libopus's stable request code
            //     OPUS_SET_DTX_REQUEST (4016).
            //
            // With both, silence collapses to a tiny comfort-noise frame
            // every ~400 ms instead of 100 full frames/sec, cutting the
            // talker's outbound — and the whole per-listener fan-out — for
            // the silent fraction of a transmission.
            enc.set_vbr(true)?;
            const OPUS_SET_DTX_REQUEST: i32 = 4016;
            enc.set_encoder_ctl_request(OPUS_SET_DTX_REQUEST, 1)?;
        }
        Ok(enc)
    }

    /// Consume a mic frame; return the `(version, payload)` packets ready
    /// to seal + send (0, 1, or more). Buffers samples until a full
    /// `frame_samples` window is available, then encodes and emits.
    fn push(&mut self, samples: &[i16]) -> Vec<(u8, Vec<u8>)> {
        match self {
            Self::Pcm => {
                let mut payload = Vec::with_capacity(samples.len() * 2);
                for &s in samples {
                    payload.extend_from_slice(&s.to_le_bytes());
                }
                vec![(VERSION_AUDIO_PCM, payload)]
            }
            Self::Opus {
                enc,
                buf,
                dtx,
                frame_samples,
            } => {
                buf.extend_from_slice(samples);
                let mut out = Vec::new();
                while buf.len() >= *frame_samples {
                    let frame: Vec<i16> = buf.drain(..*frame_samples).collect();
                    let mut encoded = [0u8; MAX_OPUS_PAYLOAD];
                    match enc.encode(&frame, &mut encoded) {
                        // With DTX, Opus signals "don't transmit" for a
                        // silent frame by returning a ≤2-byte frame. Drop
                        // it — not sending the packet is the whole point
                        // (the receiver's decoder fills the gap). Without
                        // DTX, frames are never this small, so the guard
                        // only ever fires on real DTX indicators.
                        Ok(n) if *dtx && n <= 2 => {}
                        Ok(n) => out.push((VERSION_AUDIO_OPUS, encoded[..n].to_vec())),
                        Err(e) => warn!(error = %e, "Opus encode failed, dropping frame"),
                    }
                }
                out
            }
        }
    }

    /// Flush the encoder at the end of a transmission (PTT release).
    ///
    /// In steady state the mic delivers exactly 10 ms (480-sample)
    /// frames, so the Opus buffer is empty between frames and this does
    /// nothing. But if a final odd-sized frame leaves a partial remainder
    /// it would otherwise be silently dropped (clipping the end of speech)
    /// *and* linger in the buffer to be prepended to the next transmission
    /// (garbling its start). We pad the remainder to a full 10 ms frame
    /// with silence, encode it once, and clear the buffer so the next PTT
    /// starts clean. PCM carries no buffer, so this is a no-op.
    fn flush(&mut self) -> Vec<(u8, Vec<u8>)> {
        match self {
            Self::Pcm => Vec::new(),
            // The flush frame carries the trailing speech tail, so it's
            // always sent — DTX dropping doesn't apply here (`dtx` ignored).
            Self::Opus {
                enc,
                buf,
                frame_samples,
                ..
            } => {
                if buf.is_empty() {
                    return Vec::new();
                }
                // push() drains every whole frame, so buf is always a
                // partial (< frame_samples) remainder here. Pad to a full
                // frame with silence.
                buf.resize(*frame_samples, 0);
                // Take the whole padded frame, leaving `buf` empty so the
                // next transmission starts clean (no stale-tail leak).
                let frame: Vec<i16> = std::mem::take(buf);
                let mut encoded = [0u8; MAX_OPUS_PAYLOAD];
                match enc.encode(&frame, &mut encoded) {
                    Ok(n) => vec![(VERSION_AUDIO_OPUS, encoded[..n].to_vec())],
                    Err(e) => {
                        warn!(error = %e, "Opus flush encode failed, dropping tail");
                        Vec::new()
                    }
                }
            }
        }
    }
}

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
    /// Separate debounce state for the broadcast PTT path so it can't
    /// interfere with the normal PTT gate. Mirrors the semantics of
    /// `local_pressed` / `cooldown_until` above.
    broadcast_local_pressed: AtomicBool,
    broadcast_cooldown_until: StdMutex<Option<Instant>>,
    udp: Arc<UdpSocket>,
    signaling: SignalingClient<Channel>,
    /// Outbound voice encoder (PCM or Opus per the server's advertised
    /// codec). Behind a mutex because `send_audio` takes `&self`; the
    /// lock is held only to produce packets, never across an `await`.
    encode: StdMutex<AudioEncoder>,
    /// `false` once the event stream has closed (server shutdown or admin
    /// kick). The events task is the sole writer; the runtime loop reads
    /// it to reap a dead session so a reconnect isn't blocked by the
    /// already-connected guard. See `Session::open`.
    alive: Arc<AtomicBool>,
    /// `true` while an admin has us server-side muted (see the
    /// `MuteChanged` event handler). The server refuses our presses
    /// regardless, but the mic loop also checks this so we don't keep
    /// uploading frames the relay will drop. The events task is the
    /// sole writer; the runtime mic loop reads it.
    self_muted: Arc<AtomicBool>,
    tasks: Vec<JoinHandle<()>>,
}

impl Session {
    #[allow(clippy::too_many_arguments)]
    async fn open(
        server: &str,
        display_name: &str,
        frequency: &str,
        password: &str,
        state: SharedState,
        playback: PlaybackBuf,
        effects: PlaybackBuf,
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

        // ── Optional identity handshake ───────────────────────────────
        // Load (or mint, on very first connect) the persistent keypair
        // identity, then ask the server for a challenge nonce to sign.
        // UNIMPLEMENTED means the server predates identity support —
        // register identity-less, exactly like a pre-identity client.
        // Any other challenge failure also degrades to identity-less
        // (with a warning): in this release identity is informational,
        // so a transient hiccup shouldn't cost the user the connection.
        let mut identity = crate::identity::Identity::load_or_generate();
        let (identity_pubkey, challenge_nonce, identity_signature) = match signaling
            .identity_challenge(IdentityChallengeRequest {})
            .await
        {
            Ok(resp) => {
                let nonce = resp.into_inner().nonce;
                let signature = identity.sign_challenge(&nonce);
                (identity.pubkey_bytes().to_vec(), nonce, signature)
            }
            Err(s) if s.code() == tonic::Code::Unimplemented => {
                info!("server has no identity support; registering identity-less");
                (Vec::new(), Vec::new(), Vec::new())
            }
            Err(s) => {
                warn!(error = %s, "identity challenge failed; registering identity-less");
                (Vec::new(), Vec::new(), Vec::new())
            }
        };
        let presenting_identity = !identity_pubkey.is_empty();

        let reg = signaling
            .register(RegisterRequest {
                display_name: display_name.into(),
                password: password.into(),
                // The server rejects a MAJOR.MINOR mismatch up front
                // (see toki_proto::version) so an out-of-date client gets
                // a clear "please update" instead of silently broken audio.
                client_version: env!("CARGO_PKG_VERSION").into(),
                // The identity attributes travel only alongside an actual
                // identity — sending them bare would be unverifiable noise.
                machine_hash: if presenting_identity {
                    crate::identity::machine_hash().unwrap_or_default()
                } else {
                    String::new()
                },
                origin_client_id: if presenting_identity {
                    identity.origin_client_id.clone()
                } else {
                    String::new()
                },
                identity_pubkey,
                challenge_nonce,
                identity_signature,
            })
            .await?
            .into_inner();

        // First time this identity is accepted anywhere: remember the
        // session id it was issued as its provenance breadcrumb.
        if presenting_identity {
            identity.record_origin(&reg.client_id);
        }

        let client_id = reg.client_id;
        let audio_token = reg.audio_token;
        // Codec the server asked us to use (advisory; receivers decode
        // per-packet regardless). Built into the encoder below.
        let opus_enabled = reg.opus_enabled;
        let opus_bitrate = reg.opus_bitrate;
        // Whether to enable Opus DTX (silence → tiny comfort-noise frames,
        // cutting outbound + the fan-out during gaps). Server-advertised;
        // only meaningful with Opus on.
        let opus_dtx = reg.opus_dtx;
        // Server-advertised Opus frame duration in milliseconds (10, 20, or
        // 40). Larger frames compress better but add latency; 0 or a missing
        // field from an older server defaults to 10 ms (480 samples).
        let opus_frame_ms = reg.opus_frame_ms;
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
        let audio_addr = resolve_audio_endpoint(&reg.audio_endpoint, &display_url).await?;
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
        // peers' audio can find us before we've ever transmitted. ping_id
        // 0 is the initial probe; the keepalive task counts up from 1.
        send_keepalive(&udp, &audio_token, &audio_mac_key, &udp_seq, 0).await?;
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
        // Liveness flag. The events task is the canonical signal that the
        // session has ended: when the server-side stream closes (graceful
        // shutdown *or* an admin kick) the task flips this to `false`. The
        // runtime loop reaps a session whose `alive` has gone false so a
        // subsequent reconnect isn't swallowed by the already-connected
        // guard in `Cmd::Connect`.
        let alive = Arc::new(AtomicBool::new(true));
        let alive_for_events = alive.clone();
        let self_id_for_events = client_id.clone();
        let ptt_atomic = Arc::new(AtomicBool::new(false));
        let ptt_for_events = ptt_atomic.clone();
        // Server-side mute flag. The events task sets it from a
        // `MuteChanged` addressed to us; the mic loop reads it.
        let self_muted = Arc::new(AtomicBool::new(false));
        let self_muted_for_events = self_muted.clone();
        // The events task only emits roger beeps / cues, which go on the
        // effects ring (so the voice path's catch-up can't drop them).
        let effects_for_events = effects.clone();
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
                                st.broadcast_active = false;
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
                                // Track whether the active floor is a broadcast so
                                // the UI can tint it distinctly. Set on a broadcast
                                // press; always cleared when the floor frees.
                                st.broadcast_active = new_holder.is_some() && p.broadcast;
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
                                push_playback(&effects_for_events, &tone);
                                state_for_events
                                    .lock()
                                    .unwrap()
                                    .log(format!("⚡ Preempted by {talker_name}"));
                            } else if p.broadcast && (acquired || took_over) {
                                // A global broadcast acquired the floor —
                                // play the distinct three-step falling cue
                                // so listeners know this is fleet-wide.
                                // Supersedes the priority roger check below.
                                let tone = audio::beep_pattern(
                                    audio::BROADCAST_ROGER,
                                    beeps_for_events.volume(),
                                );
                                push_playback(&effects_for_events, &tone);
                                state_for_events
                                    .lock()
                                    .unwrap()
                                    .log(format!("📡 BROADCAST: {talker_name}"));
                            } else if p.priority && (acquired || took_over) {
                                // A priority speaker took the floor (idle-grant
                                // or preemption). Everyone still listening hears
                                // the fixed two-tone priority roger.
                                let tone = audio::beep_pattern(
                                    audio::PRIORITY_ROGER,
                                    beeps_for_events.volume(),
                                );
                                push_playback(&effects_for_events, &tone);
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
                                push_playback(&effects_for_events, &tone);
                                state_for_events
                                    .lock()
                                    .unwrap()
                                    .log(format!("🔒 {talker_name} took the floor"));
                            } else if released && !p.broadcast {
                                // Normal floor-clear cue. Suppressed when the
                                // release belongs to a broadcast session
                                // (`p.broadcast == true`) — the teardown is
                                // silent on the listener side; only the
                                // acquire deserves an audible signal.
                                let preset = beeps_for_events.current_preset();
                                let tone = audio::beep_pattern(
                                    preset.release.steps,
                                    beeps_for_events.volume(),
                                );
                                push_playback(&effects_for_events, &tone);
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
                            // Drop any name + mute carried from the old
                            // channel; the new room's ChannelNameChanged
                            // (if named + feature on) and ChannelMuteChanged
                            // land right after this event. Clearing the mute
                            // here is what makes "move away from a muted
                            // channel and you can talk again" feel instant.
                            st.channel_name = None;
                            st.channel_muted = false;
                            // Priority is per-channel; the server re-asserts
                            // it for the new freq via PriorityChanged right
                            // after this. Clear so a grant bound to the old
                            // channel doesn't leak its No-Talk exemption here.
                            st.channel_priority = false;
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
                        Some(Ev::ChannelNameChanged(cnc)) => {
                            // Label (or relabel/clear) the current channel.
                            // Ignore stale events for a frequency we've
                            // since left. Defensively trim + cap at 16
                            // chars even though the server enforces it.
                            let mut st = state_for_events.lock().unwrap();
                            if st.frequency.as_deref() == Some(cnc.frequency.as_str()) {
                                let trimmed = cnc.name.trim();
                                let name: Option<String> = if trimmed.is_empty() {
                                    None
                                } else {
                                    Some(trimmed.chars().take(16).collect())
                                };
                                match &name {
                                    Some(n) => st.log(format!("🏷 channel “{n}”")),
                                    None => st.log("🏷 channel name cleared"),
                                }
                                st.channel_name = name;
                            }
                        }
                        Some(Ev::MuteChanged(mc)) => {
                            // Track every member's mute state for the
                            // roster badge; when it's *us*, also slam our
                            // local PTT gate shut (so we stop uploading
                            // frames the server will drop anyway) and log
                            // a clear operator cue.
                            let mut st = state_for_events.lock().unwrap();
                            st.set_muted(&mc.client_id, mc.muted);
                            if mc.client_id == self_id_for_events {
                                self_muted_for_events.store(mc.muted, Ordering::Relaxed);
                                if mc.muted {
                                    ptt_for_events.store(false, Ordering::Relaxed);
                                    st.log("🔇 You were muted by an operator");
                                } else {
                                    st.log("🔊 An operator unmuted you");
                                }
                            }
                        }
                        Some(Ev::ChannelMuteChanged(cmc)) => {
                            // The whole channel was muted/unmuted (or the
                            // current state delivered on join). Apply only
                            // when it's for the frequency we're tuned to,
                            // then drive the same local consequences as a
                            // personal mute: stop our mic and show the cue.
                            let mut st = state_for_events.lock().unwrap();
                            if st.frequency.as_deref() == Some(cmc.frequency.as_str()) {
                                let was = st.channel_muted;
                                st.channel_muted = cmc.muted;
                                // Our overall "can I talk" state folds
                                // member-mute, channel-mute, and the
                                // priority exception; mirror it into the
                                // session gate so the mic loop sees it
                                // immediately.
                                let silenced = st.locally_silenced();
                                self_muted_for_events.store(silenced, Ordering::Relaxed);
                                // Only drop a held press if the mute actually
                                // silences *us* — a priority speaker on this
                                // channel keeps the floor.
                                if silenced {
                                    ptt_for_events.store(false, Ordering::Relaxed);
                                }
                                if cmc.muted && !was {
                                    if silenced {
                                        st.log("🔇 This channel was muted by an operator");
                                    } else {
                                        st.log("🔇 Channel muted — you may still talk (priority)");
                                    }
                                } else if !cmc.muted && was {
                                    st.log("🔊 This channel was unmuted");
                                }
                            }
                        }
                        Some(Ev::PriorityChanged(pc)) => {
                            // Our priority standing on a channel changed
                            // (or was delivered on change-frequency).
                            // Apply only when it's addressed to us and for
                            // the frequency we're tuned to. Priority is the
                            // No-Talk exception: a priority speaker keeps a
                            // live PTT button on a muted channel, so this
                            // can *re-open* the gate that channel-mute shut.
                            let mut st = state_for_events.lock().unwrap();
                            let for_us = pc.client_id == self_id_for_events;
                            let for_here = st.frequency.as_deref() == Some(pc.frequency.as_str());
                            if for_us && for_here {
                                let was = st.channel_priority;
                                st.channel_priority = pc.granted;
                                // Re-mirror the combined gate so the mic
                                // loop reflects the new standing at once.
                                self_muted_for_events
                                    .store(st.locally_silenced(), Ordering::Relaxed);
                                if pc.granted && !was {
                                    st.log("⚡ You are a priority speaker here");
                                } else if !pc.granted && was {
                                    st.log("⚡ Priority speaker status removed");
                                }
                            }
                        }
                        Some(Ev::BroadcastCapabilityChanged(bc)) => {
                            // The server granted or revoked our global-broadcast
                            // capability (or delivered the current standing on
                            // join / change-frequency). Addressed only to us
                            // (the server filters by client_id server-side, but
                            // we double-check here for belt-and-braces safety).
                            // Session-scoped: we do NOT clear it on channel
                            // change — the server re-asserts it via this event.
                            let mut st = state_for_events.lock().unwrap();
                            if bc.client_id == self_id_for_events {
                                st.can_broadcast = bc.granted;
                                st.log(if bc.granted {
                                    "📡 Global broadcast capability granted".to_string()
                                } else {
                                    "📡 Global broadcast capability revoked".to_string()
                                });
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
            // Mark the session dead first so the runtime loop reaps the
            // stale `Session` and a reconnect is accepted (otherwise the
            // client is stuck on the offline screen until app restart).
            alive_for_events.store(false, Ordering::Relaxed);
            let mut st = state_for_events.lock().unwrap();
            st.connection = crate::state::ConnState::Disconnected;
            st.members.clear();
            st.holder = None;
            st.frequency = None;
            st.channel_name = None;
            st.conn_quality = None;
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
        // Connection-quality tracker lives on the recv task — it's the
        // single writer for loss/jitter (from inbound audio seqs/timing)
        // and RTT (from pongs, which arrive on this same socket). The
        // UI + the periodic report task read the published handle.
        let (mut quality, quality_handle) = QualityTracker::new();
        let udp_for_recv = udp.clone();
        let key_for_recv = audio_mac_key;
        let recv_task = tokio::spawn(async move {
            let mut buf = vec![0u8; MAX_AUDIO_PACKET];
            let cipher = ChaCha20Poly1305::new(Key::from_slice(&key_for_recv));
            // One reusable Opus decoder for the inbound stream. Only one
            // peer transmits per channel at a time, so a single decoder
            // is fine; `None` if libopus init somehow fails (Opus frames
            // are then dropped, PCM still plays).
            let mut decoder = audiopus::coder::Decoder::new(
                audiopus::SampleRate::Hz48000,
                audiopus::Channels::Mono,
            )
            .ok();
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
                        // S2C layout: version (1) | seq (8) | tag (16) | ciphertext
                        let version = buf[0];
                        let seq_bytes: [u8; SEQ_LEN] = buf[1..1 + SEQ_LEN]
                            .try_into()
                            .expect("slice has SEQ_LEN bytes");
                        let seq = u64::from_le_bytes(seq_bytes);
                        let tag_bytes: [u8; TAG_LEN] = buf[1 + SEQ_LEN..1 + SEQ_LEN + TAG_LEN]
                            .try_into()
                            .expect("slice has TAG_LEN bytes");
                        let mut plaintext = buf[HEADER_LEN_S2C..n].to_vec();
                        let nonce_bytes = build_nonce(seq);
                        let nonce = Nonce::from_slice(&nonce_bytes);
                        let tag = Tag::from_slice(&tag_bytes);
                        // AAD is the relayed codec version (matches the
                        // server's seal), so a tampered version byte fails
                        // the tag check.
                        if cipher
                            .decrypt_in_place_detached(nonce, &[version], &mut plaintext, tag)
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

                        // PONG: the server bounced our RTT probe back.
                        // Compute round-trip from the echoed send time and
                        // feed the tracker. Not audio — never reaches the
                        // decoder or playback.
                        if version == VERSION_PONG {
                            if let Some((_ping_id, send_ts)) = decode_ping(&plaintext) {
                                let rtt_ms =
                                    now_unix_micros().saturating_sub(send_ts) as f64 / 1000.0;
                                quality.on_rtt(rtt_ms);
                            }
                            continue;
                        }

                        // Decode per the codec the sender used. Radio FX is
                        // *not* applied here — it's a transmit-side effect now,
                        // baked into the audio by the sender, so incoming voice
                        // already carries whatever FX the sender chose.
                        let samples = match version {
                            VERSION_AUDIO_PCM => pcm_from_bytes(&plaintext),
                            VERSION_AUDIO_OPUS => decode_opus(&mut decoder, &plaintext),
                            other => {
                                debug!(version = other, "unknown audio codec, dropping");
                                continue;
                            }
                        };
                        // Feed the quality tracker: this seq advanced the
                        // S2C counter (loss = gaps) and arrived now
                        // (jitter = spacing deviation).
                        quality.on_audio(seq, Instant::now());
                        // Latency-managed: keeps the voice backlog tight
                        // so playback can't fall progressively behind.
                        push_voice(&playback, &samples);
                    }
                    Err(e) => {
                        warn!(error = %e, "udp recv error");
                        break;
                    }
                }
            }
        });

        // ── Connection-quality report ─────────────────────────────────
        // Push the locally-measured metrics up to the server every few
        // seconds so the admin dashboard can show this client's link
        // health. Fire-and-forget: a failed report just waits for the
        // next tick. Skips reporting until at least one metric exists.
        let mut signaling_for_report = signaling.clone();
        let report_client_id = client_id.clone();
        let quality_for_report = quality_handle.clone();
        let report_task = tokio::spawn(async move {
            let mut tick = tokio::time::interval(Duration::from_secs(3));
            tick.tick().await; // skip the immediate tick
            loop {
                tick.tick().await;
                let q = quality_for_report.snapshot();
                if !q.fresh {
                    continue;
                }
                let req = toki_proto::v1::ConnectionQualityReport {
                    client_id: report_client_id.clone(),
                    rtt_ms: q.rtt_ms,
                    jitter_ms: q.jitter_ms,
                    loss_pct_centi: q.loss_pct_centi,
                };
                if let Err(e) = signaling_for_report.report_connection_quality(req).await {
                    debug!(error = %e, "connection-quality report failed");
                }
            }
        });

        // Publish the quality handle to the UI via shared state.
        state.lock().unwrap().conn_quality = Some(quality_handle);

        // ── Keepalives ────────────────────────────────────────────────
        let udp_for_keepalive = udp.clone();
        let token_for_keepalive = audio_token.clone();
        let key_for_keepalive = audio_mac_key;
        let seq_for_keepalive = udp_seq.clone();
        let keepalive_task = tokio::spawn(async move {
            let mut tick = tokio::time::interval(Duration::from_secs(3));
            tick.tick().await; // consume the immediate first tick
            let mut ping_id: u64 = 0;
            loop {
                tick.tick().await;
                ping_id = ping_id.wrapping_add(1);
                if let Err(e) = send_keepalive(
                    &udp_for_keepalive,
                    &token_for_keepalive,
                    &key_for_keepalive,
                    &seq_for_keepalive,
                    ping_id,
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
            broadcast_local_pressed: AtomicBool::new(false),
            broadcast_cooldown_until: StdMutex::new(None),
            udp,
            signaling,
            encode: StdMutex::new(AudioEncoder::new(
                opus_enabled,
                opus_bitrate,
                opus_dtx,
                opus_frame_ms,
            )),
            alive,
            self_muted,
            tasks: vec![
                events_task,
                ptt_task,
                recv_task,
                keepalive_task,
                report_task,
            ],
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
        st.channel_name = None;
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
            // Normal PTT is never a broadcast; the server is the arbiter
            // for the fan-out flag.
            broadcast: false,
        };
        if let Err(e) = self.ptt_tx.send(evt).await {
            warn!(error = %e, "ptt send failed");
        }
    }

    /// Request a global-broadcast PTT state change. Works like
    /// [`Session::request_ptt`] but sends `broadcast: true` and is
    /// gated on `state.can_broadcast` — the hotkey is silently inert
    /// if the admin hasn't granted capability. Uses separate debounce
    /// state (`broadcast_local_pressed` / `broadcast_cooldown_until`)
    /// so it can't interfere with the normal PTT gate.
    async fn request_broadcast_ptt(&self, pressed: bool, state: &SharedState) {
        // Capability gate: if the admin hasn't granted broadcast capability,
        // the hotkey does nothing. Re-checked on every press so a revocation
        // that lands mid-hold takes effect on the next key event.
        if !state.lock().unwrap().can_broadcast {
            return;
        }

        if pressed {
            // Already pressing broadcast PTT? Drop duplicate down.
            if self.broadcast_local_pressed.load(Ordering::Relaxed) {
                return;
            }
            // Inside the post-release cooldown? Drop.
            let until = *self.broadcast_cooldown_until.lock().unwrap();
            if let Some(t) = until {
                if Instant::now() < t {
                    return;
                }
            }
            self.broadcast_local_pressed.store(true, Ordering::Relaxed);
        } else {
            // Orphan up (no matching down) — ignore.
            if !self.broadcast_local_pressed.swap(false, Ordering::Relaxed) {
                return;
            }
            *self.broadcast_cooldown_until.lock().unwrap() = Some(Instant::now() + PTT_COOLDOWN);
        }

        let seq = self.seq.fetch_add(1, Ordering::Relaxed);
        let evt = PttEvent {
            client_id: self.client_id.clone(),
            pressed,
            sequence: seq,
            priority: false,
            // This is the broadcast flag the server routes fleet-wide.
            broadcast: true,
        };
        if let Err(e) = self.ptt_tx.send(evt).await {
            warn!(error = %e, "broadcast ptt send failed");
        }
    }

    async fn send_audio(&self, samples: &[i16]) {
        // Encode under the lock (fast, no await), then seal + send each
        // resulting packet. Both codecs yield one packet per 10 ms mic
        // frame (Opus just makes it ~20× smaller).
        let packets = self.encode.lock().unwrap().push(samples);
        for (version, payload) in packets {
            let pkt = build_authenticated_packet(
                &self.audio_token,
                version,
                &self.audio_mac_key,
                &self.udp_seq,
                &payload,
            );
            if let Err(e) = self.udp.send(&pkt).await {
                warn!(error = %e, "udp send failed");
            }
        }
    }

    /// Flush the outbound encoder's trailing partial frame on PTT
    /// release so the end of speech isn't clipped and the next
    /// transmission starts from a clean buffer. See [`AudioEncoder::flush`].
    async fn flush_audio(&self) {
        let packets = self.encode.lock().unwrap().flush();
        for (version, payload) in packets {
            let pkt = build_authenticated_packet(
                &self.audio_token,
                version,
                &self.audio_mac_key,
                &self.udp_seq,
                &payload,
            );
            if let Err(e) = self.udp.send(&pkt).await {
                warn!(error = %e, "udp flush send failed");
            }
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
    ping_id: u64,
) -> Result<()> {
    // Carry an RTT probe (ping id + send timestamp) the server echoes in
    // a PONG so the recv task can measure round-trip time.
    let probe = encode_ping(ping_id, now_unix_micros());
    let pkt = build_authenticated_packet(token, VERSION_KEEPALIVE, mac_key, udp_seq, &probe);
    debug_assert_eq!(probe.len(), PING_LEN);
    udp.send(&pkt).await?;
    Ok(())
}

/// Wall-clock microseconds since the Unix epoch, for the RTT probe
/// timestamp. The server echoes this verbatim and the recv task diffs it
/// against `now` — so only this client's clock matters (no cross-host
/// sync needed); a backwards clock step just yields one discarded
/// sample. Saturates to 0 before 1970 (never happens).
fn now_unix_micros() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_micros() as u64)
        .unwrap_or(0)
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

/// Decode one Opus packet to 48 kHz mono i16. The output buffer is sized
/// at `OPUS_FRAME_SAMPLES * 4` (1920 samples) to cover the largest valid
/// Opus frame: 40 ms at 48 kHz = 1920 samples. A 30 ms buffer (`* 3`)
/// would overflow on a 40 ms frame. The decoder's returned sample count
/// is used to truncate `out` to the true decoded length.
/// Returns empty on a decoder error or when the decoder is unavailable —
/// playback treats it as loss.
fn decode_opus(decoder: &mut Option<audiopus::coder::Decoder>, packet: &[u8]) -> Vec<i16> {
    let Some(dec) = decoder.as_mut() else {
        return Vec::new();
    };
    let mut out = vec![0i16; OPUS_FRAME_SAMPLES * 4];
    match dec.decode(Some(packet), &mut out[..], false) {
        Ok(samples) => {
            out.truncate(samples);
            out
        }
        Err(e) => {
            warn!(error = %e, "Opus decode failed, dropping");
            Vec::new()
        }
    }
}

/// Resolve the server's advertised audio endpoint to a concrete `SocketAddr`.
///
/// We take the **host** from what the server advertises but the **port**
/// from the signaling (gRPC) URL we're already connected to — audio always
/// rides the same port as gRPC. Toki's convention is one port number for
/// both (TCP gRPC + UDP audio are distinct kernel binding tuples), so the
/// advertised port is redundant with the gRPC port in every correct
/// deployment; deriving it here means a server that advertises a stale or
/// mismatched audio port (e.g. gRPC moved to `:50052` but `TOKI_AUDIO_PUBLIC`
/// still says `:50051`) can't strand the client's UDP on a dead port. A
/// mismatch is logged at warn so a deliberately-split deployment is visible.
///
/// The server may advertise:
/// - a routable numeric address (`203.0.113.5:50051`) → host used, gRPC port;
/// - an **unspecified** address (`0.0.0.0:port` / `[::]:port`), which isn't
///   routable from a client → substitute the host of the signaling URL;
/// - a **DNS name** (`toki.example.org:50051`), when the operator set
///   `TOKI_AUDIO_PUBLIC` to a hostname → host resolved, gRPC port.
///
/// In every host-based branch the host can be a DNS name, so we resolve via
/// [`lookup_host`] rather than `parse::<SocketAddr>()` — the latter only
/// accepts IP literals and fails on names with "invalid socket address
/// syntax". When substituting for an unspecified address we keep the
/// advertised IP family (don't reach a v4 relay over a v6 record, or vice
/// versa) if the name resolves to both.
async fn resolve_audio_endpoint(advertised: &str, signaling_url: &str) -> Result<SocketAddr> {
    // The port audio will actually use: the gRPC port, always.
    let port = signaling_port(signaling_url)?;
    // Pull the host out of the advertisement (plus its port, only to warn on
    // a mismatch, and its family, to bias DNS when we substitute a host).
    let (host, advertised_port, want_ipv6): (String, u16, Option<bool>) =
        match advertised.parse::<SocketAddr>() {
            // Routable numeric address — use its host (resolved below skips
            // DNS for an IP literal), advertised port noted for the warning.
            Ok(addr) if !addr.ip().is_unspecified() => {
                (addr.ip().to_string(), addr.port(), Some(addr.is_ipv6()))
            }
            // Unspecified (0.0.0.0 / [::]) — substitute the signaling host,
            // keep the advertised family.
            Ok(addr) => (
                signaling_host(signaling_url)?.to_string(),
                addr.port(),
                Some(addr.is_ipv6()),
            ),
            // Not a numeric address — treat as `host:port` (a DNS name).
            Err(_) => {
                let (h, p) = advertised
                    .rsplit_once(':')
                    .ok_or_else(|| anyhow!("audio endpoint missing port: {advertised:?}"))?;
                let advertised_port: u16 = p
                    .parse()
                    .with_context(|| format!("audio endpoint port {p:?}"))?;
                (strip_brackets(h).to_string(), advertised_port, None)
            }
        };

    if advertised_port != port {
        warn!(
            advertised = advertised,
            advertised_port,
            grpc_port = port,
            "server advertised a different audio port than gRPC; using the gRPC port"
        );
    }

    // A routable numeric host needs no DNS — build it directly with the gRPC
    // port (parse can't fail: numeric host + valid u16).
    if let Ok(ip) = host.parse::<std::net::IpAddr>() {
        return Ok(SocketAddr::new(ip, port));
    }

    let addrs: Vec<SocketAddr> = lookup_host((host.as_str(), port))
        .await
        .with_context(|| format!("resolve {host}:{port}"))?
        .collect();
    // Prefer the advertised family when we substituted, else first result.
    if let Some(v6) = want_ipv6 {
        if let Some(matching) = addrs.iter().find(|a| a.is_ipv6() == v6) {
            return Ok(*matching);
        }
    }
    addrs
        .into_iter()
        .next()
        .ok_or_else(|| anyhow!("no addresses resolved for {host}:{port}"))
}

/// Extract the host from a signaling URL like `https://host:port/...`.
fn signaling_host(signaling_url: &str) -> Result<&str> {
    let host_port = signaling_url
        .trim_start_matches("http://")
        .trim_start_matches("https://")
        .split('/')
        .next()
        .ok_or_else(|| anyhow!("empty signaling url"))?;
    host_port
        .rsplit_once(':')
        .map(|(host, _port)| strip_brackets(host))
        .ok_or_else(|| anyhow!("signaling url missing port"))
}

/// Extract the port from a signaling URL like `https://host:port/...`.
/// Audio reuses this so its UDP endpoint always rides the gRPC port (see
/// [`resolve_audio_endpoint`]). The URL always carries an explicit port
/// here — the client builds it from the user's `host` + `port` config —
/// so a missing one is a hard error rather than an http/https default.
fn signaling_port(signaling_url: &str) -> Result<u16> {
    let host_port = signaling_url
        .trim_start_matches("http://")
        .trim_start_matches("https://")
        .split('/')
        .next()
        .ok_or_else(|| anyhow!("empty signaling url"))?;
    let (_host, port) = host_port
        .rsplit_once(':')
        .ok_or_else(|| anyhow!("signaling url missing port: {signaling_url:?}"))?;
    port.parse()
        .with_context(|| format!("signaling url port {port:?}"))
}

/// Drop surrounding `[ ]` from an IPv6 literal host (`[::1]` → `::1`); a
/// no-op for hostnames and IPv4 literals.
fn strip_brackets(host: &str) -> &str {
    host.strip_prefix('[')
        .and_then(|h| h.strip_suffix(']'))
        .unwrap_or(host)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn pcm_encoder_emits_one_packet_per_frame() {
        let mut enc = AudioEncoder::new(false, 0, false, 10);
        let pkts = enc.push(&vec![0i16; 480]);
        assert_eq!(pkts.len(), 1);
        assert_eq!(pkts[0].0, VERSION_AUDIO_PCM);
        assert_eq!(pkts[0].1.len(), 960, "480 samples × 2 bytes");
    }

    #[test]
    fn opus_emits_one_packet_per_10ms_frame_and_decodes() {
        let mut enc = AudioEncoder::new(true, 24_000, false, 10);
        // Each 10 ms (480-sample) mic frame yields exactly one Opus
        // packet — same cadence as the mic, no buffering.
        let pkts = enc.push(&vec![1000i16; 480]);
        assert_eq!(pkts.len(), 1);
        assert_eq!(pkts[0].0, VERSION_AUDIO_OPUS);
        assert!(
            !pkts[0].1.is_empty() && pkts[0].1.len() <= MAX_OPUS_PAYLOAD,
            "opus payload bounded, got {}",
            pkts[0].1.len()
        );
        // And it decodes back to a full 10 ms (480-sample) frame.
        let mut dec = Some(
            audiopus::coder::Decoder::new(audiopus::SampleRate::Hz48000, audiopus::Channels::Mono)
                .unwrap(),
        );
        let out = decode_opus(&mut dec, &pkts[0].1);
        assert_eq!(out.len(), OPUS_FRAME_SAMPLES);
    }

    #[test]
    fn opus_20ms_frame_encodes_and_decodes() {
        // Server advertises 20 ms frames (960 samples). Pushing exactly
        // 960 samples must yield exactly one packet, which decodes back to
        // 960 samples.
        let mut enc = AudioEncoder::new(true, 24_000, false, 20);
        let pkts = enc.push(&vec![1000i16; 960]);
        assert_eq!(pkts.len(), 1, "960 samples at 20 ms frame → 1 packet");
        assert_eq!(pkts[0].0, VERSION_AUDIO_OPUS);
        let mut dec = Some(
            audiopus::coder::Decoder::new(audiopus::SampleRate::Hz48000, audiopus::Channels::Mono)
                .unwrap(),
        );
        let out = decode_opus(&mut dec, &pkts[0].1);
        assert_eq!(out.len(), 960, "20 ms frame decodes to 960 samples");
    }

    #[test]
    fn opus_40ms_frame_encodes_and_decodes() {
        // Server advertises 40 ms frames (1920 samples). Pushing exactly
        // 1920 samples must yield exactly one packet, which decodes back to
        // 1920 samples. This also exercises the decode buffer fix: a 30 ms
        // buffer (OPUS_FRAME_SAMPLES * 3 = 1440) would overflow here.
        let mut enc = AudioEncoder::new(true, 24_000, false, 40);
        let pkts = enc.push(&vec![1000i16; 1920]);
        assert_eq!(pkts.len(), 1, "1920 samples at 40 ms frame → 1 packet");
        assert_eq!(pkts[0].0, VERSION_AUDIO_OPUS);
        let mut dec = Some(
            audiopus::coder::Decoder::new(audiopus::SampleRate::Hz48000, audiopus::Channels::Mono)
                .unwrap(),
        );
        let out = decode_opus(&mut dec, &pkts[0].1);
        assert_eq!(out.len(), 1920, "40 ms frame decodes to 1920 samples");
    }

    /// Egress benchmark: encode a realistic speech-with-silence stream
    /// through the *real* Opus encoder with DTX off vs on, and report the
    /// before/after bytes + the per-listener egress projection. Run with
    /// `cargo test -p toki-client opus_dtx_egress -- --nocapture` to see
    /// the numbers. The assertion is a regression guard (DTX must save a
    /// meaningful fraction); the printout is the actual measurement.
    #[test]
    fn opus_dtx_egress_before_after() {
        // Model one PTT transmission as a sequence of 10 ms frames:
        // alternating ~600 ms voiced bursts and ~500 ms silent gaps —
        // ~55% active, typical of push-to-talk speech with inter-sentence
        // pauses. Voiced frames are a 300 Hz-band tone + light noise (Opus
        // treats them as active); silent frames are digital zeros (which
        // Opus's VAD reliably routes to DTX comfort-noise).
        const VOICED_FRAMES: usize = 60; // 600 ms
        const SILENT_FRAMES: usize = 50; // 500 ms
        const CYCLES: usize = 10; // ~11 s of "talk"
        let voiced = |phase: &mut f32| -> Vec<i16> {
            let step = 2.0 * std::f32::consts::PI * 300.0 / 48_000.0;
            (0..480)
                .map(|i| {
                    let s = (*phase).sin() * 6000.0;
                    *phase += step;
                    // a little deterministic noise so it isn't a pure tone
                    let n = ((i as u32).wrapping_mul(2_654_435_761) >> 24) % 400;
                    (s + n as f32) as i16
                })
                .collect()
        };
        let silent = vec![0i16; 480];

        // Encode the same stream twice; sum the Opus payload bytes.
        let run = |dtx: bool| -> (usize, usize) {
            let mut enc = AudioEncoder::new(true, 24_000, dtx, 10);
            let mut phase = 0.0f32;
            let (mut bytes, mut packets) = (0usize, 0usize);
            for _ in 0..CYCLES {
                for _ in 0..VOICED_FRAMES {
                    for (_v, p) in enc.push(&voiced(&mut phase)) {
                        bytes += p.len();
                        packets += 1;
                    }
                }
                for _ in 0..SILENT_FRAMES {
                    for (_v, p) in enc.push(&silent) {
                        bytes += p.len();
                        packets += 1;
                    }
                }
            }
            (bytes, packets)
        };

        let (off_bytes, off_pkts) = run(false);
        let (on_bytes, on_pkts) = run(true);

        // On-wire size adds the S2C app header (HEADER_LEN_S2C = 25) plus
        // ~28 bytes of IPv4+UDP overhead per packet. With DTX, packets the
        // encoder *skips* during silence cost zero on the wire too.
        const PER_PKT_OVERHEAD: usize = toki_proto::wire::HEADER_LEN_S2C + 28;
        let on_wire = |bytes: usize, pkts: usize| bytes + pkts * PER_PKT_OVERHEAD;
        let off_wire = on_wire(off_bytes, off_pkts);
        let on_wire_b = on_wire(on_bytes, on_pkts);

        let total_frames = CYCLES * (VOICED_FRAMES + SILENT_FRAMES);
        let secs = total_frames as f64 / 100.0;
        // Per-listener egress = on-wire bytes / stream duration.
        let kbps = |w: usize| (w as f64 * 8.0) / secs / 1000.0;
        let saving = 1.0 - (on_wire_b as f64 / off_wire as f64);

        eprintln!("\n=== Opus DTX egress: before/after ===");
        eprintln!(
            "stream: {total_frames} frames ({secs:.1}s), {}% active",
            (VOICED_FRAMES * 100) / (VOICED_FRAMES + SILENT_FRAMES)
        );
        eprintln!(
            "DTX off: {off_pkts} pkts, {off_bytes} payload B, {off_wire} on-wire B  → {:.0} kbps/listener",
            kbps(off_wire)
        );
        eprintln!(
            "DTX on : {on_pkts} pkts, {on_bytes} payload B, {on_wire_b} on-wire B  → {:.0} kbps/listener",
            kbps(on_wire_b)
        );
        eprintln!("egress reduction: {:.0}%", saving * 100.0);
        eprintln!("per-room egress projection (1 talker, on-wire ×(N−1)):");
        for n in [10usize, 50, 100] {
            eprintln!(
                "  N={n:3}: off {:6.0} kbps → on {:6.0} kbps",
                kbps(off_wire) * (n - 1) as f64,
                kbps(on_wire_b) * (n - 1) as f64
            );
        }
        eprintln!();

        // Regression guard: DTX must cut a clear chunk of egress on a
        // half-silent stream. (Conservative bound — the measured figure is
        // higher; real-mic silence saves somewhat less than digital zeros.)
        assert!(
            saving > 0.20,
            "DTX saved only {:.0}% — expected >20% on a ~45%-silent stream",
            saving * 100.0
        );
        // And DTX must skip packets during silence (the mechanism).
        assert!(
            on_pkts < off_pkts,
            "DTX on emitted {on_pkts} pkts vs off {off_pkts} — DTX not engaging"
        );
    }

    #[test]
    fn opus_flush_emits_partial_tail_then_clears() {
        let mut enc = AudioEncoder::new(true, 24_000, false, 10);
        // A short, odd-sized frame leaves a partial remainder buffered.
        assert!(
            enc.push(&vec![1000i16; 200]).is_empty(),
            "sub-frame input buffers, nothing emitted"
        );
        // Releasing PTT flushes the buffered tail as one padded frame.
        let pkts = enc.flush();
        assert_eq!(pkts.len(), 1, "partial tail should flush on release");
        assert_eq!(pkts[0].0, VERSION_AUDIO_OPUS);
        // Buffer is now clear: a second flush is a no-op, and a fresh
        // sub-frame buffers again (no stale samples leaked through).
        assert!(enc.flush().is_empty(), "buffer cleared after flush");
        assert!(
            enc.push(&vec![1000i16; 200]).is_empty(),
            "next transmission starts from an empty buffer"
        );
    }

    #[test]
    fn pcm_flush_is_a_noop() {
        let mut enc = AudioEncoder::new(false, 0, false, 10);
        assert!(enc.flush().is_empty(), "PCM carries no buffer to flush");
    }

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

    #[tokio::test]
    async fn resolve_audio_endpoint_takes_host_from_advert_port_from_grpc() {
        // Routable advertised address: keep its host, but the port comes
        // from the gRPC URL (audio always rides the gRPC port).
        let resolved = resolve_audio_endpoint("203.0.113.5:50052", "https://server:50051")
            .await
            .unwrap();
        assert_eq!(resolved.to_string(), "203.0.113.5:50051");
    }

    #[tokio::test]
    async fn resolve_audio_endpoint_overrides_stale_advertised_port() {
        // The staging bug: gRPC moved to :50052 but the server still
        // advertises audio on :50051 (stale TOKI_AUDIO_PUBLIC). Audio must
        // follow gRPC to :50052, not strand UDP on the dead :50051.
        let resolved = resolve_audio_endpoint("163.172.79.96:50051", "https://host:50052")
            .await
            .unwrap();
        assert_eq!(resolved.to_string(), "163.172.79.96:50052");
    }

    #[tokio::test]
    async fn resolve_audio_endpoint_substitutes_signaling_host_for_unspecified() {
        // Server commonly advertises 0.0.0.0:port; rewrite to the host
        // portion of the gRPC URL (an IP literal here) on the gRPC port.
        let resolved = resolve_audio_endpoint("0.0.0.0:50052", "https://192.168.1.50:50051")
            .await
            .unwrap();
        assert_eq!(resolved.to_string(), "192.168.1.50:50051");
    }

    #[tokio::test]
    async fn resolve_audio_endpoint_resolves_dns_host_for_unspecified() {
        // The regression: a *named* signaling host must be DNS-resolved,
        // not bare-parsed (parse::<SocketAddr>() rejects names). `localhost`
        // resolves offline via the hosts file; assert we get a loopback addr
        // on the gRPC port.
        let resolved = resolve_audio_endpoint("0.0.0.0:50052", "https://localhost:50051")
            .await
            .unwrap();
        assert!(resolved.ip().is_loopback(), "got {resolved}");
        assert_eq!(resolved.port(), 50051);
    }

    #[tokio::test]
    async fn resolve_audio_endpoint_resolves_advertised_dns_name() {
        // Operator set TOKI_AUDIO_PUBLIC to a hostname (not an IP); host is
        // resolved, port is taken from the gRPC URL.
        let resolved = resolve_audio_endpoint("localhost:50052", "https://ignored:50051")
            .await
            .unwrap();
        assert!(resolved.ip().is_loopback(), "got {resolved}");
        assert_eq!(resolved.port(), 50051);
    }

    #[tokio::test]
    async fn resolve_audio_endpoint_rejects_malformed_input() {
        assert!(resolve_audio_endpoint("nope", "https://server:50051")
            .await
            .is_err());
    }

    #[tokio::test]
    async fn resolve_audio_endpoint_errors_when_signaling_url_has_no_port() {
        // The gRPC port is now load-bearing for audio, so a portless
        // signaling URL is a hard error rather than a silent default.
        assert!(resolve_audio_endpoint("0.0.0.0:50051", "https://server")
            .await
            .is_err());
    }

    #[test]
    fn signaling_port_parses_and_rejects() {
        assert_eq!(signaling_port("https://host:50052").unwrap(), 50052);
        assert_eq!(signaling_port("http://1.2.3.4:50051/x").unwrap(), 50051);
        assert!(signaling_port("https://host").is_err());
        assert!(signaling_port("https://host:notaport").is_err());
    }
}
