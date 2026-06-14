//! The Strip widget — landscape walkie-talkie UI per `design/`.
//!
//! Architecture: the runtime owns network + audio state; the GUI reads
//! a snapshot each frame and paints accordingly. All custom widgets
//! (waveform, knob, PTT button, OLED panels) are painted directly via
//! `Painter` rather than composed from egui's built-in widgets — the
//! design has too much custom chrome (glows, scanlines, gradients) to
//! be expressible through the default widget set.

use std::time::{Duration, Instant};

use eframe::egui::{
    self, Align2, Color32, CornerRadius, FontFamily, FontId, Pos2, Rect, Sense, Shape, Stroke,
    StrokeKind, Vec2,
};

use std::sync::Arc;

use rustfft::{num_complex::Complex, Fft, FftPlanner};

use crate::audio::{
    self, AudioControl, AudioDevices, AudioGains, AudioLevels, AudioSpectrum, BeepParams,
    BeepPreset,
};
use crate::config::{self, HotkeyBinding};
use crate::dsp::DspParams;
use crate::hotkey::{self, InstalledHotkey};
use crate::runtime::{self, Cmd};
use crate::state::{self, ConnState, SharedState};
use crate::theme as T;
use crate::update;

/// Logical UI state derived from the runtime snapshot + local hold flag.
/// Mirrors the six states in `design/behavior-spec.md` — `offline` and
/// `reconnecting` are transport-layer states that suppress all radio
/// activity; the other four describe normal half-duplex behavior on a
/// healthy connection.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum RadioState {
    Offline,
    Reconnecting,
    Idle,
    Tx,
    Rx,
    Busy,
}

/// Which audio direction a `paint_knob` call drives. Both knobs share
/// the same visuals — only the underlying field, the apply path
/// (input vs output gain), and the indicator colour-when-muted differ.
#[derive(Clone, Copy)]
enum KnobKind {
    /// Input (microphone) gain. Applied unconditionally; mute is an
    /// output-only concept.
    Mic,
    /// Output (speaker) gain. While muted, the knob updates
    /// `config.audio.output_gain` and `gain_before_mute` so the next
    /// unmute reflects the user's chosen level — but does *not* call
    /// `audio_gains.set_output`, which would defeat the mute.
    Speaker,
    /// Stereo playback balance in `[-1, 1]`, centred at 0. Routes
    /// received audio toward the left or right ear — a mono-earpiece
    /// effect for walkie-talkie listening. Unlike the gain knobs its
    /// detent sits at 12 o'clock (centre), not full-left.
    Balance,
}

impl RadioState {
    /// While the radio is in one of these states the user can't TX,
    /// can't switch channels, and the center OLED + PTT button are
    /// swapped for the offline/reconnect surfaces.
    fn is_transport_down(self) -> bool {
        matches!(self, RadioState::Offline | RadioState::Reconnecting)
    }
}

/// Buffered values for the Connect dialog. Edits stay local until the
/// user clicks `CONNECT`, at which point they're committed to
/// `config.connection` and persisted. Cancelling drops the edits.
struct ConnectForm {
    host: String,
    port: u16,
    username: String,
    /// Shared-secret password for servers that gate registration.
    /// Empty when the target server is open-mode. Rendered with
    /// `egui::TextEdit::password(true)` so the entry is masked.
    password: String,
    /// Buffered text view of `port` while the user is typing. We
    /// only parse back into `port` when the field loses focus, so
    /// partial entries like `"5005"` mid-typing don't keep
    /// snapping to a default. Empty after construction means
    /// "(re)hydrate from `port`".
    port_text: String,
}

impl Default for ConnectForm {
    fn default() -> Self {
        Self {
            host: String::new(),
            port: 50051,
            username: String::new(),
            password: String::new(),
            port_text: String::new(),
        }
    }
}

pub struct TokiApp {
    state: SharedState,
    cmd_tx: tokio::sync::mpsc::UnboundedSender<Cmd>,

    config: config::Config,
    hotkey: InstalledHotkey,
    recording: bool,
    /// Which binding the in-progress recording will write to when the
    /// poller captures a key. Meaningful only while `recording`.
    recording_target: RecordTarget,

    audio_devices: AudioDevices,
    audio_control: AudioControl,
    audio_gains: AudioGains,
    /// Live atomics behind the roger-beep parameters. Cloned at app
    /// startup and shared with the runtime so a slider tweak in
    /// Settings takes effect on the next take-floor / clear-floor
    /// event without a reconnect.
    beep_params: BeepParams,
    /// Live atomics behind the capture-DSP toggles (noise
    /// suppression and AGC). Same sharing pattern as `beep_params`:
    /// the runtime's mic loop reads them every frame, so flipping a
    /// Settings checkbox takes effect on the next 10 ms frame.
    dsp_params: DspParams,
    /// Live peak levels published by the cpal callbacks. Kept on
    /// `self` so a future Settings meter (e.g. a per-direction VU
    /// bar) can read them without re-plumbing the audio handle.
    /// The histogram itself uses the spectrum ring.
    #[allow(dead_code)]
    audio_levels: AudioLevels,
    /// Recent-sample rings (input + output) the cpal callbacks fill.
    /// `tick_waveform` snapshots the tail of one side each frame and
    /// runs an FFT for the histogram.
    audio_spectrum: AudioSpectrum,

    // ── UI-only state ───────────────────────────────────────────────
    /// True when the user is holding either the PTT key/button or
    /// click-and-holding the on-screen button. Used to detect `busy`
    /// (PTT pressed while another holder is active).
    ptt_held: bool,
    /// `Some(t)` while in TX — tracks the 30-second cap.
    tx_start: Option<Instant>,
    /// Settings sub-window open? Drives `show_viewport_immediate` in
    /// `update`; toggled by the gear icon and cleared when the user
    /// closes the OS window or hits ✕.
    show_settings: bool,
    /// Have we pushed our embedded fonts into the *settings viewport's*
    /// context yet? Each egui viewport carries its own `Context` (and
    /// therefore its own font atlas), so `register_fonts` has to be
    /// called once per viewport. Cleared when the window closes so a
    /// fresh open re-registers.
    settings_fonts_ready: bool,

    /// Connect dialog open? Triggered by the strip's "NEW CONNECTION"
    /// button when offline. Hosts URL + Username inputs in their own
    /// sub-window so the strip stays clean.
    show_connect: bool,
    /// Same font-priming trick as `settings_fonts_ready` but for the
    /// Connect viewport's context.
    connect_fonts_ready: bool,
    /// Buffered form values for the Connect dialog. We don't mutate
    /// `config` until the user confirms — that way "Cancel" leaves the
    /// saved Quick Connect target untouched.
    connect_form: ConnectForm,
    /// Mute toggle (output gate; separate from volume so unmuting
    /// restores the previous gain).
    muted: bool,
    /// Pre-mute output gain, so toggling mute round-trips cleanly.
    gain_before_mute: f32,
    /// Currently-selected channel index in the 446–448 MHz band. UI
    /// updates this instantly on chevron click; the actual server-
    /// side room join is debounced — see `freq_change_deadline`.
    channel_idx: usize,
    /// While `Some(t)`, the user is mid-tuning: they've clicked a
    /// chevron and we're holding the actual `ChangeFrequency` RPC
    /// until `t`. Each fresh chevron click pushes `t` forward by
    /// `FREQ_DEBOUNCE`. Cleared once the RPC fires (or on disconnect).
    freq_change_deadline: Option<Instant>,
    /// Smoothed bar magnitudes for the spectrum histogram, indexed
    /// low → high frequency. Updated each tick from an FFT of the
    /// active source (mic during TX, playback during RX).
    spectrum_bars: Vec<f32>,
    /// Pre-planned FFT over `SPECTRUM_FFT_LEN` samples. Reused every
    /// frame so we don't re-plan (rustfft caches twiddles internally
    /// in the planner, but re-asking each frame is still wasteful).
    fft: Arc<dyn Fft<f32>>,
    /// Scratch for the FFT — sampled audio in, complex magnitudes
    /// out. Held on `self` so we don't reallocate every frame.
    fft_workspace: Vec<Complex<f32>>,
    /// Scratch buffer the spectrum-ring snapshot drains into.
    spectrum_samples: Vec<f32>,
    /// Last frame time, for animation pacing independent of repaint rate.
    last_tick: Instant,
    /// Counter for the synthesized waveform's phase modulation.
    wave_phase: f32,
    /// App start time. Drives all UI animations (blinking dots,
    /// spinning refresh icon, RECONNECT button sweep) via
    /// `(Instant::now() - start_time).as_secs_f32()` modulated by
    /// each effect's period. Cheaper than maintaining N parallel phase
    /// accumulators and avoids float-drift over long sessions.
    start_time: Instant,
    /// Press/hold tracking for the four M1–M4 memory buttons, indexed
    /// 0..4. Latched so a single-frame flicker of egui's hit-test
    /// (see the PTT button's note) doesn't reset an in-progress hold.
    mem_press: [MemPress; 4],

    /// Shared state for the notify-only update checker (current phase +
    /// last-check time). Written by a background worker thread; read by
    /// the topbar pill and the Settings → UPDATES section.
    update_state: update::UpdateShared,
    /// A `Context` clone handed to the update worker so it can request a
    /// repaint when a check finishes (workers have no `Ui`/frame access).
    egui_ctx: egui::Context,
}

/// In-flight press gesture on a single memory button. A short left
/// release recalls/saves; a held left press (≥ [`MEM_HOLD`]) (re)assigns
/// the slot to the current frequency; a right click frees a saved slot
/// (released over it — dragging off aborts).
/// Each side latches on press-start and only resolves when its mouse
/// button goes up, so it survives the transient
/// `is_pointer_button_down_on()` flicker documented on the PTT handler.
#[derive(Default, Clone, Copy)]
struct MemPress {
    /// When the left button went down on this widget, if it's down.
    primary_since: Option<Instant>,
    /// Whether the long-press overwrite already fired this gesture, so
    /// the subsequent release doesn't *also* fire the short-click recall.
    primary_fired: bool,
    /// Whether the right button is latched on this widget. Unlike the
    /// left side this carries no timestamp — the right gesture has no
    /// hold, it just clears a *saved* slot on a clean release over it.
    secondary_latched: bool,
}

/// How long the left button must be held on a memory button before the
/// (re)assign gesture fires (a short left click recalls/saves instead).
/// Short enough to feel responsive, long enough that an ordinary click
/// never trips it. The right button frees on click, so it has no hold.
const MEM_HOLD: Duration = Duration::from_millis(450);

/// Which binding an in-progress key-capture session will write to.
/// The poller is target-agnostic (it just captures the next held
/// input); the app decides where the captured value lands.
#[derive(Clone, Copy, PartialEq, Eq)]
enum RecordTarget {
    /// The primary push-to-talk trigger.
    Ptt,
    /// The secondary/fallback push-to-talk trigger. PTT fires while
    /// either the primary or this is held.
    PttSecondary,
    /// Memory-recall hotkey for preset slot `0..4`.
    Memory(usize),
    /// Tune one channel up.
    FreqUp,
    /// Tune one channel down.
    FreqDown,
}

impl TokiApp {
    pub fn new(egui_ctx: egui::Context) -> Self {
        let state = state::shared();
        let config = config::Config::load();

        let audio_handle = audio::spawn(
            config.audio.input_device.clone(),
            config.audio.output_device.clone(),
            config.audio.input_gain,
            config.audio.output_gain,
            config.audio.balance,
        )
        .expect("audio init failed");
        let audio::AudioHandle {
            mic_rx,
            playback,
            devices,
            control,
            gains,
            levels,
            spectrum,
        } = audio_handle;

        // FFT planner is cheap to throw away once we have the
        // concrete plan — keep the `Arc<dyn Fft>` for repeated use.
        let mut planner = FftPlanner::<f32>::new();
        let fft = planner.plan_fft_forward(T::SPECTRUM_FFT_LEN);

        // Roger-beep atomics — seeded from the saved preset (an
        // unknown id falls back to index 0, the Default preset) and
        // shared live with the runtime task so a Settings change
        // takes effect immediately, no reconnect required.
        let beep_params = BeepParams::new(
            BeepPreset::index_of(&config.beeps.preset),
            config.beeps.volume,
        );
        // Capture-DSP atomics — seeded from the saved toggles, shared
        // live with the runtime's mic loop (same arrangement as the
        // beep params above).
        let dsp_params = DspParams::new(config.audio.noise_suppression, config.audio.agc);
        let cmd_tx = runtime::spawn(
            state.clone(),
            mic_rx,
            playback,
            beep_params.clone(),
            dsp_params.clone(),
        );

        let initial = config.hotkey.to_input().or_else(|| {
            tracing::warn!(
                "no parseable PTT input in config, using default ({:?})",
                hotkey::DEFAULT_KEY
            );
            Some(hotkey::Input::Key(hotkey::DEFAULT_KEY))
        });
        let installed = hotkey::install(
            cmd_tx.clone(),
            initial,
            config.hotkey.to_input_secondary(),
            config.hotkey.memory_inputs(),
            config.hotkey.freq_inputs(),
        );

        // Seed the channel index from the saved frequency. If the
        // string is bogus, fall back to the middle of the band.
        let channel_idx =
            T::channel_of_label(&config.connection.frequency).unwrap_or(T::FREQ_CHANNEL_COUNT / 2);
        // Normalize the saved frequency label in case it had drift,
        // so the wire string and the displayed value agree.
        let frequency = T::frequency_label(T::frequency_of(channel_idx));

        // Auto-connect on launch using the saved server/name. The user
        // expects "walkie-talkies stay on" — Toki should be live as
        // soon as the window opens, not require a Connect click first.
        let host = config.connection.host.trim().to_string();
        let display_name = config.connection.display_name.trim().to_string();
        if !host.is_empty() && !display_name.is_empty() {
            let _ = cmd_tx.send(Cmd::Connect {
                server: config.connection.endpoint(),
                display_name,
                frequency: frequency.clone(),
                password: config.connection.password.clone(),
            });
        }

        // Notify-only update checker. Fire a check on launch when the
        // user hasn't opted out; the worker runs off-thread and repaints
        // via the Context clone when it lands.
        let update_state = update::shared();
        if config.update.auto_check {
            update::spawn_check(update_state.clone(), egui_ctx.clone());
        }

        Self {
            state,
            cmd_tx,
            config,
            hotkey: installed,
            update_state,
            egui_ctx,
            recording: false,
            recording_target: RecordTarget::Ptt,
            audio_devices: devices,
            audio_control: control,
            audio_gains: gains,
            beep_params,
            dsp_params,
            audio_levels: levels,
            audio_spectrum: spectrum,
            ptt_held: false,
            tx_start: None,
            show_settings: false,
            settings_fonts_ready: false,
            show_connect: false,
            connect_fonts_ready: false,
            connect_form: ConnectForm::default(),
            muted: false,
            gain_before_mute: 1.0,
            channel_idx,
            freq_change_deadline: None,
            spectrum_bars: vec![0.0; T::SPECTRUM_BARS],
            fft,
            fft_workspace: vec![Complex::new(0.0, 0.0); T::SPECTRUM_FFT_LEN],
            spectrum_samples: Vec::with_capacity(T::SPECTRUM_FFT_LEN),
            last_tick: Instant::now(),
            wave_phase: 0.0,
            start_time: Instant::now(),
            mem_press: [MemPress::default(); 4],
        }
    }

    fn elapsed_secs(&self) -> f32 {
        self.start_time.elapsed().as_secs_f32()
    }

    fn snapshot(&self) -> StateSnapshot {
        let s = self.state.lock().unwrap();
        let self_id = s.self_id.clone();
        let holder = s.holder.clone();
        let is_transmitting = self_id.is_some() && holder.as_deref() == self_id.as_deref();
        let holder_name = if let Some(h) = &holder {
            s.members.get(h).cloned().unwrap_or_else(|| h.clone())
        } else {
            String::new()
        };
        StateSnapshot {
            connection: s.connection.clone(),
            holder,
            holder_name,
            is_transmitting,
            display_name: s.display_name.clone(),
            frequency: s.frequency.clone(),
            muted: s.locally_silenced(),
            conn_quality: s.conn_quality.as_ref().map(|h| h.snapshot()),
            log_tail: s.log.iter().next_back().cloned().unwrap_or_default(),
        }
    }

    fn radio_state(&self, snap: &StateSnapshot) -> RadioState {
        // Transport health wins over radio activity — if we're not on
        // the wire, we can't possibly be in tx/rx/etc.
        match &snap.connection {
            ConnState::Connecting => return RadioState::Reconnecting,
            ConnState::Disconnected | ConnState::Failed(_) => return RadioState::Offline,
            ConnState::Connected => {}
        }
        if snap.is_transmitting {
            RadioState::Tx
        } else if snap.holder.is_some() {
            // Someone else holds — they're transmitting.
            if self.ptt_held {
                RadioState::Busy // we tried to barge in
            } else {
                RadioState::Rx
            }
        } else {
            RadioState::Idle
        }
    }

    /// Compute the spectrum histogram for the current frame.
    ///
    /// Pulls the most recent `SPECTRUM_FFT_LEN` samples from the
    /// audio thread (mic during TX, playback during RX, nothing
    /// otherwise), windows them with a Hann window, runs a forward
    /// FFT, and reduces the useful bins (1 .. N/2 — DC + Nyquist
    /// dropped) into `SPECTRUM_BARS` log-magnitude bars.
    ///
    /// The bar values are smoothed across frames so a noisy FFT
    /// doesn't make the histogram twitch; same fast-attack /
    /// slow-decay shape as the audio peak meter.
    fn tick_waveform(&mut self, st: RadioState) {
        let now = Instant::now();
        let dt = now.duration_since(self.last_tick).as_secs_f32().min(0.1);
        self.last_tick = now;
        self.wave_phase += dt * 6.0;

        match st {
            RadioState::Tx => self
                .audio_spectrum
                .snapshot_input(&mut self.spectrum_samples, T::SPECTRUM_FFT_LEN),
            RadioState::Rx => self
                .audio_spectrum
                .snapshot_output(&mut self.spectrum_samples, T::SPECTRUM_FFT_LEN),
            _ => self.spectrum_samples.clear(),
        };

        // Need a full window's worth of samples to produce a useful
        // FFT — otherwise just decay the existing bars toward zero.
        if self.spectrum_samples.len() == T::SPECTRUM_FFT_LEN {
            // Hann window + load into the complex workspace.
            let n = T::SPECTRUM_FFT_LEN;
            for (i, &s) in self.spectrum_samples.iter().enumerate() {
                let w = 0.5 - 0.5 * (i as f32 * std::f32::consts::TAU / (n as f32 - 1.0)).cos();
                self.fft_workspace[i] = Complex::new(s * w, 0.0);
            }
            self.fft.process(&mut self.fft_workspace);

            // We care about bins `[1, n/2)` — skip DC (bin 0) and
            // mirror frequencies above Nyquist. Group sequential bins
            // into `SPECTRUM_BARS` buckets (so the histogram is
            // independent of the FFT size).
            let usable_bins = n / 2 - 1; // skip DC
            let per_bar = (usable_bins / T::SPECTRUM_BARS).max(1);
            // Theoretical normalization is `1 / (n * 0.5)` (Hann
            // coherent gain), which makes a full-scale sine sit at
            // 1.0. In practice voice never hits anywhere near that:
            // typical PCM peaks at 0.05–0.15 and the energy spreads
            // across many bins, so each bar's raw magnitude is in
            // the 0.001–0.02 range. Multiply the visualizer norm by
            // ~8× so a moderately-loud voice now spans the panel
            // top to bottom rather than wiggling at the baseline.
            // This is purely a *display* gain — the underlying audio
            // is untouched, so we can't actually clip anything by
            // pushing this up.
            let norm = 8.0 / (n as f32 * 0.5);
            for bar_i in 0..T::SPECTRUM_BARS {
                let bin_lo = 1 + bar_i * per_bar;
                let bin_hi = (bin_lo + per_bar).min(n / 2);
                let mut sum: f32 = 0.0;
                for c in &self.fft_workspace[bin_lo..bin_hi] {
                    sum += (c.re * c.re + c.im * c.im).sqrt();
                }
                let mag = (sum / (bin_hi - bin_lo) as f32) * norm;
                // Slightly more aggressive gamma (0.5 → 0.42) lifts
                // the tail of the distribution further — small
                // consonants get visible.
                let target = mag.clamp(0.0, 1.0).powf(0.42);
                let prev = self.spectrum_bars[bar_i];
                // Faster decay (0.75 → 0.62) so bars fall more
                // dynamically between syllables — feels lively
                // instead of mushy.
                self.spectrum_bars[bar_i] = if target >= prev {
                    target
                } else {
                    prev * 0.62 + target * 0.38
                };
            }
        } else {
            // No live audio — bleed the bars toward zero so the
            // panel doesn't freeze on the last RX frame after a
            // channel change.
            for bar in &mut self.spectrum_bars {
                *bar *= 0.85;
            }
        }
    }

    fn ensure_min_one_frame(&self, ctx: &egui::Context) {
        // 33 ms ≈ 30 fps — fast enough that waveform scrolls smoothly
        // and the TX countdown ticks visibly.
        ctx.request_repaint_after(Duration::from_millis(33));
    }

    /// Periodic auto-update re-check. With auto-check on, fire a fresh
    /// check once the last completed one is older than the re-check
    /// interval (or once, if auto-check was enabled mid-session and no
    /// check has run yet). Just a timestamp compare each frame;
    /// `spawn_check` itself no-ops while a check is already in flight.
    fn tick_update_check(&mut self) {
        if !self.config.update.auto_check {
            return;
        }
        let due = {
            let st = self.update_state.lock().unwrap();
            match st.last_checked {
                Some(t) => t.elapsed() >= update::RECHECK_INTERVAL,
                // No check yet: only fire if we're truly idle (the
                // startup check, if any, already set phase to Checking).
                None => matches!(st.phase, update::UpdatePhase::Idle),
            }
        };
        if due {
            update::spawn_check(self.update_state.clone(), self.egui_ctx.clone());
        }
    }

    /// Count of members on the current frequency (us + others), read
    /// directly from the shared state. Used for the activity light.
    fn snapshot_members_count(&self) -> usize {
        self.state.lock().unwrap().members.len()
    }

    /// Called on every chevron click. Two-phase behavior:
    ///   1. On the *first* click of a tuning burst, immediately tell
    ///      the runtime to leave the current room — we go "off the
    ///      air" so further clicks scan through frequencies without
    ///      racing join/leave RPCs against each other.
    ///   2. Always push the debounce deadline forward to
    ///      `now + FREQ_DEBOUNCE`. The actual `ChangeFrequency` RPC
    ///      fires from the update loop once the deadline passes
    ///      without any further clicks.
    ///
    /// The local `channel_idx` and `config` are updated immediately so
    /// the OLED reflects the user's intent as they scroll, even
    /// though the network move is deferred.
    fn schedule_frequency_change(&mut self) {
        let freq = T::frequency_label(T::frequency_of(self.channel_idx));
        self.config.connection.frequency = freq;
        self.config.save();

        // Only act on debounce when we're actually connected — if
        // we're disconnected, the next Connect will use the right
        // frequency from config and no leave/join is needed now.
        let connected = matches!(self.state.lock().unwrap().connection, ConnState::Connected);
        if !connected {
            return;
        }

        // First click of this burst: send LeaveRoom right away so the
        // user disappears from the old room's roster immediately.
        if self.freq_change_deadline.is_none() {
            let _ = self.cmd_tx.send(Cmd::LeaveRoom);
        }
        self.freq_change_deadline = Some(Instant::now() + T::FREQ_DEBOUNCE);
    }

    /// Snap the local tuner widget to whatever frequency the *server*
    /// last told us we're on. Normally that already matches the
    /// tuner's `channel_idx` and this is a no-op. The case it
    /// actually handles is an admin-driven move via the admin panel:
    /// the server sends us a `FrequencyChanged` event we didn't ask
    /// for, the runtime writes `state.frequency`, and on the next
    /// frame this method nudges `channel_idx` over so the OLED and
    /// channel-strip render the new value.
    ///
    /// Guard against the user-initiated case: if a debounce is in
    /// flight, the user is still tuning and `state.frequency` lags
    /// `channel_idx` until the RPC round-trips — clobbering the
    /// local value here would yank the OLED back as they scroll.
    fn sync_tuner_to_server(&mut self, snap: &StateSnapshot) {
        if self.freq_change_deadline.is_some() {
            return;
        }
        let Some(server_freq) = snap.frequency.as_deref() else {
            return;
        };
        let Some(server_idx) = T::channel_of_label(server_freq) else {
            // Unknown frequency string — refuse to clobber the
            // local tuner over a malformed server payload.
            return;
        };
        if server_idx == self.channel_idx {
            return;
        }
        // Persist the new frequency to the saved config too, so a
        // user who's moved by an admin and then reconnects lands on
        // the room they were actually on, not the one they typed in
        // the connect dialog days ago.
        self.channel_idx = server_idx;
        self.config.connection.frequency = T::frequency_label(T::frequency_of(self.channel_idx));
        self.config.save();
    }

    /// Run from the update loop each frame: if a debounce is in
    /// flight and the deadline has passed, fire the ChangeFrequency
    /// RPC for the user's final channel selection and clear the
    /// pending state.
    fn tick_freq_debounce(&mut self) {
        let Some(deadline) = self.freq_change_deadline else {
            return;
        };
        // Bail out cleanly if we lost the session mid-tune.
        if !matches!(self.state.lock().unwrap().connection, ConnState::Connected) {
            self.freq_change_deadline = None;
            return;
        }
        if Instant::now() >= deadline {
            let freq = T::frequency_label(T::frequency_of(self.channel_idx));
            let _ = self.cmd_tx.send(Cmd::ChangeFrequency(freq));
            self.freq_change_deadline = None;
        }
    }

    /// True while a frequency change is debouncing (user is tuning,
    /// not yet settled). Drives the topbar's orange "TUNING" chip.
    fn is_tuning(&self) -> bool {
        self.freq_change_deadline.is_some()
    }
}

#[derive(Default)]
struct StateSnapshot {
    connection: ConnState,
    holder: Option<String>,
    holder_name: String,
    is_transmitting: bool,
    /// Our own live callsign. Mirrors `ClientState.display_name` and
    /// changes mid-session when the admin renames us — read by the
    /// topbar so the change is visible without a reconnect.
    display_name: String,
    /// Our current frequency room as the *server* sees it. The GUI's
    /// tuner widget reads `App.channel_idx` for its primary display,
    /// but compares against this each frame so admin-driven moves
    /// (server→client `FrequencyChanged`) snap the tuner over to
    /// match without round-tripping through user input.
    frequency: Option<String>,
    /// `true` when an operator has barred us from transmitting on the
    /// current channel — either a personal member-mute or a mute on the
    /// channel we're tuned to. Drives the PTT button's disabled "UNABLE
    /// TO TALK" treatment. Moving to an unmuted channel clears it.
    muted: bool,
    /// Live connection-quality readout (RTT / jitter / loss + a 0–4 bars
    /// score), or `None` when disconnected or not yet measured. Drives
    /// the topbar signal-bars glyph.
    conn_quality: Option<crate::telemetry::ConnQuality>,
    #[allow(dead_code)]
    log_tail: String,
}

// ════════════════════════════════════════════════════════════════════════
// Painting helpers
// ════════════════════════════════════════════════════════════════════════

fn font_mono(size: f32) -> FontId {
    FontId::new(size, FontFamily::Monospace)
}

/// Best-effort truncation to fit inside `max_w` pixels at the given
/// font. We strip from the end and append "…" — the offline panel's
/// subtitle uses this so a long error string doesn't bleed past the
/// panel edge.
fn truncate_to_width(painter: &egui::Painter, s: &str, font: FontId, max_w: f32) -> String {
    let galley = painter.layout_no_wrap(s.to_string(), font.clone(), Color32::WHITE);
    if galley.size().x <= max_w {
        return s.to_string();
    }
    let mut out = String::from(s);
    while !out.is_empty() {
        out.pop();
        let with_ell = format!("{out}…");
        let g = painter.layout_no_wrap(with_ell.clone(), font.clone(), Color32::WHITE);
        if g.size().x <= max_w {
            return with_ell;
        }
    }
    "…".into()
}

/// Draw `text` left-aligned inside `region`, scrolling horizontally
/// (marquee) when it's wider than the region; otherwise static. The
/// text is clipped to `region` so it never bleeds past the edges, and
/// when scrolling a second copy trails the first by a fixed gap for a
/// seamless loop. `elapsed` is the app's running wall-clock seconds so
/// the scroll advances each frame (the GUI already repaints
/// continuously for its other animations).
fn paint_marquee(
    painter: &egui::Painter,
    region: Rect,
    text: &str,
    font: FontId,
    color: Color32,
    elapsed: f32,
) {
    if text.is_empty() || region.width() <= 1.0 {
        return;
    }
    let galley = painter.layout_no_wrap(text.to_string(), font.clone(), color);
    let text_w = galley.size().x;
    let clip = painter.with_clip_rect(region);
    let y = region.center().y;
    if text_w <= region.width() {
        clip.text(
            Pos2::new(region.left(), y),
            Align2::LEFT_CENTER,
            text,
            font,
            color,
        );
        return;
    }
    // Scroll: loop period is the text width plus a gap so the trailing
    // copy gives a continuous ticker rather than a hard jump.
    let gap = 28.0_f32;
    let period = text_w + gap;
    let speed = 26.0_f32; // px/sec
    let offset = (elapsed * speed).rem_euclid(period);
    let x0 = region.left() - offset;
    clip.text(
        Pos2::new(x0, y),
        Align2::LEFT_CENTER,
        text,
        font.clone(),
        color,
    );
    clip.text(
        Pos2::new(x0 + period, y),
        Align2::LEFT_CENTER,
        text,
        font,
        color,
    );
}

/// Map the runtime's `ConnState` to a short user-facing reason line.
/// Matches the offline-reason vocabulary in `design/behavior-spec.md`
/// — short enough to fit the 12-char column the spec calls out.
fn offline_reason(snap: &StateSnapshot, is_offline: bool) -> String {
    if !is_offline {
        // Reconnecting — show what we're contacting, not a reason.
        return "Resolving server…".into();
    }
    match &snap.connection {
        ConnState::Disconnected => "DISCONNECTED".into(),
        ConnState::Failed(e) => {
            // Pluck a short, all-caps phrase out of the underlying
            // error if we can recognize it — falls back to the raw
            // message otherwise.
            let lower = e.to_ascii_lowercase();
            if lower.contains("already in use") || lower.contains("alreadyexists") {
                "CALLSIGN TAKEN".into()
            } else if lower.contains("auth") {
                "AUTH FAILED".into()
            } else if lower.contains("refused")
                || lower.contains("unreachable")
                || lower.contains("connect")
            {
                "SERVER UNREACHABLE".into()
            } else if lower.contains("timeout") {
                "CONNECTION LOST".into()
            } else {
                e.clone()
            }
        }
        // Shouldn't be observed in offline branch but harmless.
        ConnState::Connecting | ConnState::Connected => "OFFLINE".into(),
    }
}

/// Cleaner "wifi-no-signal" icon — three smooth signal arcs with a
/// bold diagonal bar that visually punches through them via a
/// background-coloured shadow stroke painted underneath the bar. The
/// `bg` argument should match the surface the icon sits on (typically
/// `T::OLED`); without that step the bar would just overlay the arcs
/// at the intersections and the "no signal" reading would be muddier.
///
/// All five strokes (3 arcs + bar shadow + bar) use plain
/// `line_segment`s — egui's Painter is happiest with that, and SVG
/// rasterization isn't wired into the chassis yet.
fn paint_wifi_barred_icon(
    painter: &egui::Painter,
    center: Pos2,
    size: f32,
    color: Color32,
    bg: Color32,
) {
    let stroke = Stroke::new(1.8, color);
    // Anchor the arcs so the "fan" emanates from a point just below
    // the icon's vertical centre. `base_y` is the dot we eventually
    // paint as the wifi's foot.
    let base_y = center.y + size * 0.42;
    let arc_span = std::f32::consts::PI * 0.70; // 126°
    let arc_start = (std::f32::consts::PI - arc_span) / 2.0 + std::f32::consts::PI;
    // Three arcs at decreasing radii — the classic wifi fan.
    for radius_scale in [1.05_f32, 0.70, 0.36] {
        let r = size * radius_scale;
        let segments = 28;
        let mut prev = None;
        for i in 0..=segments {
            let t = i as f32 / segments as f32;
            let theta = arc_start + arc_span * t;
            let p = Pos2::new(center.x + theta.cos() * r, base_y + theta.sin() * r * 0.62);
            if let Some(p0) = prev {
                painter.line_segment([p0, p], stroke);
            }
            prev = Some(p);
        }
    }
    // Foot dot.
    painter.circle_filled(Pos2::new(center.x, base_y), 1.6, color);

    // Diagonal bar: paint a wider background-coloured stroke first so
    // the arcs visually disappear behind the bar (punch-through).
    let s = size * 1.05;
    let p1 = Pos2::new(center.x - s, center.y - s * 0.95);
    let p2 = Pos2::new(center.x + s, center.y + s * 0.95);
    painter.line_segment([p1, p2], Stroke::new(4.2, bg));
    painter.line_segment([p1, p2], Stroke::new(2.0, color));
}

/// Two-prong wall plug at `center`, scaled to `size`. Body is a small
/// rounded rectangle with two prongs poking upward and a short cable
/// curving out the bottom-right corner — reads as "plug it back in",
/// which matches the Quick Connect button's "reuse last config"
/// semantics.
fn paint_plug_icon(painter: &egui::Painter, center: Pos2, size: f32, color: Color32) {
    let stroke = Stroke::new(1.8, color);

    // Body: ~1.5× wider than tall, slightly below `center` so the
    // prongs above balance the bounding box visually.
    let body_w = size * 1.55;
    let body_h = size * 1.0;
    let body = Rect::from_center_size(
        Pos2::new(center.x, center.y + size * 0.12),
        Vec2::new(body_w, body_h),
    );
    painter.rect_stroke(body, CornerRadius::same(2), stroke, StrokeKind::Inside);

    // Prongs: two thin filled rects rising from the body's top edge.
    let prong_h = size * 0.55;
    let prong_w = 1.8;
    let prong_y_top = body.top() - prong_h;
    let prong_dx = size * 0.38;
    for dx in [-prong_dx, prong_dx] {
        let prong = Rect::from_min_max(
            Pos2::new(center.x + dx - prong_w / 2.0, prong_y_top),
            Pos2::new(center.x + dx + prong_w / 2.0, body.top()),
        );
        painter.rect_filled(prong, CornerRadius::same(1), color);
    }

    // Cable: two short segments approximating a soft curve out the
    // bottom-right of the body. Avoids depending on egui's QuadraticBezier
    // helper, which would otherwise be the right tool here.
    let cable_a = Pos2::new(center.x + body_w * 0.10, body.bottom());
    let cable_b = Pos2::new(center.x + body_w * 0.35, body.bottom() + size * 0.32);
    let cable_c = Pos2::new(center.x + body_w * 0.55, body.bottom() + size * 0.50);
    painter.line_segment([cable_a, cable_b], stroke);
    painter.line_segment([cable_b, cable_c], stroke);
}

/// 22×22-ish "wifi-off" glyph at `center`, scaled to `size`. Three
/// arcs (signal lobes) crossed by a diagonal slash, all stroked in
/// `color`. Hand-drawn primitives rather than SVG because we don't
/// have an icon-rasterization pipeline wired into the chassis yet.
#[allow(dead_code)]
fn paint_wifi_off_icon(painter: &egui::Painter, center: Pos2, size: f32, color: Color32) {
    let stroke = Stroke::new(1.8, color);
    // Three concentric arcs (the wifi "fan") above the center dot.
    let base_y = center.y + size * 0.40;
    for (i, scale) in [(2.0_f32, 0.95_f32), (1.4, 0.65), (0.8, 0.35)]
        .iter()
        .enumerate()
    {
        let r = size * scale.1;
        let pts = 14;
        let mut prev = None;
        for k in 0..=pts {
            let t = k as f32 / pts as f32;
            let theta = std::f32::consts::PI + t * std::f32::consts::PI; // bottom half
            let p = Pos2::new(center.x + theta.cos() * r, base_y + theta.sin() * r * 0.55);
            if let Some(p0) = prev {
                painter.line_segment([p0, p], stroke);
            }
            prev = Some(p);
        }
        // Slight fade for inner arcs so the icon reads.
        let _ = i;
    }
    // The center "dot" (foot of the wifi).
    painter.circle_filled(Pos2::new(center.x, base_y), 1.8, color);
    // Diagonal slash across the whole icon.
    let s = size * 1.15;
    let p1 = Pos2::new(center.x - s, center.y - s);
    let p2 = Pos2::new(center.x + s, center.y + s);
    painter.line_segment([p1, p2], Stroke::new(2.0, color));
}

/// 22×22-ish "refresh" arrow at `center`, rotated to `angle_rad`.
/// Two arc segments with little arrow heads at the open ends; used
/// as a poor-man's spinner during the reconnect handshake.
fn paint_refresh_icon(
    painter: &egui::Painter,
    center: Pos2,
    radius: f32,
    angle_rad: f32,
    color: Color32,
) {
    let stroke = Stroke::new(2.0, color);
    let segments = 32;
    // Open arc covers ~260°. Leave a 50° gap split between the two ends.
    let gap = 0.4; // rad
    let total = std::f32::consts::TAU - gap;
    let start = angle_rad - total / 2.0;
    let mut prev: Option<Pos2> = None;
    for i in 0..=segments {
        let t = i as f32 / segments as f32;
        let theta = start + total * t;
        let p = Pos2::new(
            center.x + theta.cos() * radius,
            center.y + theta.sin() * radius,
        );
        if let Some(p0) = prev {
            painter.line_segment([p0, p], stroke);
        }
        prev = Some(p);
    }
    // Small arrow head at the end of the arc to suggest direction.
    let theta_end = start + total;
    let tip = Pos2::new(
        center.x + theta_end.cos() * radius,
        center.y + theta_end.sin() * radius,
    );
    let h = radius * 0.45;
    let tangent = theta_end + std::f32::consts::FRAC_PI_2;
    let back = Pos2::new(
        tip.x - tangent.cos() * h * 0.7 + theta_end.cos() * h * 0.5,
        tip.y - tangent.sin() * h * 0.7 + theta_end.sin() * h * 0.5,
    );
    let back2 = Pos2::new(
        tip.x - tangent.cos() * h * 0.7 - theta_end.cos() * h * 0.5,
        tip.y - tangent.sin() * h * 0.7 - theta_end.sin() * h * 0.5,
    );
    painter.line_segment([tip, back], stroke);
    painter.line_segment([tip, back2], stroke);
}

/// Section header inside the settings window: a small upper-case label
/// in the phosphor primary colour, followed by a thin divider. Used to
/// group rows into "CUSTOMIZATION" and "AUDIO" buckets.
fn section_header(ui: &mut egui::Ui, label: &str) {
    ui.label(
        egui::RichText::new(label)
            .color(T::PRIMARY)
            .monospace()
            .size(10.0),
    );
    ui.add_space(2.0);
    let y = ui.cursor().top();
    ui.painter().line_segment(
        [
            Pos2::new(ui.min_rect().left(), y),
            Pos2::new(ui.min_rect().right(), y),
        ],
        Stroke::new(1.0, T::PRIMARY_INK),
    );
    ui.add_space(8.0);
}

/// A horizontal VU bar for the Settings AUDIO section. `level` is the
/// smoothed peak in `[0.0, 1.0]` straight from [`AudioLevels`]. We draw
/// a dim track and fill it left-to-right proportional to the level; the
/// fill turns amber as it nears full-scale (>0.9) so "my mic is too hot
/// / clipping" reads at a glance without a number. Sits inside a
/// [`settings_row`] like any other control.
fn paint_level_meter(ui: &mut egui::Ui, level: f32) {
    let level = level.clamp(0.0, 1.0);
    // Fixed footprint so the INPUT/OUTPUT meter rows line up under the
    // device pickers above them.
    let (rect, _resp) = ui.allocate_exact_size(Vec2::new(200.0, 10.0), egui::Sense::hover());
    let painter = ui.painter();
    // Track behind the fill.
    painter.rect_filled(rect, 2.0, T::PRIMARY_INK);
    if level > 0.0 {
        let mut fill = rect;
        fill.set_width(rect.width() * level);
        // Amber once we're within ~1 dB of full-scale — the "back off"
        // cue. Phosphor green otherwise.
        let color = if level > 0.9 { T::WARN } else { T::PRIMARY };
        painter.rect_filled(fill, 2.0, color);
    }
}

/// One row in the settings window: fixed-width label + arbitrary
/// control on the right. Free function (not a method) so the closure
/// can also borrow `self` mutably without colliding on `&mut self`.
fn settings_row(ui: &mut egui::Ui, label: &str, content: impl FnOnce(&mut egui::Ui)) {
    ui.horizontal(|ui| {
        ui.add_sized(
            Vec2::new(110.0, 18.0),
            egui::Label::new(
                egui::RichText::new(label)
                    .color(T::INK_DIM)
                    .monospace()
                    .size(9.0),
            ),
        );
        content(ui);
    });
    ui.add_space(6.0);
    let y = ui.cursor().top();
    ui.painter().line_segment(
        [
            Pos2::new(ui.min_rect().left(), y),
            Pos2::new(ui.min_rect().right(), y),
        ],
        Stroke::new(
            1.0,
            Color32::from_rgba_premultiplied(0x06, 0x06, 0x06, 0x0a),
        ),
    );
    ui.add_space(2.0);
}

fn font_ui(size: f32, weight_600: bool) -> FontId {
    // `weight_600=true` routes through the bold face we register in
    // `register_fonts` under its own named family. If the bold file
    // ever goes missing the family lookup falls back to Proportional
    // (egui doesn't synthesize weight; the worst case is "looks like
    // the regular weight" rather than a panic).
    if weight_600 {
        FontId::new(size, FontFamily::Name(UI_BOLD_FAMILY.into()))
    } else {
        FontId::new(size, FontFamily::Proportional)
    }
}

/// Name of the bold UI family registered in [`register_fonts`].
const UI_BOLD_FAMILY: &str = "toki-ui-bold";

/// Register the three embedded TTFs as egui font families. Called
/// exactly once during app startup (before `TokiApp::new`) so that
/// `font_ui` / `font_mono` see the custom faces from the very first
/// frame.
///
/// Three faces ship in `assets/ui/`:
/// * `ui.ttf` — regular weight, becomes `FontFamily::Proportional` slot 0
/// * `ui-bold.ttf` — bold weight, registered as a named family
///   (`UI_BOLD_FAMILY`) so `font_ui(_, true)` can ask for it explicitly;
///   egui doesn't synthesize weight
/// * `mono.ttf` — monospace, becomes `FontFamily::Monospace` slot 0
///
/// Egui keeps its built-in fonts as fallbacks under each family, so
/// any glyph the custom faces don't cover (emoji, niche symbols) still
/// renders — we just push our faces to the front of the list.
pub fn register_fonts(ctx: &egui::Context) {
    use eframe::egui::{FontData, FontDefinitions};

    let mut fonts = FontDefinitions::default();

    fonts.font_data.insert(
        "toki-ui".into(),
        FontData::from_static(include_bytes!("../assets/ui/ui.ttf")).into(),
    );
    fonts.font_data.insert(
        "toki-ui-bold".into(),
        FontData::from_static(include_bytes!("../assets/ui/ui-bold.ttf")).into(),
    );
    fonts.font_data.insert(
        "toki-mono".into(),
        FontData::from_static(include_bytes!("../assets/ui/mono.ttf")).into(),
    );

    // Push custom faces to the front of each family's fallback chain.
    fonts
        .families
        .entry(FontFamily::Proportional)
        .or_default()
        .insert(0, "toki-ui".into());
    fonts
        .families
        .entry(FontFamily::Monospace)
        .or_default()
        .insert(0, "toki-mono".into());

    // Bold lives in its own named family so callers can opt into it
    // explicitly. We seed the fallback chain with the regular UI face
    // and the default proportional fonts so missing glyphs still
    // render even when bold is requested.
    let bold_chain = vec!["toki-ui-bold".into(), "toki-ui".into()];
    fonts
        .families
        .insert(FontFamily::Name(UI_BOLD_FAMILY.into()), bold_chain);

    ctx.set_fonts(fonts);
}

/// Paint a vertical gradient inside `rect` using `egui_colorgradient`
/// to interpolate between the given stops. The mesh underneath is a
/// plain rectangle; we paint a `corners`-rounded base first with the
/// stops' midpoint so the rounded-corner pixels read as gradient-mid
/// rather than the rectangular-fill end colors at the corners.
///
/// `stops` are `(t, color)` pairs with `t ∈ [0,1]`; for a two-stop
/// gradient pass `&[(0.0, top), (1.0, bottom)]`.
fn paint_vertical_gradient(
    painter: &egui::Painter,
    rect: Rect,
    corners: CornerRadius,
    stops: &[(f32, Color32)],
) {
    use eframe::egui::ecolor::Hsva;
    use eframe::egui::epaint::{Mesh, Vertex, WHITE_UV};
    use egui_colorgradient::{Gradient, InterpolationMethod};

    if stops.is_empty() {
        return;
    }

    // Build the gradient from the stops. The crate stores them as Hsva
    // internally — convert via egui's `Color32 → Hsva` blanket.
    let stop_iter: Vec<(f32, Hsva)> = stops.iter().map(|(t, c)| (*t, Hsva::from(*c))).collect();
    let gradient = Gradient::new(InterpolationMethod::Linear, stop_iter);

    // Pre-fill the rounded shape with a midpoint color so the four
    // corner arcs render in something close to the gradient's color
    // at their vertical position. Without this base, the strip mesh
    // would render with sharp rectangular corners showing the strip's
    // top/bottom end colors — visible as a "filled-in" look when the
    // rounded outline is drawn on top.
    let mid = gradient.linear_eval(3, true)[1];
    painter.rect_filled(rect, corners, mid);

    // 32 strips is enough for visually smooth gradients at typical
    // widget heights (<200 px) without throwing thousands of triangles
    // at the GPU each frame.
    const N: usize = 32;
    let colors = gradient.linear_eval(N + 1, true);

    let mut mesh = Mesh::default();
    for (i, &color) in colors.iter().enumerate().take(N + 1) {
        let t = i as f32 / N as f32;
        let y = rect.top() + t * rect.height();
        mesh.vertices.push(Vertex {
            pos: Pos2::new(rect.left(), y),
            uv: WHITE_UV,
            color,
        });
        mesh.vertices.push(Vertex {
            pos: Pos2::new(rect.right(), y),
            uv: WHITE_UV,
            color,
        });
    }
    for i in 0..N {
        let v = (i * 2) as u32;
        mesh.indices
            .extend_from_slice(&[v, v + 1, v + 2, v + 1, v + 3, v + 2]);
    }
    painter.add(Shape::mesh(mesh));
}

/// Paint a rounded panel with a fill, an inset border, and (optionally)
/// a 1 px top highlight. The OLED panels and the chassis both use this.
fn paint_panel(
    painter: &egui::Painter,
    rect: Rect,
    fill: Color32,
    border: Color32,
    radius: f32,
    top_highlight: Option<Color32>,
) {
    painter.rect(
        rect,
        CornerRadius::same(radius as u8),
        fill,
        Stroke::new(1.0, border),
        StrokeKind::Inside,
    );
    if let Some(hl) = top_highlight {
        let y = rect.top() + 0.5;
        painter.line_segment(
            [
                Pos2::new(rect.left() + radius, y),
                Pos2::new(rect.right() - radius, y),
            ],
            Stroke::new(1.0, hl),
        );
    }
}

/// Overlay the scanline pattern on an OLED panel. 1 px highlight every
/// 3 px at the design's 1.8% white. Subtle, intentional.
fn paint_scanlines(painter: &egui::Painter, rect: Rect, radius: f32) {
    // Clip to the panel's rounded shape by walking lines only within
    // the rect — the slight bleed at the corners is unnoticeable at the
    // 1.8% alpha we're using.
    let color = Color32::from_rgba_premultiplied(0x05, 0x05, 0x05, 0x05);
    let _ = radius;
    let mut y = rect.top() + 2.0;
    while y < rect.bottom() {
        painter.line_segment(
            [
                Pos2::new(rect.left() + 2.0, y),
                Pos2::new(rect.right() - 2.0, y),
            ],
            Stroke::new(1.0, color),
        );
        y += 3.0;
    }
}

/// Approximate text glow by painting the same text three times at the
/// same position with the glow color at increasing alpha "halos". egui
/// can't blur text cheaply, so this is a softer-than-CSS approximation,
/// but it reads as a glow on dark backgrounds.
#[allow(clippy::too_many_arguments)]
fn glow_text(
    painter: &egui::Painter,
    pos: Pos2,
    anchor: Align2,
    text: &str,
    font: FontId,
    color: Color32,
    glow: Color32,
    intensity: f32,
) {
    // Draw the halo by offsetting in 4 cardinal directions, scaled by
    // the intensity. Stack 2 passes for additive softness.
    let layers: &[(f32, u8)] = &[(3.0 * intensity, 80), (1.5 * intensity, 140)];
    for (offset, alpha) in layers {
        let mut c = glow;
        c = Color32::from_rgba_unmultiplied(c.r(), c.g(), c.b(), *alpha);
        for dx in [-offset, 0.0, *offset] {
            for dy in [-offset, 0.0, *offset] {
                if dx == 0.0 && dy == 0.0 {
                    continue;
                }
                painter.text(
                    Pos2::new(pos.x + dx, pos.y + dy),
                    anchor,
                    text,
                    font.clone(),
                    c,
                );
            }
        }
    }
    painter.text(pos, anchor, text, font, color);
}

/// Small filled circle with a glow halo behind it. Used for status dots.
fn glow_dot(painter: &egui::Painter, center: Pos2, radius: f32, color: Color32, intensity: f32) {
    if intensity > 0.0 {
        let glow_color = Color32::from_rgba_unmultiplied(color.r(), color.g(), color.b(), 90);
        painter.circle_filled(center, radius + 3.0 * intensity, glow_color);
        let stronger = Color32::from_rgba_unmultiplied(color.r(), color.g(), color.b(), 160);
        painter.circle_filled(center, radius + 1.5 * intensity, stronger);
    }
    painter.circle_filled(center, radius, color);
}

// ════════════════════════════════════════════════════════════════════════
// Main update / layout
// ════════════════════════════════════════════════════════════════════════

impl eframe::App for TokiApp {
    fn update(&mut self, ctx: &egui::Context, _frame: &mut eframe::Frame) {
        self.ensure_min_one_frame(ctx);
        self.tick_update_check();

        let snap = self.snapshot();
        let st = self.radio_state(&snap);
        self.tick_waveform(st);
        // Fire any pending frequency-change RPC once the user has
        // stopped clicking chevrons for `FREQ_DEBOUNCE`.
        self.tick_freq_debounce();
        // Sync the tuner widget's local `channel_idx` to whatever the
        // server says we're on. Normally these match (the user moved
        // the tuner, we sent ChangeFrequency, the server echoed back).
        // But an admin-driven move sends a server-initiated
        // `FrequencyChanged` event the user didn't request — the
        // tuner needs to snap over to the new value. Single-direction
        // sync (server → tuner) so we don't loop the user's own
        // pending moves back into the local state mid-debounce.
        self.sync_tuner_to_server(&snap);

        // ── Recording: poll the device_query listener for a capture ──
        if self.recording {
            if let Some(input) = self.hotkey.take_recorded() {
                match self.recording_target {
                    RecordTarget::Ptt => {
                        if let Err(e) = self.hotkey.rebind(input) {
                            tracing::warn!(error = %e, "PTT rebind failed");
                        } else {
                            self.config.hotkey.set_ptt(input);
                            self.config.save();
                        }
                    }
                    RecordTarget::PttSecondary => {
                        if let Err(e) = self.hotkey.rebind_secondary(Some(input)) {
                            tracing::warn!(error = %e, "secondary PTT rebind failed");
                        } else {
                            self.config.hotkey.set_ptt_secondary(Some(input));
                            self.config.save();
                        }
                    }
                    RecordTarget::Memory(i) => {
                        self.hotkey.rebind_memory(i, Some(input));
                        self.config
                            .hotkey
                            .set_memory(i, HotkeyBinding::from_input(input));
                        self.config.save();
                    }
                    RecordTarget::FreqUp => {
                        self.hotkey.rebind_freq(true, Some(input));
                        self.config.hotkey.freq_up = HotkeyBinding::from_input(input);
                        self.config.save();
                    }
                    RecordTarget::FreqDown => {
                        self.hotkey.rebind_freq(false, Some(input));
                        self.config.hotkey.freq_down = HotkeyBinding::from_input(input);
                        self.config.save();
                    }
                }
                self.recording = false;
            }
        }

        // ── Action hotkeys: apply edges the poller queued ───────────
        // Gated like the chevrons / on-screen M-buttons: only act when
        // connected and not mid-transmission. We still drain the queues
        // when gated so events don't pile up.
        let recalls = self.hotkey.take_recalls();
        let freq_delta = self.hotkey.take_freq_delta();
        let can_switch =
            matches!(snap.connection, ConnState::Connected) && !matches!(st, RadioState::Tx);
        if can_switch {
            for i in recalls {
                self.recall_memory(i);
            }
            self.step_frequency(freq_delta);
        }

        // ── TX timer ────────────────────────────────────────────────
        if matches!(st, RadioState::Tx) {
            if self.tx_start.is_none() {
                self.tx_start = Some(Instant::now());
            }
            // 30 s cap: release PTT locally and let the next snapshot
            // reflect it after the runtime processes the release.
            if let Some(start) = self.tx_start {
                if start.elapsed() >= Duration::from_millis(T::TX_LIMIT_MS as u64) {
                    let _ = self.cmd_tx.send(Cmd::PttUp);
                    self.ptt_held = false;
                    self.tx_start = None;
                }
            }
        } else {
            self.tx_start = None;
        }

        let central =
            egui::CentralPanel::default().frame(egui::Frame::NONE.fill(Color32::TRANSPARENT));
        central.show(ctx, |ui| {
            self.paint_strip(ui, &snap, st);
        });

        // Settings live in a real OS-level child viewport (own titlebar,
        // resizable, can be moved off the strip). Using
        // `show_viewport_immediate` rather than `_deferred` keeps the
        // closure free to borrow `&mut self`; the cost is that we re-run
        // the settings tree synchronously on every parent repaint, which
        // is fine — it's a tiny form.
        if self.show_settings {
            let viewport_id = egui::ViewportId::from_hash_of("toki-settings");
            let builder = egui::ViewportBuilder::default()
                .with_title("Toki — Settings")
                .with_inner_size([460.0, 520.0])
                .with_min_inner_size([380.0, 380.0]);
            ctx.show_viewport_immediate(viewport_id, builder, |child_ctx, _class| {
                // Each viewport carries its own font atlas — push the
                // brand fonts on the first frame after open so the
                // settings window doesn't show in Ubuntu-Light.
                if !self.settings_fonts_ready {
                    register_fonts(child_ctx);
                    child_ctx.set_visuals(egui::Visuals::dark());
                    self.settings_fonts_ready = true;
                }
                egui::CentralPanel::default()
                    .frame(egui::Frame::NONE.fill(T::SHELL).inner_margin(16.0))
                    .show(child_ctx, |ui| {
                        // Wrap the settings tree in a vertical scroll
                        // area so the (now fairly tall) PTT + memory +
                        // tuning + audio + beeps sections stay reachable
                        // even when the window is shorter than the
                        // content. `auto_shrink([false; 2])` lets the
                        // area fill the panel width so rows don't reflow
                        // narrower than the window.
                        egui::ScrollArea::vertical()
                            .auto_shrink([false; 2])
                            .show(ui, |ui| {
                                self.paint_settings_window(ui);
                            });
                    });
                // Keep the AUDIO section's VU meters animating even when
                // we're offline. While connected the parent repaints on a
                // 33 ms cadence and this immediate child rides along, but
                // offline the parent goes idle — without this the meters
                // would freeze the instant the user stops interacting,
                // exactly when they're trying to check their mic.
                child_ctx.request_repaint_after(Duration::from_millis(33));
                // Honor the OS close button (red dot / X / window menu).
                if child_ctx.input(|i| i.viewport().close_requested()) {
                    self.show_settings = false;
                }
            });
        } else if self.settings_fonts_ready {
            // Window just closed — arm `register_fonts` to run again on
            // the next open, since the child context will be re-created.
            self.settings_fonts_ready = false;
        }

        // Connect dialog (sibling viewport to Settings). Same
        // immediate-viewport pattern so the closure can borrow `self`
        // mutably; same one-shot font-priming flag.
        if self.show_connect {
            let viewport_id = egui::ViewportId::from_hash_of("toki-connect");
            let builder = egui::ViewportBuilder::default()
                .with_title("Toki — Connect")
                .with_inner_size([420.0, 280.0])
                .with_min_inner_size([360.0, 200.0])
                .with_resizable(false);
            ctx.show_viewport_immediate(viewport_id, builder, |child_ctx, _class| {
                if !self.connect_fonts_ready {
                    register_fonts(child_ctx);
                    child_ctx.set_visuals(egui::Visuals::dark());
                    self.connect_fonts_ready = true;
                }
                egui::CentralPanel::default()
                    .frame(egui::Frame::NONE.fill(T::SHELL).inner_margin(16.0))
                    .show(child_ctx, |ui| {
                        self.paint_connect_window(ui);
                    });
                if child_ctx.input(|i| i.viewport().close_requested()) {
                    self.show_connect = false;
                }
            });
        } else if self.connect_fonts_ready {
            self.connect_fonts_ready = false;
        }
    }

    /// Called once when eframe is tearing the window down (user closed
    /// the window, Cmd-Q, etc). If a session is open, send a
    /// [`Cmd::Shutdown`] and block briefly for the Leave RPC to land
    /// — so the server-side roster updates instantly instead of
    /// waiting ~10s for the reaper to time us out.
    ///
    /// The wait is bounded: the runtime thread normally completes
    /// a localhost gRPC round-trip in well under 100ms, so 800ms is
    /// generous. If we hit the timeout (dead server, unplugged
    /// network), we fall through and let the process exit — the
    /// reaper will catch the stale session shortly after anyway.
    fn on_exit(&mut self, _gl: Option<&eframe::glow::Context>) {
        let connected = matches!(
            self.state.lock().unwrap().connection,
            crate::state::ConnState::Connected | crate::state::ConnState::Connecting
        );
        if !connected {
            return;
        }
        // std mpsc rather than tokio oneshot so we can `recv_timeout`
        // from this sync context without juggling a second runtime.
        let (ack_tx, ack_rx) = std::sync::mpsc::channel();
        if self.cmd_tx.send(runtime::Cmd::Shutdown(ack_tx)).is_err() {
            // Runtime thread already gone — nothing left to do.
            return;
        }
        match ack_rx.recv_timeout(std::time::Duration::from_millis(800)) {
            Ok(()) => tracing::info!("graceful disconnect acknowledged"),
            Err(_) => tracing::warn!("graceful disconnect timed out; relying on server reaper"),
        }
    }
}

impl TokiApp {
    fn paint_strip(&mut self, ui: &mut egui::Ui, snap: &StateSnapshot, st: RadioState) {
        let rect = ui.max_rect();
        let painter = ui.painter().clone();

        // ── Chassis ────────────────────────────────────────────────
        // Three-stop vertical gradient per `design-tokens.md`:
        //   shell_top (0%) → shell (50%) → shell_bottom (100%).
        // Painted via `egui_colorgradient`, then stroked with a 1 px
        // edge color and a 1 px white-6% top highlight.
        let corners = CornerRadius::same(T::RADIUS_WIDGET as u8);
        paint_vertical_gradient(
            &painter,
            rect,
            corners,
            &[(0.0, T::SHELL_TOP), (0.5, T::SHELL), (1.0, T::SHELL_BOTTOM)],
        );
        painter.rect_stroke(
            rect,
            corners,
            Stroke::new(1.0, T::SHELL_EDGE),
            StrokeKind::Inside,
        );
        // 1 px top highlight (chassis inner bevel).
        let y = rect.top() + 0.5;
        painter.line_segment(
            [
                Pos2::new(rect.left() + T::RADIUS_WIDGET, y),
                Pos2::new(rect.right() - T::RADIUS_WIDGET, y),
            ],
            Stroke::new(1.0, T::HIGHLIGHT),
        );

        let pad = T::PAD_OUTER;
        let inner = Rect::from_min_size(
            Pos2::new(rect.left() + pad, rect.top() + pad),
            Vec2::new(rect.width() - 2.0 * pad, rect.height() - 2.0 * pad),
        );

        // ── Row layout: topbar, main, bottom ───────────────────────
        let topbar_rect = Rect::from_min_size(inner.min, Vec2::new(inner.width(), T::TOPBAR_H));
        let bottom_rect = Rect::from_min_size(
            Pos2::new(inner.left(), inner.bottom() - T::BOTTOM_H),
            Vec2::new(inner.width(), T::BOTTOM_H),
        );
        let main_rect = Rect::from_min_max(
            Pos2::new(inner.left(), topbar_rect.bottom() + T::GAP_ROW),
            Pos2::new(inner.right(), bottom_rect.top() - T::GAP_ROW),
        );

        self.paint_topbar(ui, &painter, topbar_rect, snap, st);
        self.paint_main(ui, &painter, main_rect, snap, st);
        self.paint_bottom(ui, &painter, bottom_rect, st);
    }

    // ── Top bar ─────────────────────────────────────────────────────
    fn paint_topbar(
        &mut self,
        ui: &mut egui::Ui,
        painter: &egui::Painter,
        rect: Rect,
        snap: &StateSnapshot,
        st: RadioState,
    ) {
        let y_mid = rect.center().y;

        // Brand — "TOKI" with a soft phosphor glow.
        let brand_pos = Pos2::new(rect.left() + 2.0, y_mid);
        glow_text(
            painter,
            brand_pos,
            Align2::LEFT_CENTER,
            "TOKI",
            font_mono(13.0),
            T::PRIMARY,
            T::PRIMARY_GLOW,
            0.5,
        );

        // 1 px vertical divider after the brand.
        let brand_w = 42.0;
        let divider_x = rect.left() + 2.0 + brand_w + 10.0;
        painter.line_segment(
            [
                Pos2::new(divider_x, y_mid - 7.0),
                Pos2::new(divider_x, y_mid + 7.0),
            ],
            Stroke::new(1.0, T::DIVIDER),
        );

        // Callsign / connection-state label. Read from the live
        // `snap.display_name` rather than `config.connection.display_name`
        // so admin-driven renames (which mutate the live state via the
        // `DisplayNameChanged` event) take effect immediately on the
        // topbar without a reconnect. The config field stays as the
        // saved default for the *next* connection.
        let callsign = match &snap.connection {
            ConnState::Connected => snap.display_name.to_uppercase(),
            ConnState::Connecting => "CONNECTING…".into(),
            ConnState::Disconnected => "OFFLINE".into(),
            ConnState::Failed(_) => "FAILED".into(),
        };
        let callsign_w = painter
            .layout_no_wrap(callsign.clone(), font_mono(10.0), T::INK_DIM)
            .size()
            .x;
        painter.text(
            Pos2::new(divider_x + 10.0, y_mid),
            Align2::LEFT_CENTER,
            callsign,
            font_mono(10.0),
            T::INK_DIM,
        );

        // ── Update-available pill ──────────────────────────────────
        // Non-modal nudge: when a newer release is known (and not
        // skipped), show a small clickable amber pill after the callsign
        // that opens the release page. Drawn in the topbar so it's always
        // visible without opening Settings, but never interrupts the UI.
        let pill = {
            let us = self.update_state.lock().unwrap();
            match &us.phase {
                update::UpdatePhase::Available(info)
                    if self.config.update.skip_version.as_deref() != Some(info.latest.as_str()) =>
                {
                    Some((format!("↑ UPDATE v{}", info.latest), info.html_url.clone()))
                }
                _ => None,
            }
        };
        if let Some((label, url)) = pill {
            let font = font_mono(9.0);
            let text_w = painter
                .layout_no_wrap(label.clone(), font.clone(), T::WARN)
                .size()
                .x;
            let pill_pad = 7.0;
            let pill_h = 16.0;
            let pill_rect = Rect::from_min_size(
                Pos2::new(divider_x + 10.0 + callsign_w + 10.0, y_mid - pill_h / 2.0),
                Vec2::new(text_w + 2.0 * pill_pad, pill_h),
            );
            let resp = ui.allocate_rect(pill_rect, Sense::click());
            let hover_bg = if resp.hovered() {
                Color32::from_rgba_unmultiplied(255, 184, 77, 28)
            } else {
                Color32::TRANSPARENT
            };
            painter.rect(
                pill_rect,
                CornerRadius::same(3),
                hover_bg,
                Stroke::new(1.0, T::WARN),
                StrokeKind::Inside,
            );
            painter.text(
                pill_rect.center(),
                Align2::CENTER_CENTER,
                &label,
                font,
                T::WARN,
            );
            if resp.hovered() {
                ui.ctx().set_cursor_icon(egui::CursorIcon::PointingHand);
            }
            if resp.clicked() {
                update::open_release_page(&url);
            }
        }

        // ── Right cluster: status chip + mute + settings ───────────
        let mut x = rect.right();
        // Settings icon (rightmost).
        x -= T::ICON_BTN_D;
        let gear_rect = Rect::from_min_size(
            Pos2::new(x, y_mid - T::ICON_BTN_D / 2.0),
            Vec2::splat(T::ICON_BTN_D),
        );
        if self.icon_button(ui, painter, gear_rect, "⚙", self.show_settings) {
            self.show_settings = !self.show_settings;
        }
        x -= 14.0;

        // Mute icon.
        x -= T::ICON_BTN_D;
        let mute_rect = Rect::from_min_size(
            Pos2::new(x, y_mid - T::ICON_BTN_D / 2.0),
            Vec2::splat(T::ICON_BTN_D),
        );
        let mute_glyph = if self.muted { "🔇" } else { "🔊" };
        if self.icon_button(ui, painter, mute_rect, mute_glyph, self.muted) {
            self.toggle_mute();
        }
        x -= 14.0;

        // Disconnect icon — only shown when we have a live session.
        // Replaces the Settings panel's old CONNECT/DISCONNECT row now
        // that connection management lives outside Settings.
        if matches!(snap.connection, ConnState::Connected) {
            x -= T::ICON_BTN_D;
            let disc_rect = Rect::from_min_size(
                Pos2::new(x, y_mid - T::ICON_BTN_D / 2.0),
                Vec2::splat(T::ICON_BTN_D),
            );
            if self.icon_button(ui, painter, disc_rect, "⏻", false) {
                let _ = self.cmd_tx.send(Cmd::Disconnect);
            }
            x -= 14.0;

            // Signal-quality bars — cell-style 4-bar glyph from the
            // live loss/jitter/RTT score, with the raw numbers on hover.
            // Only while connected (no link to rate otherwise).
            let bars_w = 18.0;
            x -= bars_w;
            let bars_rect = Rect::from_min_size(
                Pos2::new(x, y_mid - T::ICON_BTN_D / 2.0),
                Vec2::new(bars_w, T::ICON_BTN_D),
            );
            self.paint_signal_bars(ui, painter, bars_rect, snap.conn_quality);
            x -= 14.0;
        }

        // Status chip: dot + label. Transport-down states win over
        // everything (you literally can't be on the air); then tuning
        // (debouncing channel switch); then radio activity. Per the
        // spec, the "reconnecting" dot blinks (1.1 s ease) and its
        // chip text is `CONN…` rather than the full word.
        let blink_alpha =
            0.4 + 0.6 * (0.5 + 0.5 * (self.elapsed_secs() * std::f32::consts::TAU / 1.1).sin());
        let (chip_color, chip_label, chip_glow, label_color) = match st {
            // Offline dot was reading as "alarming" at intensity 1.0;
            // toned to 0.5 so it still stands out against IDLE/RX
            // without screaming.
            RadioState::Offline => (T::WARN, "OFFLINE", 0.5, T::WARN),
            RadioState::Reconnecting => {
                let alpha = (blink_alpha * 255.0) as u8;
                let pulsing =
                    Color32::from_rgba_unmultiplied(T::TX.r(), T::TX.g(), T::TX.b(), alpha);
                (pulsing, "CONN…", 1.0, T::INK_DIM)
            }
            _ if self.is_tuning() => (T::TX, "TUNING", 1.0, T::INK_DIM),
            RadioState::Tx => (T::TX, "TX", 1.2, T::INK_DIM),
            RadioState::Rx => (T::PRIMARY, "RX", 1.2, T::INK_DIM),
            RadioState::Busy => (T::WARN, "BUSY", 1.0, T::INK_DIM),
            RadioState::Idle => (T::PRIMARY_DIM, "IDLE", 0.3, T::INK_DIM),
        };
        // Label first (we draw right-to-left): place label, then dot.
        let label_w = chip_label.len() as f32 * 6.5 + 8.0;
        x -= label_w;
        painter.text(
            Pos2::new(x, y_mid),
            Align2::LEFT_CENTER,
            chip_label,
            font_mono(10.0),
            label_color,
        );
        x -= 12.0;
        glow_dot(painter, Pos2::new(x, y_mid), 3.0, chip_color, chip_glow);
    }

    /// Cell-style 4-bar signal indicator driven by the connection-quality
    /// score. Four ascending bars: filled green→amber→red up to the bar
    /// count, the rest drawn as faint outlines. `None` (not yet measured)
    /// shows all four as faint outlines. Hover reveals the raw
    /// RTT / jitter / loss numbers.
    fn paint_signal_bars(
        &self,
        ui: &mut egui::Ui,
        painter: &egui::Painter,
        rect: Rect,
        quality: Option<crate::telemetry::ConnQuality>,
    ) {
        let bars = quality.and_then(|q| q.bars());
        // Colour by score: 3–4 healthy, 2 marginal, 0–1 poor.
        let fill = match bars {
            Some(b) if b >= 3 => T::PRIMARY, // healthy (phosphor green)
            Some(2) => Color32::from_rgb(0xff, 0xba, 0x4d), // marginal amber
            Some(_) => T::WARN,              // poor (red)
            None => T::INK_DIM,
        };
        let lit = bars.unwrap_or(0);
        let n = 4usize;
        let gap = 2.0;
        let bar_w = (rect.width() - gap * (n as f32 - 1.0)) / n as f32;
        // Cap the tallest bar to ~62% of the row height (shorter than the
        // neighbouring icon buttons) and center the cluster vertically so
        // it reads as a compact signal glyph rather than a full-height bar.
        let max_h = (rect.height() * 0.62).round();
        let baseline = rect.center().y + max_h / 2.0;
        for i in 0..n {
            // Ascending heights: shortest bar ~40% up to the capped height.
            let frac = 0.4 + 0.6 * (i as f32 / (n as f32 - 1.0));
            let h = max_h * frac;
            let bx = rect.left() + i as f32 * (bar_w + gap);
            let br =
                Rect::from_min_max(Pos2::new(bx, baseline - h), Pos2::new(bx + bar_w, baseline));
            if (i as u8) < lit {
                painter.rect_filled(br, CornerRadius::same(1), fill);
            } else {
                painter.rect_stroke(
                    br,
                    CornerRadius::same(1),
                    Stroke::new(1.0, T::DIVIDER),
                    StrokeKind::Inside,
                );
            }
        }

        // Hover tooltip with the raw metrics.
        let resp = ui.allocate_rect(rect, Sense::hover());
        if resp.hovered() {
            let text = match quality {
                Some(q) if q.fresh => format!(
                    "RTT {} ms · jitter {} ms · loss {:.2}%",
                    q.rtt_ms,
                    q.jitter_ms,
                    q.loss_pct_centi as f32 / 100.0,
                ),
                _ => "measuring link…".to_string(),
            };
            resp.on_hover_text(text);
        }
    }

    /// Generic topbar icon button: 28×28 with a faint border. Returns
    /// `true` on click. The "icon" itself is a 14 px glyph (we use
    /// emoji-as-glyph for v1; replace with SVG raster when font work
    /// lands).
    fn icon_button(
        &self,
        ui: &mut egui::Ui,
        painter: &egui::Painter,
        rect: Rect,
        glyph: &str,
        active: bool,
    ) -> bool {
        let resp = ui.allocate_rect(rect, Sense::click());
        let border = if active {
            T::PRIMARY
        } else {
            Color32::from_rgba_premultiplied(0x0f, 0x0f, 0x0f, 0x0f)
        };
        let color = if active { T::PRIMARY } else { T::INK };
        painter.rect(
            rect,
            CornerRadius::same(T::RADIUS_BUTTON as u8),
            Color32::TRANSPARENT,
            Stroke::new(1.0, border),
            StrokeKind::Inside,
        );
        painter.text(
            rect.center(),
            Align2::CENTER_CENTER,
            glyph,
            font_mono(14.0),
            color,
        );
        resp.clicked()
    }

    // ── Main row ────────────────────────────────────────────────────
    fn paint_main(
        &mut self,
        ui: &mut egui::Ui,
        painter: &egui::Painter,
        rect: Rect,
        snap: &StateSnapshot,
        st: RadioState,
    ) {
        let left_rect = Rect::from_min_size(rect.min, Vec2::new(T::OLED_LEFT_W, rect.height()));
        let center_rect = Rect::from_min_max(
            Pos2::new(left_rect.right() + T::GAP_ROW, rect.top()),
            rect.max,
        );
        self.paint_oled_left(ui, painter, left_rect, st);
        self.paint_oled_center(painter, center_rect, snap, st);
    }

    fn paint_oled_left(
        &mut self,
        ui: &mut egui::Ui,
        painter: &egui::Painter,
        rect: Rect,
        st: RadioState,
    ) {
        paint_panel(painter, rect, T::OLED, T::OLED_RIM, T::RADIUS_OLED, None);
        paint_scanlines(painter, rect, T::RADIUS_OLED);

        let pad_x = T::OLED_PAD_X;
        let pad_y = T::OLED_PAD_Y;

        // ── Activity light (top-right) ─────────────────────────────
        // No caption now that there's no other text up there — the
        // glowing dot reads on its own. Lit (primary, glowing) when
        // more than one member is on this frequency; dim ink_mute
        // when alone or disconnected.

        // Top row: CHANNEL NN label + activity light. The light glows
        // (primary) when more than one member is on this frequency,
        // sits dim (ink_mute) when we're alone or disconnected.
        let activity = self.snapshot_members_count() > 1;
        let label_y = rect.top() + pad_y + 4.0;
        let dot_x = rect.right() - pad_x - 4.0;
        let dot_y = label_y + 4.0;
        if activity {
            glow_dot(painter, Pos2::new(dot_x, dot_y), 3.0, T::PRIMARY, 1.0);
        } else {
            painter.circle_filled(Pos2::new(dot_x, dot_y), 3.0, T::INK_MUTE);
        }
        // Tiny "ACT" caption next to the light, on its left — gives
        // the dot a label without crowding the corner.
        painter.text(
            Pos2::new(dot_x - 6.0, dot_y),
            Align2::RIGHT_CENTER,
            "ACT",
            font_mono(7.0),
            if activity {
                T::PRIMARY_DIM
            } else {
                T::INK_MUTE
            },
        );

        // ── Channel name marquee (upper-left) ──────────────────────
        // The admin-assigned name of the current channel, scrolled in
        // the top-left corner of the OLED. Nothing is drawn when the
        // channel is unnamed or the feature is off (state.channel_name
        // is None). Clipped to a region that stops short of the ACT
        // caption + dot so it never collides with them.
        let channel_name = self.state.lock().unwrap().channel_name.clone();
        if let Some(name) = channel_name {
            if !name.is_empty() {
                // Vertically centered on `dot_y` so the name's midline
                // matches the ACT caption + activity dot on the right.
                let name_region = Rect::from_min_max(
                    Pos2::new(rect.left() + pad_x, dot_y - 6.0),
                    Pos2::new(dot_x - 26.0, dot_y + 6.0),
                );
                paint_marquee(
                    painter,
                    name_region,
                    &name,
                    font_mono(10.0),
                    T::PRIMARY_DIM,
                    self.elapsed_secs(),
                );
            }
        }

        // ── Frequency readout ──────────────────────────────────────
        // Now the only text on this panel, so we let it dominate:
        // bigger font and centered both horizontally and vertically
        // between the top edge and the chevron row. Width-fit via
        // `egui::Painter::layout_no_wrap` so we know the true glyph
        // advance and don't rely on a fudged "mono char width" guess.
        //
        // When transport is down we strip the readout to a dim "—"
        // and drop the glow entirely (per `behavior-spec.md`:
        // "frequency dimmed to ink-mute (no glow)"). The MHz suffix
        // disappears too — there's no value to label.
        let freq = T::frequency_of(self.channel_idx);
        let offline_view = st.is_transport_down();
        let freq_text = if offline_view {
            "—".to_string()
        } else {
            T::frequency_label(freq)
        };
        let active_color = if offline_view {
            T::INK_MUTE
        } else if matches!(st, RadioState::Tx) {
            T::TX
        } else {
            T::PRIMARY
        };
        let active_glow = if offline_view {
            // Transparent — `glow_text` paints multiple alpha-faded
            // copies in this color; the all-zero alpha effectively
            // skips them.
            Color32::TRANSPARENT
        } else if matches!(st, RadioState::Tx) {
            T::TX_GLOW
        } else {
            T::PRIMARY_GLOW
        };

        // Available horizontal space between the panel pads, minus a
        // bit of room for the " MHz" suffix.
        let available_w = rect.width() - 2.0 * pad_x;
        // Try a generous size; if the layout actually overflows we
        // step down. With the band's 3.2-digit numbers (e.g. "447.05")
        // 38 px fits the 200-px-wide regular OLED comfortably.
        let mut font_size = 38.0_f32;
        let unit_font = font_mono(12.0);
        let unit_galley =
            painter.layout_no_wrap("MHz".to_string(), unit_font.clone(), T::PRIMARY_DIM);
        let unit_advance = unit_galley.size().x + 6.0; // gap to digits
        loop {
            let g = painter.layout_no_wrap(freq_text.clone(), font_mono(font_size), active_color);
            if g.size().x + unit_advance <= available_w || font_size <= 22.0 {
                break;
            }
            font_size -= 1.0;
        }
        let freq_font = font_mono(font_size);
        let freq_galley =
            painter.layout_no_wrap(freq_text.clone(), freq_font.clone(), active_color);
        let block_w = freq_galley.size().x + unit_advance;
        // Bottom of the panel now stacks two rows: the chevron row,
        // and below it the M1–M4 memory row hugging the bottom edge.
        // Compute their geometry up front so the frequency readout can
        // center itself in whatever vertical band is left above them.
        let mem_h = 18.0;
        let mem_top = rect.bottom() - pad_y - mem_h;
        let chev_h = 24.0;
        let chev_cy = mem_top - 6.0 - chev_h / 2.0;
        // Vertically center the readout between the top edge (after the
        // activity-dot row) and the chevron row.
        let band_top = rect.top() + pad_y + 14.0;
        let band_bot = chev_cy - chev_h / 2.0 - 6.0;
        let center_y = (band_top + band_bot) * 0.5;
        let freq_left = rect.left() + (rect.width() - block_w) * 0.5;
        glow_text(
            painter,
            Pos2::new(freq_left, center_y),
            Align2::LEFT_CENTER,
            &freq_text,
            freq_font,
            active_color,
            active_glow,
            if offline_view { 0.0 } else { 1.0 },
        );
        // "MHz" baseline-aligned to the digits. The digits' baseline
        // sits roughly at (center + font_size * 0.30) for a mono font;
        // good enough that the suffix tracks the readout cleanly.
        // Suppressed in the offline view — the "—" placeholder
        // doesn't need a unit.
        let baseline_y = center_y + font_size * 0.30;
        if !offline_view {
            painter.text(
                Pos2::new(freq_left + freq_galley.size().x + 6.0, baseline_y),
                Align2::LEFT_BOTTOM,
                "MHz",
                unit_font,
                T::PRIMARY_DIM,
            );
        }
        // `baseline_y` is only relevant for the MHz suffix above; in
        // the offline branch nothing reads it, so suppress the unused
        // warning rather than reorder the block.
        let _ = baseline_y;

        // ── Chevron row (no label between them) ────────────────────
        let chev_w = 56.0;
        let left_chev = Rect::from_min_size(
            Pos2::new(rect.left() + pad_x, chev_cy - chev_h / 2.0),
            Vec2::new(chev_w, chev_h),
        );
        let right_chev = Rect::from_min_size(
            Pos2::new(rect.right() - pad_x - chev_w, chev_cy - chev_h / 2.0),
            Vec2::new(chev_w, chev_h),
        );

        // Chevron clicks switch channels. We disable cycling during TX
        // (you can't change frequency mid-transmission — the design
        // spec calls this out as a hard constraint). Changing in RX
        // is allowed: you simply leave the current peer's room.
        // Chevrons are off-limits during TX (you can't channel-hop
        // mid-transmission per the spec) AND while transport is down
        // (there's no room to join until we've handshaken).
        let can_switch = !matches!(st, RadioState::Tx) && !st.is_transport_down();
        let prev_idx = self.channel_idx;
        if can_switch && self.chevron(ui, painter, left_chev, "◀") {
            self.channel_idx = if self.channel_idx == 0 {
                T::FREQ_CHANNEL_COUNT - 1
            } else {
                self.channel_idx - 1
            };
        } else if !can_switch {
            self.chevron_disabled(painter, left_chev, "◀");
        }
        if can_switch && self.chevron(ui, painter, right_chev, "▶") {
            self.channel_idx = (self.channel_idx + 1) % T::FREQ_CHANNEL_COUNT;
        } else if !can_switch {
            self.chevron_disabled(painter, right_chev, "▶");
        }

        if prev_idx != self.channel_idx {
            self.schedule_frequency_change();
        }

        // ── Memory row (M1–M4 quick-recall presets) ────────────────
        let usable_l = rect.left() + pad_x;
        let usable_r = rect.right() - pad_x;
        let gap = 6.0;
        let btn_w = (usable_r - usable_l - 3.0 * gap) / 4.0;
        for i in 0..4 {
            let bx = usable_l + i as f32 * (btn_w + gap);
            let brect = Rect::from_min_size(Pos2::new(bx, mem_top), Vec2::new(btn_w, mem_h));
            self.memory_button(ui, painter, brect, i, can_switch);
        }
    }

    /// Switch the tuner to memory preset `i`'s saved frequency, if the
    /// slot holds a valid one. Shared by the on-screen M-button recall
    /// and the M1–M4 hotkeys. Callers are responsible for gating on
    /// "can switch" (connected, not transmitting) — this just moves the
    /// dial and schedules the debounced `ChangeFrequency`.
    fn recall_memory(&mut self, i: usize) {
        if let Some(label) = self.config.memory.slots().get(i).and_then(|s| s.clone()) {
            if let Some(idx) = T::channel_of_label(&label) {
                if idx != self.channel_idx {
                    self.channel_idx = idx;
                    self.schedule_frequency_change();
                }
            }
        }
    }

    /// Step the tuner by `delta` channels (positive = up), wrapping at
    /// the band edges exactly like the ◀ ▶ chevrons. Used by the tune
    /// up/down hotkeys. A zero delta is a no-op.
    fn step_frequency(&mut self, delta: i32) {
        if delta == 0 {
            return;
        }
        let n = T::FREQ_CHANNEL_COUNT as i32;
        let next = (((self.channel_idx as i32 + delta) % n) + n) % n;
        let next = next as usize;
        if next != self.channel_idx {
            self.channel_idx = next;
            self.schedule_frequency_change();
        }
    }

    /// Paint and handle one M1–M4 memory button.
    ///
    /// Gestures (all latched in `self.mem_press[i]` so an egui hit-test
    /// flicker mid-hold doesn't reset them):
    ///   * **Left click** — an empty slot saves the current frequency;
    ///     a filled slot recalls it (switches the tuner). Recall is
    ///     gated on `can_switch`, exactly like the chevrons.
    ///   * **Left hold** (≥ `MEM_HOLD`) — (re)assign the slot to the
    ///     current frequency, even if it was already set.
    ///   * **Right click** — clear a saved slot; releasing off the slot
    ///     aborts. A right-click on an empty slot does nothing.
    ///
    /// A filled slot pulses in the amber memory color so it's instantly
    /// distinguishable from an empty (dim, static) one.
    fn memory_button(
        &mut self,
        ui: &mut egui::Ui,
        painter: &egui::Painter,
        rect: Rect,
        i: usize,
        can_switch: bool,
    ) {
        let resp = ui.allocate_rect(rect, Sense::click_and_drag());
        let now = Instant::now();
        let (primary_down, secondary_down) =
            ui.input(|inp| (inp.pointer.primary_down(), inp.pointer.secondary_down()));
        let started_here = resp.is_pointer_button_down_on();

        let saved = self.config.memory.slots()[i].clone();
        let current_label = T::frequency_label(T::frequency_of(self.channel_idx));

        // What the gesture resolves to this frame; applied after the
        // borrow of `self.mem_press[i]` ends.
        enum Act {
            None,
            Save,   // write current_label into slot i
            Recall, // switch the tuner to the saved frequency
            Free,   // clear slot i
        }
        let mut act = Act::None;

        {
            let mp = &mut self.mem_press[i];

            // ── Left (primary): click = save/recall, hold = overwrite.
            if mp.primary_since.is_some() {
                if primary_down {
                    let held = now.duration_since(mp.primary_since.unwrap());
                    if !mp.primary_fired && held >= MEM_HOLD {
                        mp.primary_fired = true;
                        act = Act::Save; // overwrite
                    }
                } else {
                    // Released. A clean (non-hold) release is the
                    // short-click: save when empty, recall when filled.
                    if !mp.primary_fired {
                        act = if saved.is_some() {
                            Act::Recall
                        } else {
                            Act::Save
                        };
                    }
                    mp.primary_since = None;
                    mp.primary_fired = false;
                }
            } else if started_here && primary_down && !mp.secondary_latched {
                mp.primary_since = Some(now);
                mp.primary_fired = false;
            }

            // ── Right (secondary): a click clears a saved slot. We fire
            // on release (not press) so the held-down frames can paint
            // the "about to erase" preview, and a right-click on an empty
            // slot resolves to nothing. The clear only lands if the
            // pointer is still over the slot at release, so dragging off
            // aborts — and a latch left stale by a focus/visibility loss
            // mid-gesture can't fire a phantom clear on resume.
            if mp.secondary_latched {
                if !secondary_down {
                    // Don't let a destructive clear clobber a save/recall
                    // the left block already resolved this frame (a chord
                    // where both buttons release together): the left
                    // gesture wins, the stray right release is dropped.
                    if matches!(act, Act::None) && saved.is_some() && resp.hovered() {
                        act = Act::Free;
                    }
                    mp.secondary_latched = false;
                }
            } else if started_here && secondary_down && mp.primary_since.is_none() {
                mp.secondary_latched = true;
            }
        }

        match act {
            Act::Save => {
                self.config.memory.set(i, Some(current_label.clone()));
                self.config.save();
            }
            Act::Free => {
                self.config.memory.set(i, None);
                self.config.save();
            }
            Act::Recall => {
                if can_switch {
                    self.recall_memory(i);
                }
            }
            Act::None => {}
        }

        // ── Paint ───────────────────────────────────────────────────
        // State priority (highest first):
        //   * erase-hold  → grey & dark (you're about to free it)
        //   * save-hold   → strong amber (you're about to write it)
        //   * on this freq→ green (the tuner is sitting on this preset)
        //   * saved       → gentle amber pulse
        //   * empty       → dim, static
        let filled = saved.is_some();
        let on_this = filled && saved.as_deref() == Some(current_label.as_str());
        let primary_holding = self.mem_press[i].primary_since.is_some();
        // `will_free` is the single source of truth shared with the
        // resolve block above: a right-press latched over a saved slot
        // with the pointer still on it would clear it on release. Driving
        // the erase preview off the same predicate keeps the "about to
        // erase" paint from ever disagreeing with what release does.
        let will_free = self.mem_press[i].secondary_latched && filled && resp.hovered();

        let t = ui.input(|inp| inp.time) as f32;
        let pulse = 0.5 + 0.5 * (t * 3.6).sin();
        let radius = CornerRadius::same(T::RADIUS_CHEVRON as u8);

        let (border, text_col) = if will_free {
            // Erasing: shift dark + grey while the right button is held
            // over a saved slot (releasing clears it). An empty slot has
            // nothing to erase, so it ignores the right press.
            painter.rect_filled(
                rect,
                radius,
                Color32::from_rgba_unmultiplied(0x00, 0x00, 0x00, 150),
            );
            (T::INK_DIM, T::INK_DIM)
        } else if primary_holding {
            // Saving/overwriting: a strong, breathing amber wash.
            let a = (110.0 + 60.0 * pulse) as u8;
            painter.rect_filled(
                rect,
                radius,
                Color32::from_rgba_unmultiplied(0xff, 0xba, 0x4d, a),
            );
            (T::TX, T::TX)
        } else if on_this {
            // The tuner is currently parked on this preset → green.
            let a = (26.0 + 46.0 * pulse) as u8;
            painter.rect_filled(
                rect,
                radius,
                Color32::from_rgba_unmultiplied(0x7f, 0xff, 0x90, a),
            );
            (T::PRIMARY, T::PRIMARY)
        } else if filled {
            // Saved but not the current channel → amber pulse.
            let a = (26.0 + 46.0 * pulse) as u8;
            painter.rect_filled(
                rect,
                radius,
                Color32::from_rgba_unmultiplied(0xff, 0xba, 0x4d, a),
            );
            (T::TX, T::TX)
        } else {
            // Empty.
            (T::INK_MUTE, T::INK_DIM)
        };

        painter.rect(
            rect,
            radius,
            Color32::TRANSPARENT,
            Stroke::new(1.0, border),
            StrokeKind::Inside,
        );
        let label = match i {
            0 => "M1",
            1 => "M2",
            2 => "M3",
            _ => "M4",
        };
        painter.text(
            rect.center(),
            Align2::CENTER_CENTER,
            label,
            font_mono(10.0),
            text_col,
        );
    }

    /// Greyed-out chevron when the user can't change frequency (TX
    /// state). Same border + glyph but in `ink_mute` instead of
    /// `primary`, and no click sense.
    fn chevron_disabled(&self, painter: &egui::Painter, rect: Rect, glyph: &str) {
        painter.rect(
            rect,
            CornerRadius::same(T::RADIUS_CHEVRON as u8),
            Color32::TRANSPARENT,
            Stroke::new(1.0, T::INK_MUTE),
            StrokeKind::Inside,
        );
        painter.text(
            rect.center(),
            Align2::CENTER_CENTER,
            glyph,
            font_mono(11.0),
            T::INK_MUTE,
        );
    }

    fn chevron(&self, ui: &mut egui::Ui, painter: &egui::Painter, rect: Rect, glyph: &str) -> bool {
        let resp = ui.allocate_rect(rect, Sense::click());
        painter.rect(
            rect,
            CornerRadius::same(T::RADIUS_CHEVRON as u8),
            Color32::TRANSPARENT,
            Stroke::new(1.0, T::PRIMARY_INK),
            StrokeKind::Inside,
        );
        painter.text(
            rect.center(),
            Align2::CENTER_CENTER,
            glyph,
            font_mono(11.0),
            T::PRIMARY,
        );
        resp.clicked()
    }

    fn paint_oled_center(
        &self,
        painter: &egui::Painter,
        rect: Rect,
        snap: &StateSnapshot,
        st: RadioState,
    ) {
        paint_panel(painter, rect, T::OLED, T::OLED_RIM, T::RADIUS_OLED, None);
        paint_scanlines(painter, rect, T::RADIUS_OLED);

        // Transport-down states replace the waveform + status with a
        // dedicated "OfflineCenter" panel (icon + reason + footer).
        if st.is_transport_down() {
            self.paint_offline_center(painter, rect, snap, st);
            return;
        }

        // Carve out top portion for the waveform, bottom strip for the
        // status line (10 px high text, a few px of padding).
        let pad_x = 12.0;
        let pad_y = 8.0;
        let status_h = 18.0;
        let wave_rect = Rect::from_min_max(
            Pos2::new(rect.left() + pad_x, rect.top() + pad_y),
            Pos2::new(rect.right() - pad_x, rect.bottom() - status_h - pad_y),
        );
        self.paint_waveform(painter, wave_rect, st);

        // 1 px primary_ink divider between waveform and status row.
        let divider_y = rect.bottom() - status_h - 2.0;
        painter.line_segment(
            [
                Pos2::new(rect.left() + pad_x, divider_y),
                Pos2::new(rect.right() - pad_x, divider_y),
            ],
            Stroke::new(1.0, T::PRIMARY_INK),
        );

        // Status line: state-dependent left/right text.
        let status_y = rect.bottom() - status_h / 2.0;
        let left_x = rect.left() + pad_x;
        let right_x = rect.right() - pad_x;
        match st {
            // Transport-down arms are unreachable here — the early
            // return above (in `paint_oled_center`) routes them to
            // `paint_offline_center` before we get here. Cover them
            // for exhaustiveness so any future code rearrangement
            // doesn't silently lose the offline UI.
            RadioState::Offline | RadioState::Reconnecting => {
                // Render an empty status row; the offline panel
                // owns this entire OLED above.
            }
            RadioState::Idle => {
                let label = if matches!(snap.connection, ConnState::Connected) {
                    "READY · CHANNEL CLEAR"
                } else if matches!(snap.connection, ConnState::Connecting) {
                    "CONNECTING…"
                } else {
                    "OFFLINE · OPEN SETTINGS TO CONNECT"
                };
                painter.text(
                    Pos2::new(left_x, status_y),
                    Align2::LEFT_CENTER,
                    label,
                    font_mono(10.0),
                    T::INK_DIM,
                );
                let hotkey_label = self
                    .config
                    .hotkey
                    .to_input()
                    .map(hotkey::format)
                    .unwrap_or_else(|| "—".into());
                painter.text(
                    Pos2::new(right_x, status_y),
                    Align2::RIGHT_CENTER,
                    format!("HOLD {} TO TX", hotkey_label.to_uppercase()),
                    font_mono(10.0),
                    T::INK_MUTE,
                );
            }
            RadioState::Tx => {
                glow_text(
                    painter,
                    Pos2::new(left_x, status_y),
                    Align2::LEFT_CENTER,
                    "● TRANSMITTING",
                    font_mono(10.0),
                    T::TX,
                    T::TX_GLOW,
                    0.6,
                );
                let remaining = self
                    .tx_start
                    .map(|s| T::TX_LIMIT_MS as f32 / 1000.0 - s.elapsed().as_secs_f32())
                    .unwrap_or(T::TX_LIMIT_MS as f32 / 1000.0)
                    .max(0.0);
                painter.text(
                    Pos2::new(right_x, status_y),
                    Align2::RIGHT_CENTER,
                    format!("{:.1}s LEFT", remaining),
                    font_mono(10.0),
                    T::TX,
                );
            }
            RadioState::Rx => {
                let peer_label = if snap.holder_name.is_empty() {
                    "PEER".into()
                } else {
                    snap.holder_name.to_uppercase()
                };
                glow_text(
                    painter,
                    Pos2::new(left_x, status_y),
                    Align2::LEFT_CENTER,
                    &format!("◐ {peer_label}"),
                    font_mono(10.0),
                    T::PRIMARY,
                    T::PRIMARY_GLOW,
                    0.6,
                );
                painter.text(
                    Pos2::new(right_x, status_y),
                    Align2::RIGHT_CENTER,
                    "RECEIVING",
                    font_mono(10.0),
                    T::INK_DIM,
                );
            }
            RadioState::Busy => {
                painter.text(
                    Pos2::new(left_x, status_y),
                    Align2::LEFT_CENTER,
                    "⊘ CHANNEL BUSY · WAIT FOR CLEAR",
                    font_mono(10.0),
                    T::WARN,
                );
            }
        }
    }

    /// The center-OLED contents when transport is down. Two rows:
    ///   * Hero: animated round icon (wifi-off blinking when offline,
    ///     refresh spinning when reconnecting) + title + reason.
    ///   * Footer: server URL on the left, "TRANSMISSION DISABLED" /
    ///     "PLEASE WAIT" on the right.
    fn paint_offline_center(
        &self,
        painter: &egui::Painter,
        rect: Rect,
        snap: &StateSnapshot,
        st: RadioState,
    ) {
        let pad_x = 12.0;
        let pad_y = 8.0;
        let inner = Rect::from_min_max(
            Pos2::new(rect.left() + pad_x, rect.top() + pad_y),
            Pos2::new(rect.right() - pad_x, rect.bottom() - pad_y),
        );
        let is_offline = matches!(st, RadioState::Offline);
        let accent = if is_offline { T::WARN } else { T::TX };
        // Use the dimmed `*_GLOW` companion of each accent for halo
        // work — passing the saturated accent itself reads as a flare.
        let accent_glow = if is_offline { T::WARN_GLOW } else { T::TX_GLOW };

        // ── Hero row ────────────────────────────────────────────────
        let footer_h = 16.0;
        let hero_rect = Rect::from_min_max(
            inner.min,
            Pos2::new(inner.right(), inner.bottom() - footer_h - 4.0),
        );
        let icon_d = 38.0;
        let icon_center = Pos2::new(hero_rect.left() + icon_d / 2.0 + 2.0, hero_rect.center().y);

        // Icon background — only offline gets the soft red wash; the
        // reconnecting icon spins on a transparent backdrop.
        if is_offline {
            // Blink the wash with a 1.6s pulse: 0.5 ↔ 1.0 of base alpha.
            // Halved from the original 8% to 4% — red surfaces stack
            // visually faster than amber, so even the previous tiny
            // value was reading as a hot wash.
            let pulse = 0.5 + 0.5 * (self.elapsed_secs() * std::f32::consts::TAU / 1.6).sin();
            let alpha = (0.04 * 255.0 * pulse) as u8;
            let wash = Color32::from_rgba_unmultiplied(accent.r(), accent.g(), accent.b(), alpha);
            painter.circle_filled(icon_center, icon_d / 2.0, wash);
        }
        // Outer glow halo. Reconnecting (amber) keeps the previous
        // intensity; offline (red) is dimmed to ~half because the eye
        // weights red glows heavier than amber.
        let halo_alpha = if is_offline { 16 } else { 30 };
        painter.circle_filled(
            icon_center,
            icon_d / 2.0 + 4.0,
            Color32::from_rgba_unmultiplied(accent.r(), accent.g(), accent.b(), halo_alpha),
        );
        // Faint border to define the disc.
        painter.circle_stroke(
            icon_center,
            icon_d / 2.0,
            Stroke::new(
                1.0,
                Color32::from_rgba_unmultiplied(accent.r(), accent.g(), accent.b(), 90),
            ),
        );

        // Icon glyph.
        if is_offline {
            paint_wifi_barred_icon(painter, icon_center, 11.0, accent, T::OLED);
        } else {
            // 1.4 s/turn spin per the spec.
            let angle = self.elapsed_secs() * std::f32::consts::TAU / 1.4;
            paint_refresh_icon(painter, icon_center, 10.0, angle, accent);
        }

        // Text column to the right of the icon.
        let text_x = icon_center.x + icon_d / 2.0 + 14.0;
        let title = if is_offline {
            "NO SIGNAL"
        } else {
            "CONNECTING…"
        };
        let reason = offline_reason(snap, is_offline);
        // Title (mono 13, primary-glow style). Red gets a smaller
        // glow multiplier than amber — at parity the red bleeds.
        let title_intensity = if is_offline { 0.45 } else { 0.7 };
        glow_text(
            painter,
            Pos2::new(text_x, hero_rect.center().y - 6.0),
            Align2::LEFT_CENTER,
            title,
            font_mono(13.0),
            accent,
            accent_glow,
            title_intensity,
        );
        // Subtitle (truncated if needed).
        let max_subtitle_w = inner.right() - text_x;
        let subtitle = truncate_to_width(painter, &reason, font_mono(10.0), max_subtitle_w);
        painter.text(
            Pos2::new(text_x, hero_rect.center().y + 8.0),
            Align2::LEFT_CENTER,
            subtitle,
            font_mono(10.0),
            T::INK_DIM,
        );

        // ── Footer ─────────────────────────────────────────────────
        let footer_y = inner.bottom() - footer_h / 2.0;
        let divider_y = footer_y - footer_h / 2.0;
        let border_color = if is_offline {
            Color32::from_rgba_unmultiplied(accent.r(), accent.g(), accent.b(), 45)
        } else {
            T::PRIMARY_INK
        };
        painter.line_segment(
            [
                Pos2::new(inner.left(), divider_y),
                Pos2::new(inner.right(), divider_y),
            ],
            Stroke::new(1.0, border_color),
        );

        let server_label = self.config.connection.endpoint();
        painter.text(
            Pos2::new(inner.left(), footer_y + 2.0),
            Align2::LEFT_CENTER,
            "SERVER",
            font_mono(10.0),
            T::INK_DIM,
        );
        painter.text(
            Pos2::new(inner.left() + 50.0, footer_y + 2.0),
            Align2::LEFT_CENTER,
            &server_label,
            font_mono(10.0),
            T::INK,
        );
        let right_label = if is_offline {
            "TRANSMISSION DISABLED"
        } else {
            "PLEASE WAIT"
        };
        painter.text(
            Pos2::new(inner.right(), footer_y + 2.0),
            Align2::RIGHT_CENTER,
            right_label,
            font_mono(10.0),
            T::INK_MUTE,
        );
    }

    /// Frequency-domain histogram. Each bar is one spectrum bucket;
    /// height is the bucket's magnitude (after windowing + gamma).
    /// We mirror the bars top + bottom so the panel reads like an
    /// audio analyzer rather than a one-sided meter — keeps visual
    /// weight in the center the same as the old waveform.
    fn paint_waveform(&self, painter: &egui::Painter, rect: Rect, st: RadioState) {
        let active = matches!(st, RadioState::Tx | RadioState::Rx);
        let color = match st {
            RadioState::Tx => T::TX,
            _ if active => T::PRIMARY,
            _ => T::PRIMARY_INK,
        };
        let fill_alpha = if active { 235 } else { 100 };
        let fill = Color32::from_rgba_unmultiplied(color.r(), color.g(), color.b(), fill_alpha);

        // Dashed center axis — kept from the old waveform, ties the
        // mirrored bars to a clean baseline.
        let mid_y = rect.center().y;
        let mut x = rect.left();
        while x < rect.right() {
            let x_end = (x + 2.0).min(rect.right());
            painter.line_segment(
                [Pos2::new(x, mid_y), Pos2::new(x_end, mid_y)],
                Stroke::new(0.5, T::PRIMARY_INK),
            );
            x += 5.0;
        }

        let bars = self.spectrum_bars.len();
        if bars == 0 {
            return;
        }
        // Bar geometry: leave a small gap between bars so each one
        // reads independently. A bar covers ~70% of its slot width.
        let slot_w = rect.width() / bars as f32;
        let bar_w = (slot_w * 0.72).max(2.0);
        let half_h = rect.height() / 2.0;

        // Soft halo behind active bars — paints a slightly taller
        // version with low alpha so each bar gets a phosphor bloom.
        if active {
            let halo = Color32::from_rgba_unmultiplied(color.r(), color.g(), color.b(), 50);
            for (i, &v) in self.spectrum_bars.iter().enumerate() {
                let amp = (v * (half_h - 2.0)).max(1.5) + 1.5;
                let cx = rect.left() + (i as f32 + 0.5) * slot_w;
                let bar = Rect::from_min_max(
                    Pos2::new(cx - bar_w * 0.5 - 1.0, mid_y - amp),
                    Pos2::new(cx + bar_w * 0.5 + 1.0, mid_y + amp),
                );
                painter.rect_filled(bar, CornerRadius::same(1), halo);
            }
        }

        for (i, &v) in self.spectrum_bars.iter().enumerate() {
            // Cap the minimum so the baseline reads as a continuous
            // "floor" of dim bars instead of empty space when there's
            // no signal.
            let amp = (v * (half_h - 2.0)).max(1.5);
            let cx = rect.left() + (i as f32 + 0.5) * slot_w;
            let bar = Rect::from_min_max(
                Pos2::new(cx - bar_w * 0.5, mid_y - amp),
                Pos2::new(cx + bar_w * 0.5, mid_y + amp),
            );
            painter.rect_filled(bar, CornerRadius::same(1), fill);
        }
    }

    // ── Bottom row: knob + PTT ─────────────────────────────────────
    fn paint_bottom(
        &mut self,
        ui: &mut egui::Ui,
        painter: &egui::Painter,
        rect: Rect,
        st: RadioState,
    ) {
        // Two knobs on the left, vertically centred and laid out side
        // by side: mic gain (capture) first, then speaker gain
        // (playback). The output mute toggle still lives in the top
        // bar — the SPK knob just sets the *level* that's restored on
        // unmute (and applied immediately while unmuted).
        let knob_y = rect.center().y - 4.0;
        let knob_gap = T::GAP_BOTTOM;
        let mic_rect = Rect::from_center_size(
            Pos2::new(rect.left() + T::KNOB_D / 2.0 + 4.0, knob_y),
            Vec2::splat(T::KNOB_D),
        );
        let spk_rect = Rect::from_center_size(
            Pos2::new(mic_rect.right() + knob_gap + T::KNOB_D / 2.0, knob_y),
            Vec2::splat(T::KNOB_D),
        );
        // Third knob: stereo balance (L↔R) for the mono-earpiece effect.
        let bal_rect = Rect::from_center_size(
            Pos2::new(spk_rect.right() + knob_gap + T::KNOB_D / 2.0, knob_y),
            Vec2::splat(T::KNOB_D),
        );
        self.paint_knob(ui, painter, mic_rect, KnobKind::Mic);
        self.paint_knob(ui, painter, spk_rect, KnobKind::Speaker);
        self.paint_knob(ui, painter, bal_rect, KnobKind::Balance);
        // Captions — short labels so they fit cleanly under the
        // 42 px knobs at 8 px mono.
        for (r, label) in [
            (mic_rect, "MIC VOL"),
            (spk_rect, "SPK VOL"),
            (bal_rect, "BALANCE"),
        ] {
            painter.text(
                Pos2::new(r.center().x, r.bottom() + 8.0),
                Align2::CENTER_CENTER,
                label,
                font_mono(8.0),
                T::INK_MUTE,
            );
        }

        // PTT button (or Reconnect button when transport is down) —
        // fills the rest of the row.
        let ptt_x = bal_rect.right() + T::GAP_BOTTOM;
        let ptt_rect = Rect::from_min_size(
            Pos2::new(ptt_x, rect.center().y - T::PTT_H / 2.0),
            Vec2::new(rect.right() - ptt_x, T::PTT_H),
        );
        if st.is_transport_down() {
            // Reconnecting keeps the single sweep button — a connect
            // attempt is already in flight, so offering "quick" vs
            // "new" would be misleading. Offline gets the choice.
            if matches!(st, RadioState::Reconnecting) {
                self.paint_reconnect(ui, painter, ptt_rect, st);
            } else {
                self.paint_offline_choice(ui, painter, ptt_rect);
            }
        } else {
            self.paint_ptt(ui, painter, ptt_rect, st);
        }
    }

    /// The PTT button's replacement when we're not online. Same
    /// dimensions and corner radius — only the surface and behavior
    /// differ. Clicking while `Offline` triggers a reconnect; while
    /// `Reconnecting` the button is non-interactive (cursor doesn't
    /// matter — there's no hover state to speak of yet).
    fn paint_reconnect(
        &mut self,
        ui: &mut egui::Ui,
        painter: &egui::Painter,
        rect: Rect,
        st: RadioState,
    ) {
        let is_offline = matches!(st, RadioState::Offline);
        let (top, bottom, border, label, hint, glyph_color) = if is_offline {
            (
                Color32::from_rgb(0x6b, 0x21, 0x21),
                Color32::from_rgb(0x3b, 0x12, 0x12),
                Color32::from_rgba_unmultiplied(T::WARN.r(), T::WARN.g(), T::WARN.b(), 128),
                "RECONNECT",
                "TAP TO RETRY",
                T::WARN,
            )
        } else {
            (
                Color32::from_rgb(0x6b, 0x49, 0x1c),
                Color32::from_rgb(0x3f, 0x2a, 0x10),
                T::TX,
                "CONNECTING…",
                "· · ·",
                T::TX,
            )
        };

        let sense = if is_offline {
            Sense::click()
        } else {
            Sense::hover()
        };
        let resp = ui.allocate_rect(rect, sense);
        if is_offline && resp.clicked() {
            // Re-dispatch a fresh Connect with the saved config. The
            // runtime ignores the request if a session is already open;
            // here it can't be, so this always fires the handshake.
            let frequency = T::frequency_label(T::frequency_of(self.channel_idx));
            let _ = self.cmd_tx.send(Cmd::Connect {
                server: self.config.connection.endpoint(),
                display_name: self.config.connection.display_name.trim().to_string(),
                frequency,
                password: self.config.connection.password.clone(),
            });
        }

        // Background gradient + 1 px inset stroke matches the PTT
        // button's recipe — same chassis aesthetic.
        let corners = CornerRadius::same(T::RADIUS_PTT as u8);
        paint_vertical_gradient(painter, rect, corners, &[(0.0, top), (1.0, bottom)]);
        painter.rect_stroke(rect, corners, Stroke::new(1.0, border), StrokeKind::Inside);

        if !is_offline {
            // Sweep highlight: a soft band sliding left-to-right
            // every 1.6 s, drawn as a couple of short vertical bars
            // with low alpha. egui can't smoothly mask a moving
            // gradient against a rounded rect without a custom mesh,
            // so this is the cheap approximation.
            let t = (self.elapsed_secs() / 1.6).fract();
            let band_x = rect.left() + (rect.width() + 80.0) * t - 40.0;
            let bar_w = 24.0;
            let bar_rect = Rect::from_min_max(
                Pos2::new(band_x, rect.top() + 1.0),
                Pos2::new(band_x + bar_w, rect.bottom() - 1.0),
            );
            let bar_rect = bar_rect.intersect(rect);
            if bar_rect.is_positive() {
                painter.rect_filled(
                    bar_rect,
                    CornerRadius::ZERO,
                    Color32::from_rgba_unmultiplied(T::TX.r(), T::TX.g(), T::TX.b(), 40),
                );
            }
        }

        // Left cluster: round icon + glowing label.
        let icon_x = rect.left() + 18.0 + 8.0;
        let label_y = rect.center().y;
        if is_offline {
            paint_wifi_off_icon(painter, Pos2::new(icon_x, label_y), 8.0, glyph_color);
        } else {
            let angle = self.elapsed_secs() * std::f32::consts::TAU / 1.4;
            paint_refresh_icon(painter, Pos2::new(icon_x, label_y), 7.5, angle, glyph_color);
        }
        let text_x = icon_x + 18.0;
        glow_text(
            painter,
            Pos2::new(text_x, label_y),
            Align2::LEFT_CENTER,
            label,
            font_ui(13.0, true),
            glyph_color,
            if is_offline { T::WARN_GLOW } else { T::TX_GLOW },
            if is_offline { 0.45 } else { 0.7 },
        );

        // Right hint.
        painter.text(
            Pos2::new(rect.right() - 18.0, label_y),
            Align2::RIGHT_CENTER,
            hint,
            font_mono(9.0),
            Color32::from_rgba_unmultiplied(0xff, 0xff, 0xff, 100),
        );
    }

    /// Offline-state replacement for the PTT button: split horizontally
    /// into a left "Quick Connect" (uses the saved config) and a right
    /// "New Connection" (opens the Connect dialog viewport). Same
    /// outer dimensions and corner radius as `paint_ptt` so the row
    /// height doesn't jitter when the state transitions.
    fn paint_offline_choice(&mut self, ui: &mut egui::Ui, painter: &egui::Painter, rect: Rect) {
        let gap = 6.0;
        // ~60% for Quick Connect (the default action), ~40% for New
        // Connection. The subtext under Quick Connect ("server@call")
        // needs the extra width to render at a readable size.
        let split = (rect.width() - gap) * 0.6;
        let quick_rect =
            Rect::from_min_max(rect.min, Pos2::new(rect.left() + split, rect.bottom()));
        let new_rect =
            Rect::from_min_max(Pos2::new(quick_rect.right() + gap, rect.top()), rect.max);

        self.paint_quick_connect_button(ui, painter, quick_rect);
        self.paint_new_connection_button(ui, painter, new_rect);
    }

    /// Left half of `paint_offline_choice`. Dispatches `Cmd::Connect`
    /// with the currently-saved config when clicked. Disabled (greyed)
    /// when no server or callsign is on file — Quick Connect with an
    /// empty target would just bounce off the runtime.
    fn paint_quick_connect_button(
        &mut self,
        ui: &mut egui::Ui,
        painter: &egui::Painter,
        rect: Rect,
    ) {
        let host = self.config.connection.host.trim().to_string();
        let display_name = self.config.connection.display_name.trim().to_string();
        let enabled = !host.is_empty() && !display_name.is_empty();
        let endpoint = self.config.connection.endpoint();

        let sense = if enabled {
            Sense::click()
        } else {
            Sense::hover()
        };
        let resp = ui.allocate_rect(rect, sense);
        if enabled && resp.clicked() {
            let frequency = T::frequency_label(T::frequency_of(self.channel_idx));
            let _ = self.cmd_tx.send(Cmd::Connect {
                server: endpoint.clone(),
                display_name: display_name.clone(),
                frequency,
                password: self.config.connection.password.clone(),
            });
        }

        // Phosphor-tinted card to mark this as the primary action when
        // a saved config exists. When disabled we paint the same idle
        // dark surface the PTT button uses so it visually recedes.
        let (top, bottom, border, label_color, accent_color, accent_glow) = if enabled {
            (
                Color32::from_rgb(0x16, 0x33, 0x1c),
                Color32::from_rgb(0x09, 0x1c, 0x0e),
                T::PRIMARY_INK,
                T::INK,
                T::PRIMARY,
                T::PRIMARY_GLOW,
            )
        } else {
            (
                T::PTT_IDLE_TOP,
                T::PTT_IDLE_BOTTOM,
                T::SHELL_EDGE,
                T::INK_MUTE,
                T::INK_MUTE,
                T::PRIMARY_GLOW,
            )
        };
        let corners = CornerRadius::same(T::RADIUS_PTT as u8);
        paint_vertical_gradient(painter, rect, corners, &[(0.0, top), (1.0, bottom)]);
        painter.rect_stroke(rect, corners, Stroke::new(1.0, border), StrokeKind::Inside);

        // Left: icon + label stacked over subtext.
        let icon_x = rect.left() + 18.0 + 6.0;
        let label_y = rect.center().y - 6.0;
        paint_plug_icon(painter, Pos2::new(icon_x, label_y), 7.5, accent_color);
        let text_x = icon_x + 18.0;
        glow_text(
            painter,
            Pos2::new(text_x, label_y),
            Align2::LEFT_CENTER,
            "QUICK CONNECT",
            font_ui(12.0, true),
            label_color,
            accent_glow,
            if enabled { 0.55 } else { 0.0 },
        );
        // Subtext: "<host:port>  ·  <callsign>" or a no-config hint.
        let subtext = if enabled {
            // Truncate to fit; we know the full string only fills here
            // when the user picked a long hostname.
            let max_w = rect.right() - text_x - 12.0;
            truncate_to_width(
                painter,
                &format!("{endpoint}  ·  {display_name}"),
                font_mono(9.0),
                max_w,
            )
        } else {
            "NO SAVED CONFIG · USE NEW CONNECTION".into()
        };
        painter.text(
            Pos2::new(text_x, rect.center().y + 9.0),
            Align2::LEFT_CENTER,
            subtext,
            font_mono(9.0),
            T::INK_DIM,
        );
    }

    /// Right half of `paint_offline_choice`. Opens the Connect dialog
    /// (separate viewport) with the URL + Username fields pre-filled
    /// from the last-saved config — saves typing for the common case
    /// where the user is fixing one field.
    fn paint_new_connection_button(
        &mut self,
        ui: &mut egui::Ui,
        painter: &egui::Painter,
        rect: Rect,
    ) {
        let resp = ui.allocate_rect(rect, Sense::click());
        if resp.clicked() {
            self.connect_form.host = self.config.connection.host.clone();
            self.connect_form.port = self.config.connection.port;
            self.connect_form.port_text = self.config.connection.port.to_string();
            self.connect_form.username = self.config.connection.display_name.clone();
            self.connect_form.password = self.config.connection.password.clone();
            self.show_connect = true;
        }

        let corners = CornerRadius::same(T::RADIUS_PTT as u8);
        paint_vertical_gradient(
            painter,
            rect,
            corners,
            &[(0.0, T::PTT_IDLE_TOP), (1.0, T::PTT_IDLE_BOTTOM)],
        );
        painter.rect_stroke(
            rect,
            corners,
            Stroke::new(1.0, T::SHELL_EDGE),
            StrokeKind::Inside,
        );

        // Plus glyph + label centered vertically.
        let label_y = rect.center().y;
        let icon_x = rect.left() + 18.0;
        let stroke = Stroke::new(1.5, T::INK);
        let half = 4.0;
        painter.line_segment(
            [
                Pos2::new(icon_x - half, label_y),
                Pos2::new(icon_x + half, label_y),
            ],
            stroke,
        );
        painter.line_segment(
            [
                Pos2::new(icon_x, label_y - half),
                Pos2::new(icon_x, label_y + half),
            ],
            stroke,
        );
        painter.text(
            Pos2::new(icon_x + 14.0, label_y),
            Align2::LEFT_CENTER,
            "NEW CONNECTION",
            font_ui(11.0, true),
            T::INK,
        );
    }

    fn paint_knob(
        &mut self,
        ui: &mut egui::Ui,
        painter: &egui::Painter,
        rect: Rect,
        kind: KnobKind,
    ) {
        let resp = ui.allocate_rect(rect, Sense::click_and_drag());

        // Click-drag adjusts the bound gain by the *horizontal* pixel
        // delta of the pointer. Right increases, left decreases —
        // matches the universal "slider" mental model and avoids the
        // "which way around the knob am I rotating?" ambiguity of
        // angle-based interaction. We previously rotated around the
        // knob center; users found that twitchy near the rim and
        // unintuitive near the dead-center.
        //
        // Sensitivity history (per-frame divisor in pixels):
        //   8 px/unit  ≈ snappy software slider
        //   250 px/unit ≈ current — a full 0→2.0 sweep takes ~500 px
        //   of horizontal drag (about half a monitor-width), which feels
        //   like a real, heavily-detented hardware pot: large gestures
        //   for coarse moves, deliberate small motions for the 1–2%
        //   nudges users actually want.
        if resp.dragged() {
            let dx = ui.input(|i| i.pointer.delta().x);
            if dx != 0.0 {
                let increment = dx / 250.0;
                // All three knobs operate on a normalized [0..1] unit.
                // Gains map unit→[0,2]; balance maps unit→[-1,1] so its
                // detent is the 12 o'clock centre.
                let current_unit = match kind {
                    KnobKind::Mic => self.config.audio.input_gain / 2.0,
                    KnobKind::Speaker => self.config.audio.output_gain / 2.0,
                    KnobKind::Balance => (self.config.audio.balance + 1.0) / 2.0,
                };
                let new_unit = (current_unit + increment).clamp(0.0, 1.0);
                match kind {
                    KnobKind::Mic => {
                        let gain = new_unit * 2.0;
                        self.config.audio.input_gain = gain;
                        // Mic gain is independent of mute (which
                        // gates the *output* side) — apply
                        // unconditionally.
                        self.audio_gains.set_input(gain);
                    }
                    KnobKind::Speaker => {
                        let gain = new_unit * 2.0;
                        self.config.audio.output_gain = gain;
                        if self.muted {
                            // Stage the post-mute target — applying
                            // now would punch through the mute.
                            self.gain_before_mute = gain;
                        } else {
                            self.audio_gains.set_output(gain);
                        }
                    }
                    KnobKind::Balance => {
                        // Snap to dead-centre when within ~3% so the
                        // user can reliably re-centre by dragging back.
                        let mut balance = new_unit * 2.0 - 1.0;
                        if balance.abs() < 0.03 {
                            balance = 0.0;
                        }
                        self.config.audio.balance = balance;
                        self.audio_gains.set_balance(balance);
                    }
                }
            }
        }
        if resp.drag_stopped() {
            self.config.save();
        }

        // Map the bound value → display unit [0..1]. Gains are [0,2];
        // balance is [-1,1] (centre 0 → unit 0.5, so the indicator
        // points straight up at dead-centre).
        let v = match kind {
            KnobKind::Mic => self.config.audio.input_gain / 2.0,
            KnobKind::Speaker => self.config.audio.output_gain / 2.0,
            KnobKind::Balance => (self.config.audio.balance + 1.0) / 2.0,
        }
        .clamp(0.0, 1.0);
        let angle_deg = -135.0 + v * 270.0;
        let angle = angle_deg.to_radians();

        // Outer disc.
        painter.circle_filled(rect.center(), T::KNOB_D / 2.0, T::SHELL_EDGE);
        painter.circle_stroke(
            rect.center(),
            T::KNOB_D / 2.0,
            Stroke::new(1.0, T::SHELL_BOTTOM),
        );
        // Inner disc.
        painter.circle_filled(rect.center(), T::KNOB_D / 2.0 - 4.0, T::SHELL_TOP);

        // Indicator line at top of inner disc, rotated to angle. The
        // Speaker knob dims its indicator while muted — visual cue
        // that the knob's value is staged but not currently audible.
        let r_outer = T::KNOB_D / 2.0 - 6.0;
        let r_inner = T::KNOB_D / 2.0 - 13.0;
        let ind_color = match kind {
            KnobKind::Speaker if self.muted => T::INK_MUTE,
            _ => T::PRIMARY,
        };
        let cx = rect.center().x;
        let cy = rect.center().y;
        // The "top" before rotation is -π/2; angle is rotation from that.
        let theta = angle - std::f32::consts::FRAC_PI_2;
        let p_out = Pos2::new(cx + theta.cos() * r_outer, cy + theta.sin() * r_outer);
        let p_in = Pos2::new(cx + theta.cos() * r_inner, cy + theta.sin() * r_inner);
        // Soft halo behind the indicator + the indicator itself.
        painter.line_segment(
            [p_out, p_in],
            Stroke::new(
                3.5,
                Color32::from_rgba_unmultiplied(ind_color.r(), ind_color.g(), ind_color.b(), 70),
            ),
        );
        painter.line_segment([p_out, p_in], Stroke::new(2.0, ind_color));
        // No tick marks around the rim — the indicator alone reads
        // cleanly and avoids the "what do these dots mean?" question.
    }

    fn paint_ptt(
        &mut self,
        ui: &mut egui::Ui,
        painter: &egui::Painter,
        rect: Rect,
        st: RadioState,
    ) {
        let snap = self.snapshot();
        let connected = matches!(snap.connection, ConnState::Connected);
        // Server-side mute (our own member-mute, or the channel we're on
        // being muted) makes the button inert: no press is sent and the
        // visuals go disabled. The server would refuse the press anyway —
        // this just makes the "you can't talk right now" obvious instead
        // of a button that looks live but silently does nothing.
        let muted = connected && snap.muted;
        let sense = if connected && !muted {
            Sense::click_and_drag()
        } else {
            Sense::hover()
        };
        let resp = ui.allocate_rect(rect, sense);

        // Click-and-hold semantics: we send PttDown/Up on edge
        // transitions of `ptt_held`. The on-screen button is always
        // clickable when connected — pressing during RX intentionally
        // triggers the `busy` UI state (the server still denies the
        // request, so no audio leaks).
        //
        // Release is gated on the *global* pointer state rather than
        // `is_pointer_button_down_on()` because the latter can flicker
        // to `false` for a frame or two during a sustained hold (egui's
        // hit-test against the button rect occasionally misses on
        // sub-pixel mouse motion or when other widgets repaint). Each
        // flicker used to fire a spurious PttUp / PttDown pair, which
        // hit the runtime's 250 ms cooldown and produced a visible
        // "constantly spammed" pulse pattern. Holding latches `ptt_held`
        // and we only let go when no pointer button is down anywhere.
        if connected && !muted {
            let any_down = ui.input(|i| i.pointer.any_down());
            let pressed_on_button = resp.is_pointer_button_down_on();
            if self.ptt_held {
                if !any_down {
                    self.ptt_held = false;
                    let _ = self.cmd_tx.send(Cmd::PttUp);
                }
            } else if pressed_on_button {
                self.ptt_held = true;
                let _ = self.cmd_tx.send(Cmd::PttDown);
            }
        } else if muted && self.ptt_held {
            // If a mute lands mid-hold, release locally so we don't sit
            // latched in `ptt_held` (the runtime already gates the mic).
            self.ptt_held = false;
            let _ = self.cmd_tx.send(Cmd::PttUp);
        }

        // Visuals per state. A mute overrides whatever the radio state
        // would otherwise show: inert dark button, red dot, and an
        // explicit "UNABLE TO TALK" so the cause is unmistakable.
        let (top, bottom, label, label_color, dot_color, border, glow_intensity) = if muted {
            (
                T::PTT_MUTED_TOP,
                T::PTT_MUTED_BOTTOM,
                "UNABLE TO TALK",
                T::INK_MUTE,
                T::WARN,
                T::SHELL_EDGE,
                0.0,
            )
        } else {
            match st {
                RadioState::Tx => (
                    T::PTT_TX_TOP,
                    T::PTT_TX_BOTTOM,
                    "TRANSMITTING",
                    T::TX,
                    T::TX,
                    T::TX,
                    1.4,
                ),
                RadioState::Busy => (
                    T::PTT_BUSY_TOP,
                    T::PTT_BUSY_BOTTOM,
                    "CHANNEL BUSY",
                    T::WARN,
                    T::WARN,
                    T::SHELL_EDGE,
                    0.0,
                ),
                _ => (
                    T::PTT_IDLE_TOP,
                    T::PTT_IDLE_BOTTOM,
                    "HOLD TO TALK",
                    T::INK,
                    T::PRIMARY,
                    T::SHELL_EDGE,
                    0.5,
                ),
            }
        };

        // Two-stop vertical gradient via the colorgradient helper —
        // see `paint_vertical_gradient` for the mesh logic. The button
        // also gets a 1 px rounded stroke around it.
        paint_vertical_gradient(
            painter,
            rect,
            CornerRadius::same(T::RADIUS_PTT as u8),
            &[(0.0, top), (1.0, bottom)],
        );
        painter.rect_stroke(
            rect,
            CornerRadius::same(T::RADIUS_PTT as u8),
            Stroke::new(1.0, border),
            StrokeKind::Inside,
        );

        // TX progress underline.
        if matches!(st, RadioState::Tx) {
            let progress = self
                .tx_start
                .map(|s| s.elapsed().as_millis() as f32 / T::TX_LIMIT_MS as f32)
                .unwrap_or(0.0)
                .clamp(0.0, 1.0);
            let bar = Rect::from_min_max(
                Pos2::new(rect.left(), rect.bottom() - 2.0),
                Pos2::new(rect.left() + rect.width() * progress, rect.bottom()),
            );
            painter.rect_filled(bar, CornerRadius::ZERO, T::TX);
        }

        // Left cluster: glowing dot + label.
        let label_x = rect.left() + 18.0;
        let label_y = rect.center().y;
        glow_dot(
            painter,
            Pos2::new(label_x, label_y),
            4.5,
            dot_color,
            glow_intensity,
        );
        let text_x = label_x + 18.0;
        if matches!(st, RadioState::Tx) {
            glow_text(
                painter,
                Pos2::new(text_x, label_y),
                Align2::LEFT_CENTER,
                label,
                font_ui(12.0, true),
                label_color,
                T::TX_GLOW,
                0.6,
            );
        } else {
            painter.text(
                Pos2::new(text_x, label_y),
                Align2::LEFT_CENTER,
                label,
                font_ui(12.0, true),
                label_color,
            );
        }

        // Right-edge hint with the configured PTT key.
        let hint = self
            .config
            .hotkey
            .to_input()
            .map(hotkey::format)
            .unwrap_or_else(|| "—".into())
            .to_uppercase();
        let hint_text = if matches!(st, RadioState::Tx) {
            format!("◀ {hint} ▶")
        } else {
            hint
        };
        painter.text(
            Pos2::new(rect.right() - 18.0, label_y),
            Align2::RIGHT_CENTER,
            hint_text,
            font_mono(9.0),
            Color32::from_rgba_unmultiplied(0xff, 0xff, 0xff, 90),
        );
    }

    // ── Connect dialog body ────────────────────────────────────────
    //
    // Painted into a child viewport (see `update`). Three text fields
    // (SERVER + PORT side-by-side, USERNAME, PASSWORD) plus a
    // Cancel / Connect button pair. The form is buffered in
    // `self.connect_form` — we only commit to `self.config.connection`
    // if the user clicks Connect.
    fn paint_connect_window(&mut self, ui: &mut egui::Ui) {
        ui.style_mut().visuals.override_text_color = Some(T::INK);

        section_header(ui, "NEW CONNECTION");

        settings_row(ui, "SERVER", |ui| {
            ui.add(
                egui::TextEdit::singleline(&mut self.connect_form.host)
                    .desired_width(180.0)
                    .hint_text("127.0.0.1")
                    .font(egui::TextStyle::Monospace),
            );
            ui.label("PORT");
            // Port renders against a separate text buffer so partial
            // entries while typing (e.g. "5005") don't snap back to
            // the previous valid value on every keystroke. We parse
            // on focus-loss; bad / out-of-range strings keep the
            // last good port.
            let resp = ui.add(
                egui::TextEdit::singleline(&mut self.connect_form.port_text)
                    .desired_width(60.0)
                    .hint_text("50051")
                    .font(egui::TextStyle::Monospace),
            );
            if resp.lost_focus() {
                if let Ok(p) = self.connect_form.port_text.trim().parse::<u16>() {
                    if p > 0 {
                        self.connect_form.port = p;
                    }
                }
                // Either way, re-sync the text field to the
                // committed port so the user can see what stuck.
                self.connect_form.port_text = self.connect_form.port.to_string();
            }
        });
        settings_row(ui, "USERNAME", |ui| {
            // Same uppercase / 10-char cap the old Settings row used —
            // server-side display name semantics haven't changed.
            let resp = ui.add(
                egui::TextEdit::singleline(&mut self.connect_form.username)
                    .desired_width(160.0)
                    .hint_text("CALLSIGN")
                    .font(egui::TextStyle::Monospace),
            );
            self.connect_form.username = self.connect_form.username.to_uppercase();
            if self.connect_form.username.len() > 10 {
                self.connect_form.username.truncate(10);
            }
            let _ = resp; // (text edit response unused beyond display)
        });
        settings_row(ui, "PASSWORD", |ui| {
            // Optional — leave blank when joining an open-mode server.
            // `password(true)` swaps the rendered glyphs for •••• so the
            // value isn't visible to anyone glancing at the screen, but
            // we still store and send the underlying string.
            ui.add(
                egui::TextEdit::singleline(&mut self.connect_form.password)
                    .desired_width(200.0)
                    .password(true)
                    .hint_text("optional")
                    .font(egui::TextStyle::Monospace),
            );
        });

        ui.add_space(10.0);
        ui.horizontal(|ui| {
            // Right-align the action pair: Cancel | Connect.
            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                let host = self.connect_form.host.trim().to_string();
                // Parse-on-press as a backup for the lost_focus
                // commit — handles the "user typed and clicked
                // Connect without ever tabbing out" case.
                if let Ok(p) = self.connect_form.port_text.trim().parse::<u16>() {
                    if p > 0 {
                        self.connect_form.port = p;
                    }
                }
                let port = self.connect_form.port;
                let username = self.connect_form.username.trim().to_string();
                let password = self.connect_form.password.clone();
                let can_connect = !host.is_empty() && !username.is_empty() && port > 0;

                let connect_btn = ui.add_enabled(can_connect, egui::Button::new("CONNECT"));
                if connect_btn.clicked() {
                    // Commit form → config, persist, dispatch the
                    // Connect command, then close the dialog.
                    self.config.connection.host = host.clone();
                    self.config.connection.port = port;
                    self.config.connection.display_name = username.clone();
                    self.config.connection.password = password.clone();
                    self.config.save();
                    let frequency = T::frequency_label(T::frequency_of(self.channel_idx));
                    let _ = self.cmd_tx.send(Cmd::Connect {
                        server: format!("{host}:{port}"),
                        display_name: username,
                        frequency,
                        password,
                    });
                    self.show_connect = false;
                }
                if ui.button("CANCEL").clicked() {
                    self.show_connect = false;
                }
            });
        });
    }

    // ── Settings window body ───────────────────────────────────────
    //
    // Painted into a child viewport (see `update`). No outer chrome
    // here — the OS window provides the titlebar and the
    // `CentralPanel`'s `Frame` provides the shell-coloured background
    // and 16 px inner margin. Two clearly-labelled sections:
    //
    //   * CUSTOMIZATION — PTT bind
    //   * AUDIO         — input/output device, mic gain
    //
    // Server URL / callsign / connect / disconnect live on the strip
    // and in the dedicated Connect dialog, not here.
    /// One key-binding row in Settings, shared by the PTT trigger and
    /// the four memory-recall hotkeys. While recording *this* target it
    /// shows the hold-progress bar + CANCEL; otherwise it shows the
    /// current binding plus BIND (and, for memory slots, CLEAR to
    /// unbind). Only one recording can be in flight at a time — pressing
    /// BIND on a different row cancels the previous capture.
    fn bind_row(&mut self, ui: &mut egui::Ui, label: &str, target: RecordTarget) {
        let current = match target {
            RecordTarget::Ptt => self.config.hotkey.to_input(),
            RecordTarget::PttSecondary => self.config.hotkey.to_input_secondary(),
            RecordTarget::Memory(i) => self.config.hotkey.memory(i).to_input(),
            RecordTarget::FreqUp => self.config.hotkey.freq_up.to_input(),
            RecordTarget::FreqDown => self.config.hotkey.freq_down.to_input(),
        }
        .map(hotkey::format);

        settings_row(ui, label, |ui| {
            if self.recording && self.recording_target == target {
                let progress = self.hotkey.hold_progress();
                ui.colored_label(T::TX, "hold any button for 1s…");
                ui.add(
                    egui::ProgressBar::new(progress)
                        .desired_width(80.0)
                        .desired_height(8.0)
                        .fill(T::TX),
                );
                if ui.button("CANCEL").clicked() {
                    self.recording = false;
                    self.hotkey.cancel_recording();
                }
            } else {
                ui.monospace(current.clone().unwrap_or_else(|| "(none)".into()));
                if ui.button("BIND").clicked() {
                    // Cancel any capture already in flight on another row
                    // so two rows can't both think they're recording.
                    if self.recording {
                        self.hotkey.cancel_recording();
                    }
                    if self.hotkey.start_recording() {
                        self.recording = true;
                        self.recording_target = target;
                    }
                }
                // The action hotkeys and the secondary PTT are optional,
                // so offer an explicit unbind. (The PRIMARY PTT has no
                // CLEAR — there's always meant to be a transmit trigger.)
                if !matches!(target, RecordTarget::Ptt)
                    && current.is_some()
                    && ui.button("CLEAR").clicked()
                {
                    match target {
                        RecordTarget::PttSecondary => {
                            let _ = self.hotkey.rebind_secondary(None);
                            self.config.hotkey.set_ptt_secondary(None);
                        }
                        RecordTarget::Memory(i) => {
                            self.hotkey.rebind_memory(i, None);
                            self.config.hotkey.set_memory(i, HotkeyBinding::default());
                        }
                        RecordTarget::FreqUp => {
                            self.hotkey.rebind_freq(true, None);
                            self.config.hotkey.freq_up = HotkeyBinding::default();
                        }
                        RecordTarget::FreqDown => {
                            self.hotkey.rebind_freq(false, None);
                            self.config.hotkey.freq_down = HotkeyBinding::default();
                        }
                        RecordTarget::Ptt => {}
                    }
                    self.config.save();
                }
            }
        });
    }

    fn paint_settings_window(&mut self, ui: &mut egui::Ui) {
        ui.style_mut().visuals.override_text_color = Some(T::INK);

        section_header(ui, "CUSTOMIZATION");

        self.bind_row(ui, "PTT", RecordTarget::Ptt);
        // Optional fallback trigger — bind a second device (e.g. a
        // keyboard key backing up a gamepad button). PTT fires while
        // either is held.
        self.bind_row(ui, "PTT (2ND)", RecordTarget::PttSecondary);

        ui.add_space(14.0);
        section_header(ui, "MEMORY HOTKEYS");
        for i in 0..4 {
            let label = match i {
                0 => "RECALL M1",
                1 => "RECALL M2",
                2 => "RECALL M3",
                _ => "RECALL M4",
            };
            self.bind_row(ui, label, RecordTarget::Memory(i));
        }

        ui.add_space(14.0);
        section_header(ui, "TUNING HOTKEYS");
        self.bind_row(ui, "FREQ UP", RecordTarget::FreqUp);
        self.bind_row(ui, "FREQ DOWN", RecordTarget::FreqDown);

        ui.add_space(14.0);
        section_header(ui, "AUDIO");

        settings_row(ui, "INPUT", |ui| {
            let prev = self.config.audio.input_device.clone();
            let selected = self
                .config
                .audio
                .input_device
                .as_deref()
                .unwrap_or("(system default)");
            egui::ComboBox::from_id_salt("input_dev")
                .selected_text(selected)
                .width(200.0)
                .show_ui(ui, |ui| {
                    ui.selectable_value(
                        &mut self.config.audio.input_device,
                        None,
                        "(system default)",
                    );
                    for name in &self.audio_devices.inputs {
                        ui.selectable_value(
                            &mut self.config.audio.input_device,
                            Some(name.clone()),
                            name,
                        );
                    }
                });
            if prev != self.config.audio.input_device {
                self.audio_control
                    .set_input(self.config.audio.input_device.clone());
                self.config.save();
            }
        });
        // Live mic VU bar. The capture stream runs continuously
        // (independent of PTT), so this moves whenever the user talks —
        // even offline, even with PTT not held. Makes "is my mic working
        // / which of these devices is the right one" obvious without
        // having to key up and ask someone.
        settings_row(ui, "INPUT LEVEL", |ui| {
            paint_level_meter(ui, self.audio_levels.input());
        });
        settings_row(ui, "OUTPUT", |ui| {
            let prev = self.config.audio.output_device.clone();
            let selected = self
                .config
                .audio
                .output_device
                .as_deref()
                .unwrap_or("(system default)");
            egui::ComboBox::from_id_salt("output_dev")
                .selected_text(selected)
                .width(200.0)
                .show_ui(ui, |ui| {
                    ui.selectable_value(
                        &mut self.config.audio.output_device,
                        None,
                        "(system default)",
                    );
                    for name in &self.audio_devices.outputs {
                        ui.selectable_value(
                            &mut self.config.audio.output_device,
                            Some(name.clone()),
                            name,
                        );
                    }
                });
            if prev != self.config.audio.output_device {
                self.audio_control
                    .set_output(self.config.audio.output_device.clone());
                self.config.save();
            }
        });
        // Output VU bar + a test-tone button. The meter confirms audio
        // is actually reaching the selected device (it moves while the
        // test tone, a roger beep, or incoming voice plays); the chime
        // is self-generated so it needs no session — press it offline to
        // check the speaker/earpiece and balance before connecting.
        settings_row(ui, "OUTPUT LEVEL", |ui| {
            paint_level_meter(ui, self.audio_levels.output());
        });
        settings_row(ui, "OUTPUT TEST", |ui| {
            if ui.button("TEST TONE").clicked() {
                let _ = self.cmd_tx.send(Cmd::TestTone);
            }
        });
        // Mic / Speaker gain sliders used to live here. They moved to
        // the bottom-row knobs (MIC VOL / SPK VOL) on the strip, which
        // adjust the same `config.audio.{input,output}_gain` fields —
        // duplicating them in Settings just left two ways to change
        // the same value out of sync.

        ui.add_space(14.0);
        section_header(ui, "VOICE DSP");

        // Capture-side processing toggles. Both write straight into
        // the shared `DspParams` atomics, so the change lands on the
        // very next 10 ms mic frame — no reconnect, no stream restart.
        // Off = bit-exact raw mic, for operators who want the
        // unprocessed CB character.
        settings_row(ui, "NOISE FILTER", |ui| {
            let mut v = self.config.audio.noise_suppression;
            if ui.checkbox(&mut v, "").changed() {
                self.config.audio.noise_suppression = v;
                self.dsp_params.set_noise_suppression(v);
                self.config.save();
            }
            ui.label(
                egui::RichText::new("RNNoise — strips steady background noise")
                    .color(T::INK_DIM)
                    .monospace()
                    .size(9.0),
            );
        });
        settings_row(ui, "AUTO GAIN", |ui| {
            let mut v = self.config.audio.agc;
            if ui.checkbox(&mut v, "").changed() {
                self.config.audio.agc = v;
                self.dsp_params.set_agc(v);
                self.config.save();
            }
            ui.label(
                egui::RichText::new("levels quiet & hot mics toward a fixed target")
                    .color(T::INK_DIM)
                    .monospace()
                    .size(9.0),
            );
        });

        ui.add_space(14.0);
        section_header(ui, "ROGER BEEPS");

        // Preset-based design: tone choice (acquire Hz, release Hz,
        // duration) is selected from a fixed table in
        // `BeepPreset::ALL`, so adding a new tone palette later is one
        // entry in that array rather than four fresh sliders here.
        // Volume stays per-user. TEST buttons preview via the
        // runtime's `Cmd::TestBeep`, keeping the audio pipeline on
        // a single thread.
        settings_row(ui, "PRESET", |ui| {
            let current = BeepPreset::by_id(&self.config.beeps.preset);
            let prev_id = current.id;
            egui::ComboBox::from_id_salt("toki-beep-preset")
                .selected_text(current.label)
                .show_ui(ui, |ui| {
                    for (i, preset) in BeepPreset::ALL.iter().enumerate() {
                        let selected = preset.id == current.id;
                        if ui.selectable_label(selected, preset.label).clicked() {
                            self.config.beeps.preset = preset.id.into();
                            self.beep_params.set_preset_index(i);
                        }
                    }
                });
            if self.config.beeps.preset != prev_id {
                self.config.save();
            }
            // Compact summary of the selected preset's two patterns
            // for the user's reference — total length per cue, plus a
            // note count when there's more than one step so users can
            // tell single-tone presets from multi-step ones at a
            // glance.
            let resolved = BeepPreset::by_id(&self.config.beeps.preset);
            let acq_steps = resolved.acquire.steps.len();
            let rel_steps = resolved.release.steps.len();
            let summary = if acq_steps == 1 && rel_steps == 1 {
                format!(
                    "{} ms / {} ms",
                    resolved.acquire.total_duration_ms(),
                    resolved.release.total_duration_ms()
                )
            } else {
                format!(
                    "{}-note {} ms / {}-note {} ms",
                    acq_steps,
                    resolved.acquire.total_duration_ms(),
                    rel_steps,
                    resolved.release.total_duration_ms(),
                )
            };
            ui.monospace(summary);
        });
        settings_row(ui, "VOLUME", |ui| {
            let mut v = self.config.beeps.volume;
            let resp = ui.add(egui::Slider::new(&mut v, 0.0..=1.0).show_value(false));
            ui.monospace(format!("{:>3.0}%", v * 100.0));
            if resp.changed() {
                self.config.beeps.volume = v;
                self.beep_params.set_volume(v);
            }
            if resp.drag_stopped() || resp.lost_focus() {
                self.config.save();
            }
        });
        settings_row(ui, "TEST", |ui| {
            if ui.button("TAKEN").clicked() {
                let _ = self.cmd_tx.send(Cmd::TestBeep(runtime::BeepKind::Acquire));
            }
            if ui.button("CLEARED").clicked() {
                let _ = self.cmd_tx.send(Cmd::TestBeep(runtime::BeepKind::Release));
            }
        });

        ui.add_space(14.0);
        section_header(ui, "UPDATES");

        settings_row(ui, "VERSION", |ui| {
            ui.label(
                egui::RichText::new(format!("v{}", update::current_version()))
                    .color(T::INK)
                    .monospace()
                    .size(10.0),
            );
        });

        // Snapshot the phase once for the status + action rows below.
        let phase = self.update_state.lock().unwrap().phase.clone();

        settings_row(ui, "STATUS", |ui| {
            let (text, color) = match &phase {
                update::UpdatePhase::Idle => ("—".to_string(), T::INK_DIM),
                update::UpdatePhase::Checking => ("checking…".to_string(), T::INK_DIM),
                update::UpdatePhase::UpToDate => ("up to date".to_string(), T::PRIMARY_DIM),
                update::UpdatePhase::Available(info) => {
                    (format!("v{} available", info.latest), T::WARN)
                }
                update::UpdatePhase::Error(e) => (e.clone(), T::WARN),
            };
            ui.label(egui::RichText::new(text).color(color).monospace().size(9.0));
        });

        settings_row(ui, "", |ui| {
            if ui.button("CHECK NOW").clicked() {
                update::spawn_check(self.update_state.clone(), self.egui_ctx.clone());
            }
            if let update::UpdatePhase::Available(info) = &phase {
                if ui.button("OPEN PAGE").clicked() {
                    update::open_release_page(&info.html_url);
                }
                if ui.button("SKIP").clicked() {
                    self.config.update.skip_version = Some(info.latest.clone());
                    self.config.save();
                }
            }
        });

        settings_row(ui, "AUTO-CHECK", |ui| {
            let mut v = self.config.update.auto_check;
            if ui.checkbox(&mut v, "").changed() {
                self.config.update.auto_check = v;
                self.config.save();
            }
        });
    }

    fn toggle_mute(&mut self) {
        if self.muted {
            self.muted = false;
            self.audio_gains.set_output(self.gain_before_mute);
        } else {
            self.gain_before_mute = self.config.audio.output_gain;
            self.muted = true;
            self.audio_gains.set_output(0.0);
        }
    }
}
