//! cpal capture/playback running on a dedicated OS thread.
//!
//! cpal's `Stream` is neither `Send` nor `Sync` on most platforms, so we
//! spawn a thread that owns the active streams. It listens on a control
//! channel for `Set{Input,Output}` commands and hot-swaps streams when
//! the user picks a different device in the UI; cpal itself runs its own
//! threads for the actual audio I/O, so this thread is idle most of the
//! time.
//!
//! Device selection: at startup we enumerate everything cpal sees and
//! hand the snapshot back via [`AudioHandle::devices`]. If a persisted
//! device name no longer matches anything enumerated (e.g. headset
//! unplugged between sessions), we silently fall back to the host's
//! default and log a warning — chosen for forgivingness per the user's
//! preference.
//!
//! Device-format handling: we'd like everyone at 48 kHz mono f32 (matching
//! the wire format), but that's not always supported — notably on Windows
//! / WASAPI in shared mode, the device only exposes the system's configured
//! format (often 44.1 kHz stereo, sometimes i16). So we *try* the preferred
//! config first and fall back to the device's native default if cpal
//! refuses, adapting on the fly:
//!
//!   - Multi-channel input is downmixed to mono by averaging channels.
//!   - Output is opened in stereo so the balance knob can pan our mono
//!     content between L/R (equal-power); on a 2-channel stream both
//!     channels get the sample scaled by the pan gains, on other
//!     channel counts it's simply replicated.
//!   - Non-f32 sample formats (i16, u16) are converted in the callback.
//!   - Sample-rate mismatch is resolved by an inline linear resampler at
//!     each boundary: capture → wire (48 kHz) before send, wire → device
//!     rate as the output callback drains. This keeps timing consistent
//!     across clients (a frame = 10 ms of real time regardless of who's
//!     running at 44.1 / 48 / 96 kHz natively), which is what prevents
//!     the periodic clicks you'd otherwise get when a Windows client
//!     (44.1) talks to a macOS client (48) — without resampling, frames
//!     arrive faster or slower than the receiver consumes them.

use std::collections::{HashMap, VecDeque};
use std::sync::atomic::{AtomicU32, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use anyhow::{anyhow, Context, Result};
use cpal::traits::{DeviceTrait, HostTrait, StreamTrait};
use cpal::{FromSample, SizedSample};
use tokio::sync::mpsc::{unbounded_channel, UnboundedReceiver, UnboundedSender};
use tracing::{info, warn};

use toki_proto::wire::{FRAME_SAMPLES, SAMPLE_RATE_HZ};

/// Reserved mixer slot for locally-generated, play-in-full audio (roger
/// beeps, tone previews). Server-assigned sender ids start at 1, so 0
/// never collides with a peer's voice stream.
pub const LOCAL_SENDER_ID: u32 = 0;

/// Evict a peer's drained jitter buffer after this long with no new
/// audio, so the per-sender map doesn't accumulate one entry per session
/// ever seen. Far longer than any inter-packet gap during speech.
const SENDER_IDLE_EVICT: Duration = Duration::from_secs(3);

/// One source's playback jitter buffer plus when it last received audio
/// (for idle eviction). Wire-rate (48 kHz) mono i16.
struct SenderBuf {
    buf: VecDeque<i16>,
    last: Instant,
}

/// Playback mixer: one jitter buffer per audio source (each peer's
/// `sender_id`, plus [`LOCAL_SENDER_ID`] for beeps), summed sample-for-
/// sample by the output callback. On a half-duplex channel there's only
/// ever one peer talking, so this degrades to a single buffer; on a
/// full-duplex channel concurrent talkers mix here.
#[derive(Default)]
pub struct Mixer {
    senders: HashMap<u32, SenderBuf>,
}

impl Mixer {
    /// Pop up to `max` mixed mono samples into `out`. Each position sums
    /// one sample from every source buffer (absent → 0), clipped to i16.
    /// Stops early once *every* buffer is dry (so the callback pads with
    /// silence rather than emitting zeros forever).
    fn mix_into(&mut self, out: &mut Vec<i16>, max: usize) {
        for _ in 0..max {
            let mut any = false;
            let mut acc: i32 = 0;
            for sb in self.senders.values_mut() {
                if let Some(s) = sb.buf.pop_front() {
                    acc += s as i32;
                    any = true;
                }
            }
            if !any {
                break;
            }
            out.push(acc.clamp(i16::MIN as i32, i16::MAX as i32) as i16);
        }
    }

    /// Test-only: a source's current jitter buffer, if present.
    #[cfg(test)]
    fn sender_buf(&self, id: u32) -> Option<&VecDeque<i16>> {
        self.senders.get(&id).map(|sb| &sb.buf)
    }
}

pub type PlaybackBuf = Arc<Mutex<Mixer>>;

pub struct AudioHandle {
    pub mic_rx: UnboundedReceiver<Vec<i16>>,
    pub playback: PlaybackBuf,
    /// Device names visible to cpal at startup. We don't auto-refresh —
    /// the user can restart the app if they hot-plug new hardware.
    pub devices: AudioDevices,
    /// Send `Set{Input,Output}` commands here to hot-swap the active
    /// streams. Cheap and `Clone`-able.
    pub control: AudioControl,
    /// Shared linear gains read by the audio callbacks every frame.
    /// Update via [`AudioGains::set_input`] / [`AudioGains::set_output`]
    /// — changes apply on the next callback (sub-10 ms typical), no
    /// stream restart needed.
    pub gains: AudioGains,
    /// Live peak levels published by the cpal callbacks (input on the
    /// capture side, output on the drain side). UI polls these to
    /// drive the waveform. Cheap to clone — same `Arc`-of-atomics
    /// pattern as `AudioGains`, opposite direction.
    pub levels: AudioLevels,
    /// Recent-sample rings for FFT-based spectrum rendering. UI
    /// snapshots the last N samples each frame, runs a windowed FFT
    /// off-thread (well, on the UI thread — see `tick_waveform`), and
    /// displays bin magnitudes.
    pub spectrum: AudioSpectrum,
}

/// Lock-free linear gain controls shared between the UI thread (writer)
/// and the cpal audio callbacks (readers). We use `AtomicU32` with
/// `f32::to_bits` so the callback path stays mutex-free — important
/// because cpal callbacks run on a real-time-priority thread that
/// must never block on the UI.
#[derive(Clone)]
pub struct AudioGains {
    input: Arc<AtomicU32>,
    output: Arc<AtomicU32>,
    /// Stereo balance for playback, in `[-1.0, 1.0]`: `-1` = full
    /// left, `0` = centered, `+1` = full right. Lets the operator
    /// route received audio entirely into one ear to mimic a mono
    /// walkie-talkie earpiece. Only meaningful on 2-channel output
    /// devices; ignored for mono / >2-channel outputs.
    balance: Arc<AtomicU32>,
}

impl AudioGains {
    fn new(input: f32, output: f32, balance: f32) -> Self {
        Self {
            input: Arc::new(AtomicU32::new(input.to_bits())),
            output: Arc::new(AtomicU32::new(output.to_bits())),
            balance: Arc::new(AtomicU32::new(balance.to_bits())),
        }
    }

    pub fn set_input(&self, gain: f32) {
        self.input.store(gain.to_bits(), Ordering::Relaxed);
    }

    pub fn set_output(&self, gain: f32) {
        self.output.store(gain.to_bits(), Ordering::Relaxed);
    }

    /// Set the playback balance. Clamped to `[-1.0, 1.0]`.
    pub fn set_balance(&self, balance: f32) {
        self.balance
            .store(balance.clamp(-1.0, 1.0).to_bits(), Ordering::Relaxed);
    }

    fn input_atomic(&self) -> Arc<AtomicU32> {
        self.input.clone()
    }

    fn output_atomic(&self) -> Arc<AtomicU32> {
        self.output.clone()
    }

    fn balance_atomic(&self) -> Arc<AtomicU32> {
        self.balance.clone()
    }
}

// ── Note pitch constants ──────────────────────────────────────────
// Equal-temperament A4 = 440 Hz. Defined so multi-note preset
// patterns read like sheet music instead of opaque float literals.
#[allow(dead_code)] // some pitches only used by future presets
const C5: f32 = 1046.50;
#[allow(dead_code)]
const G5: f32 = 1567.98;
#[allow(dead_code)]
const C6: f32 = 2093.00;

/// One step of a [`BeepPattern`]. Three flavours:
///
/// * [`BeepStep::Tone`] — a pure sine at a single frequency.
/// * [`BeepStep::Rest`] — silence for `duration_ms`. Rests let
///   presets carve rhythm out of a sequence (CB12's release motif
///   uses them for its "tap-tap…tap-tap" feel).
/// * [`BeepStep::Noise`] — bandpass-filtered white noise at
///   `center_hz` with `bandwidth_hz` of spread. Use for static /
///   hiss textures: a tight bandwidth reads as a pitched whoosh,
///   a wide one approaches plain broadband noise. Renderer applies
///   the same fade-in / fade-out it uses for tones so noise bursts
///   don't click at the seams.
///
/// All three carry their own `duration_ms` so a single accessor
/// works for [`BeepPattern::total_duration_ms`].
pub enum BeepStep {
    Tone {
        freq_hz: f32,
        duration_ms: u32,
    },
    Rest {
        duration_ms: u32,
    },
    // No preset uses Noise yet — silenced rather than gated so a new
    // preset can pick it up just by referencing `BeepStep::Noise { … }`.
    #[allow(dead_code)]
    Noise {
        center_hz: f32,
        bandwidth_hz: f32,
        duration_ms: u32,
    },
}

impl BeepStep {
    pub fn duration_ms(&self) -> u32 {
        match self {
            Self::Tone { duration_ms, .. }
            | Self::Rest { duration_ms }
            | Self::Noise { duration_ms, .. } => *duration_ms,
        }
    }
}

/// A sequence of tones (and optional rests) that together form a
/// single roger beep. Single-tone presets just have one step; richer
/// presets chain several to play a short melody. Total length is the
/// sum of every step's duration.
pub struct BeepPattern {
    pub steps: &'static [BeepStep],
}

impl BeepPattern {
    pub fn total_duration_ms(&self) -> u32 {
        self.steps.iter().map(|s| s.duration_ms()).sum()
    }
}

/// A named pair of [`BeepPattern`]s — one for the take-floor cue, one
/// for the clear-floor cue. Volume stays out of the preset because
/// it's a personal loudness preference; users should be able to trim
/// it without disturbing the preset they've picked.
///
/// To add a new preset, append a `BeepPreset { id, label, … }` entry
/// to [`BeepPreset::ALL`] with both patterns declared as `&[BeepStep]`
/// slices. The `id` is the stable key that lands in `config.toml` —
/// pick something short, lowercase, and stable across renames (the
/// user-facing string is `label`).
pub struct BeepPreset {
    /// Stable identifier persisted in the config file.
    pub id: &'static str,
    /// Human-readable label shown in the Settings dropdown.
    pub label: &'static str,
    pub acquire: BeepPattern,
    pub release: BeepPattern,
}

/// Fixed two-tone roger played fleet-wide when a *priority* speaker
/// takes the floor — whether by keying up on an idle channel or by
/// preempting a non-priority holder. Deliberately *not* part of the
/// tunable [`BeepPreset`] set: a priority cue is only useful if every
/// member recognizes it, so it stays constant network-wide (volume
/// still honours the user's beep-volume preference). A rising G5→C6
/// pair reads as urgent/attention without sounding like a normal
/// take-floor blip.
pub const PRIORITY_ROGER: &[BeepStep] = &[
    BeepStep::Tone {
        freq_hz: G5,
        duration_ms: 130,
    },
    BeepStep::Rest { duration_ms: 40 },
    BeepStep::Tone {
        freq_hz: C6,
        duration_ms: 170,
    },
];

/// Short descending cue heard *only* by the speaker who was bumped off
/// the floor by a priority preemption. Pairs with the "Preempted by
/// <name>" log line so the cut-off operator gets both an audible and a
/// visible signal. A C6→C5 drop reads as "you lost it".
pub const PREEMPTED_BUMP: &[BeepStep] = &[
    BeepStep::Tone {
        freq_hz: C6,
        duration_ms: 80,
    },
    BeepStep::Tone {
        freq_hz: C5,
        duration_ms: 150,
    },
];

impl BeepPreset {
    /// Master list of available presets. The first entry is treated
    /// as the fallback for unknown/legacy IDs.
    pub const ALL: &'static [BeepPreset] = &[
        BeepPreset {
            id: "default",
            label: "Default",
            acquire: BeepPattern {
                steps: &[
                    BeepStep::Tone {
                        freq_hz: 659.25,
                        duration_ms: 50,
                    },
                    BeepStep::Tone {
                        freq_hz: 523.25,
                        duration_ms: 50,
                    },
                    BeepStep::Tone {
                        freq_hz: 783.99,
                        duration_ms: 50,
                    },
                    BeepStep::Rest { duration_ms: 350 },
                ],
            },
            release: BeepPattern {
                steps: &[
                    BeepStep::Tone {
                        freq_hz: 783.99,
                        duration_ms: 100,
                    },
                    BeepStep::Tone {
                        freq_hz: 659.25,
                        duration_ms: 100,
                    },
                    BeepStep::Rest { duration_ms: 250 },
                ],
            },
        },
        BeepPreset {
            id: "default_with_noise",
            label: "Default with Noise",
            acquire: BeepPattern {
                steps: &[
                    BeepStep::Tone {
                        freq_hz: 659.25,
                        duration_ms: 50,
                    },
                    BeepStep::Tone {
                        freq_hz: 523.25,
                        duration_ms: 50,
                    },
                    BeepStep::Tone {
                        freq_hz: 783.99,
                        duration_ms: 50,
                    },
                    BeepStep::Rest { duration_ms: 350 },
                ],
            },
            release: BeepPattern {
                steps: &[BeepStep::Noise {
                    center_hz: 1000.0,
                    bandwidth_hz: 2000.0,
                    duration_ms: 150,
                }],
            },
        },
        // CB12 — five-step "Morse-ish" cues at 50 ms per step.
        //
        // TAKEN  : G5 · C5 · G5 · C5 · G5  (alternating major-fifth bounce)
        // CLEARED: C6 · ·  · C6 · C6 · ·   (three-tap with a hole at slot 2 & 5)
        //
        // Both add up to 250 ms — about twice as long as the default
        // tone but still well within "short feedback chirp" territory.
        BeepPreset {
            id: "cb12",
            label: "CB12",
            acquire: BeepPattern {
                steps: &[
                    BeepStep::Tone {
                        freq_hz: G5,
                        duration_ms: 120,
                    },
                    BeepStep::Tone {
                        freq_hz: C5,
                        duration_ms: 120,
                    },
                    BeepStep::Tone {
                        freq_hz: G5,
                        duration_ms: 120,
                    },
                    BeepStep::Tone {
                        freq_hz: C5,
                        duration_ms: 120,
                    },
                    BeepStep::Tone {
                        freq_hz: G5,
                        duration_ms: 120,
                    },
                ],
            },
            release: BeepPattern {
                steps: &[
                    BeepStep::Tone {
                        freq_hz: C6,
                        duration_ms: 120,
                    },
                    BeepStep::Rest { duration_ms: 120 },
                    BeepStep::Tone {
                        freq_hz: C6,
                        duration_ms: 120,
                    },
                    BeepStep::Tone {
                        freq_hz: C6,
                        duration_ms: 120,
                    },
                    BeepStep::Rest { duration_ms: 120 },
                ],
            },
        },
    ];

    /// Resolve a config-file ID to a preset, falling back to the
    /// first entry (`Default`) on miss so an unknown name doesn't
    /// brick the app's audio cues.
    pub fn by_id(id: &str) -> &'static BeepPreset {
        Self::ALL
            .iter()
            .find(|p| p.id == id)
            .unwrap_or(&Self::ALL[0])
    }

    /// Index into [`BeepPreset::ALL`] for a given id — needed for
    /// the atomic-index live-lookup used by [`BeepParams`]. Unknown
    /// ids land on index 0 (Default).
    pub fn index_of(id: &str) -> usize {
        Self::ALL.iter().position(|p| p.id == id).unwrap_or(0)
    }
}

/// Live "roger beep" parameters. The runtime reads from these
/// lock-free on every take-floor / clear-floor broadcast; the UI
/// writes when the user changes the preset or volume.
///
/// Tone choice is stored as an *index* into [`BeepPreset::ALL`] (a
/// single `AtomicUsize` load), not as the Hz/duration values
/// themselves — multi-step presets like CB12 have several notes per
/// pattern and we'd otherwise need atomic plumbing for an unbounded
/// sequence. The static table is `'static` data, so resolving the
/// index gives us a `&'static BeepPreset` with no allocation.
///
/// `Clone` because both fields are `Arc<…>` — cloning bumps refcounts
/// so the runtime + UI can hold independent handles pointing at the
/// same atomics.
#[derive(Clone)]
pub struct BeepParams {
    preset_index: Arc<AtomicUsize>,
    volume: Arc<AtomicU32>,
}

impl BeepParams {
    pub fn new(preset_index: usize, volume: f32) -> Self {
        Self {
            preset_index: Arc::new(AtomicUsize::new(preset_index)),
            volume: Arc::new(AtomicU32::new(volume.to_bits())),
        }
    }

    /// Resolved preset for the current `preset_index`. Out-of-range
    /// indices clamp to the first entry (defensive — should never
    /// happen since the only writer is the dropdown which writes a
    /// value it just iterated over).
    pub fn current_preset(&self) -> &'static BeepPreset {
        let i = self.preset_index.load(Ordering::Relaxed);
        BeepPreset::ALL.get(i).unwrap_or(&BeepPreset::ALL[0])
    }

    pub fn volume(&self) -> f32 {
        f32::from_bits(self.volume.load(Ordering::Relaxed))
    }

    pub fn set_preset_index(&self, i: usize) {
        self.preset_index.store(i, Ordering::Relaxed);
    }

    pub fn set_volume(&self, v: f32) {
        self.volume.store(v.to_bits(), Ordering::Relaxed);
    }
}

/// Lock-free peak meters fed by the cpal callbacks. `[0.0, 1.0]`
/// each — `1.0` means "this callback hit full-scale on at least one
/// sample". Smoothed with fast-attack / slow-decay inside the
/// callback so successive UI reads see a coherent envelope rather
/// than per-callback jitter.
#[derive(Clone)]
pub struct AudioLevels {
    input: Arc<AtomicU32>,
    output: Arc<AtomicU32>,
}

impl AudioLevels {
    fn new() -> Self {
        Self {
            input: Arc::new(AtomicU32::new(0)),
            output: Arc::new(AtomicU32::new(0)),
        }
    }

    /// Mic-side peak in `[0.0, 1.0]`. Currently unused at the UI
    /// surface (the histogram reads the spectrum ring directly), but
    /// kept around for future affordances like a Settings VU meter.
    #[allow(dead_code)]
    pub fn input(&self) -> f32 {
        f32::from_bits(self.input.load(Ordering::Relaxed))
    }

    /// Playback-side peak in `[0.0, 1.0]`. Same future-use story as
    /// `input` above.
    #[allow(dead_code)]
    pub fn output(&self) -> f32 {
        f32::from_bits(self.output.load(Ordering::Relaxed))
    }

    fn input_atomic(&self) -> Arc<AtomicU32> {
        self.input.clone()
    }

    fn output_atomic(&self) -> Arc<AtomicU32> {
        self.output.clone()
    }
}

/// Ring buffer of the most recent `f32` samples for both directions.
/// The capacity is sized to comfortably exceed our FFT window
/// (256 samples) plus a frame or two of jitter — anything older is
/// dropped on the next push.
pub const SPECTRUM_BUF_LEN: usize = 1024;

/// Shared sample rings: the cpal callbacks push the latest mono
/// samples after gain is applied; the UI thread snapshots the tail
/// each frame to feed an FFT. A `Mutex<VecDeque>` is fine here — the
/// snapshot copy is a few thousand `f32`s, well under a millisecond
/// even on the audio thread, and contention is rare (one writer, one
/// reader, opposite cadences).
#[derive(Clone)]
pub struct AudioSpectrum {
    input: Arc<Mutex<VecDeque<f32>>>,
    output: Arc<Mutex<VecDeque<f32>>>,
}

impl AudioSpectrum {
    fn new() -> Self {
        Self {
            input: Arc::new(Mutex::new(VecDeque::with_capacity(SPECTRUM_BUF_LEN))),
            output: Arc::new(Mutex::new(VecDeque::with_capacity(SPECTRUM_BUF_LEN))),
        }
    }

    /// Copy the last `n` samples of the input ring into `out`. Clears
    /// `out` first. If the ring holds fewer than `n` samples the
    /// short prefix is returned — callers can check `out.len()`.
    pub fn snapshot_input(&self, out: &mut Vec<f32>, n: usize) {
        let buf = self.input.lock().unwrap();
        snapshot_tail(&buf, n, out);
    }

    pub fn snapshot_output(&self, out: &mut Vec<f32>, n: usize) {
        let buf = self.output.lock().unwrap();
        snapshot_tail(&buf, n, out);
    }

    fn input_buf(&self) -> Arc<Mutex<VecDeque<f32>>> {
        self.input.clone()
    }

    fn output_buf(&self) -> Arc<Mutex<VecDeque<f32>>> {
        self.output.clone()
    }
}

fn snapshot_tail(buf: &VecDeque<f32>, n: usize, out: &mut Vec<f32>) {
    out.clear();
    let start = buf.len().saturating_sub(n);
    out.extend(buf.iter().skip(start).copied());
}

/// Push samples into a spectrum ring, dropping the oldest beyond
/// `SPECTRUM_BUF_LEN`. Called from inside the cpal callback so this
/// must stay cheap — bulk-extend then drain at most a frame's worth
/// from the front, no per-sample lock thrashing.
fn push_spectrum(buf: &Mutex<VecDeque<f32>>, samples: &[f32]) {
    if samples.is_empty() {
        return;
    }
    let mut b = buf.lock().unwrap();
    // Drop overflow up front so the extend doesn't reallocate past
    // the buffer's preallocated capacity.
    let total = b.len() + samples.len();
    if total > SPECTRUM_BUF_LEN {
        let drop_n = total - SPECTRUM_BUF_LEN;
        for _ in 0..drop_n {
            b.pop_front();
        }
    }
    b.extend(samples.iter().copied());
}

/// Blend `peak` into the atomic with fast-attack, slow-decay smoothing.
/// Called from inside the cpal callbacks so it lives on the audio
/// thread. The 0.85 decay coefficient gives roughly a 60 ms half-life
/// at ~100 callbacks/sec — visually smooth without lagging behind
/// the actual signal.
fn blend_level(atomic: &AtomicU32, peak: f32) {
    let prev = f32::from_bits(atomic.load(Ordering::Relaxed));
    // Snap up on a louder reading, ease down otherwise.
    let next = if peak >= prev {
        peak
    } else {
        prev * 0.85 + peak * 0.15
    };
    atomic.store(next.to_bits(), Ordering::Relaxed);
}

#[derive(Clone, Debug, Default)]
pub struct AudioDevices {
    pub inputs: Vec<String>,
    pub outputs: Vec<String>,
}

#[derive(Clone)]
pub struct AudioControl {
    tx: std::sync::mpsc::Sender<AudioCmd>,
}

impl AudioControl {
    /// Switch the active input device. `None` = use the host's default.
    /// If the named device isn't found, we silently fall back to default.
    pub fn set_input(&self, name: Option<String>) {
        let _ = self.tx.send(AudioCmd::SetInput(name));
    }

    /// Switch the active output device. `None` = use the host's default.
    pub fn set_output(&self, name: Option<String>) {
        let _ = self.tx.send(AudioCmd::SetOutput(name));
    }
}

enum AudioCmd {
    SetInput(Option<String>),
    SetOutput(Option<String>),
}

/// Spawn the audio thread and open initial streams using the supplied
/// device preferences (`None` = host default for either). Returns once
/// the thread has enumerated devices and attempted to open both streams.
pub fn spawn(
    initial_input: Option<String>,
    initial_output: Option<String>,
    initial_input_gain: f32,
    initial_output_gain: f32,
    initial_balance: f32,
) -> Result<AudioHandle> {
    let (mic_tx, mic_rx) = unbounded_channel::<Vec<i16>>();
    // Per-source playback mixer (one jitter buffer per talker + beeps).
    let playback: PlaybackBuf = Arc::new(Mutex::new(Mixer::default()));
    let (cmd_tx, cmd_rx) = std::sync::mpsc::channel::<AudioCmd>();

    let (init_tx, init_rx) = std::sync::mpsc::channel::<AudioDevices>();

    let gains = AudioGains::new(initial_input_gain, initial_output_gain, initial_balance);
    let levels = AudioLevels::new();
    let spectrum = AudioSpectrum::new();

    let mic_tx_for_thread = mic_tx;
    let playback_for_thread = playback.clone();
    let in_gain_for_thread = gains.input_atomic();
    let out_gain_for_thread = gains.output_atomic();
    let out_balance_for_thread = gains.balance_atomic();
    let in_level_for_thread = levels.input_atomic();
    let out_level_for_thread = levels.output_atomic();
    let in_spec_for_thread = spectrum.input_buf();
    let out_spec_for_thread = spectrum.output_buf();

    std::thread::Builder::new()
        .name("toki-audio".into())
        .spawn(move || {
            let host = cpal::default_host();
            let _ = init_tx.send(enumerate(&host));

            let mut input_stream = open_input_for(
                &host,
                initial_input.as_deref(),
                &mic_tx_for_thread,
                &in_gain_for_thread,
                &in_level_for_thread,
                &in_spec_for_thread,
            );
            let mut output_stream = open_output_for(
                &host,
                initial_output.as_deref(),
                &playback_for_thread,
                &out_gain_for_thread,
                &out_balance_for_thread,
                &out_level_for_thread,
                &out_spec_for_thread,
            );

            // cpal streams run on their own threads; this loop is idle
            // most of the time, only waking up to hot-swap on user
            // selection. When the control sender drops (app shutdown),
            // recv() errors and we fall through to drop the streams.
            loop {
                match cmd_rx.recv() {
                    Ok(AudioCmd::SetInput(name)) => {
                        // Drop the old stream *before* opening the new
                        // one — WASAPI is happier with one stream per
                        // device class at a time.
                        drop(input_stream.take());
                        input_stream = open_input_for(
                            &host,
                            name.as_deref(),
                            &mic_tx_for_thread,
                            &in_gain_for_thread,
                            &in_level_for_thread,
                            &in_spec_for_thread,
                        );
                    }
                    Ok(AudioCmd::SetOutput(name)) => {
                        drop(output_stream.take());
                        output_stream = open_output_for(
                            &host,
                            name.as_deref(),
                            &playback_for_thread,
                            &out_gain_for_thread,
                            &out_balance_for_thread,
                            &out_level_for_thread,
                            &out_spec_for_thread,
                        );
                    }
                    Err(_) => break,
                }
            }
            drop(input_stream);
            drop(output_stream);
        })?;

    let devices = init_rx
        .recv()
        .map_err(|_| anyhow!("audio thread died before initial enumeration"))?;

    Ok(AudioHandle {
        mic_rx,
        playback,
        devices,
        control: AudioControl { tx: cmd_tx },
        gains,
        levels,
        spectrum,
    })
}

fn enumerate(host: &cpal::Host) -> AudioDevices {
    let inputs = host
        .input_devices()
        .map(|it| it.filter_map(|d| d.name().ok()).collect())
        .unwrap_or_default();
    let outputs = host
        .output_devices()
        .map(|it| it.filter_map(|d| d.name().ok()).collect())
        .unwrap_or_default();
    AudioDevices { inputs, outputs }
}

/// Pick the input device with the given name, falling back silently to
/// the host's default if the name is `None` or doesn't match anything.
fn pick_input(host: &cpal::Host, name: Option<&str>) -> Option<cpal::Device> {
    if let Some(want) = name {
        if let Ok(iter) = host.input_devices() {
            if let Some(d) = iter
                .into_iter()
                .find(|d| d.name().ok().as_deref() == Some(want))
            {
                return Some(d);
            }
        }
        warn!(name = %want, "saved input device not found, falling back to host default");
    }
    host.default_input_device()
}

fn pick_output(host: &cpal::Host, name: Option<&str>) -> Option<cpal::Device> {
    if let Some(want) = name {
        if let Ok(iter) = host.output_devices() {
            if let Some(d) = iter
                .into_iter()
                .find(|d| d.name().ok().as_deref() == Some(want))
            {
                return Some(d);
            }
        }
        warn!(name = %want, "saved output device not found, falling back to host default");
    }
    host.default_output_device()
}

#[allow(clippy::too_many_arguments)]
fn open_input_for(
    host: &cpal::Host,
    name: Option<&str>,
    mic_tx: &UnboundedSender<Vec<i16>>,
    gain: &Arc<AtomicU32>,
    level: &Arc<AtomicU32>,
    spectrum: &Arc<Mutex<VecDeque<f32>>>,
) -> Option<cpal::Stream> {
    let device = pick_input(host, name)?;
    let device_name = device.name().unwrap_or_else(|_| "?".into());
    let stream = match open_input(
        &device,
        mic_tx.clone(),
        gain.clone(),
        level.clone(),
        spectrum.clone(),
    ) {
        Ok(s) => s,
        Err(e) => {
            warn!(device = %device_name, error = %e, "failed to open input stream");
            return None;
        }
    };
    if let Err(e) = stream.play() {
        warn!(device = %device_name, error = %e, "failed to start input stream");
        return None;
    }
    info!(device = %device_name, "input stream live");
    Some(stream)
}

#[allow(clippy::too_many_arguments)]
fn open_output_for(
    host: &cpal::Host,
    name: Option<&str>,
    playback: &PlaybackBuf,
    gain: &Arc<AtomicU32>,
    balance: &Arc<AtomicU32>,
    level: &Arc<AtomicU32>,
    spectrum: &Arc<Mutex<VecDeque<f32>>>,
) -> Option<cpal::Stream> {
    let device = pick_output(host, name)?;
    let device_name = device.name().unwrap_or_else(|_| "?".into());
    let stream = match open_output(
        &device,
        playback.clone(),
        gain.clone(),
        balance.clone(),
        level.clone(),
        spectrum.clone(),
    ) {
        Ok(s) => s,
        Err(e) => {
            warn!(device = %device_name, error = %e, "failed to open output stream");
            return None;
        }
    };
    if let Err(e) = stream.play() {
        warn!(device = %device_name, error = %e, "failed to start output stream");
        return None;
    }
    info!(device = %device_name, "output stream live");
    Some(stream)
}

// Per-OS preferred capture/playback rate.
//
// On Windows, WASAPI shared mode only exposes whichever format is set in
// Sound Settings — almost always 44.1 kHz. Asking for 48 kHz there hits
// the device-default fallback, which downmixes 2 channels per callback,
// runs at the wrong rate vs the wire constant, and produces the
// laggy-and-high-pitched behavior we saw. Asking for 44.1 kHz up front
// matches the native device, avoids the downmix, and keeps the audio
// pipeline on the fast path.
//
// Caveat: the wire format is still nominally 48 kHz. When a Windows
// client (44.1 kHz native) talks to a macOS/Linux client (48 kHz
// native), the receiver plays the sender's PCM at its own device rate,
// producing a ~9% pitch shift. Fixing this properly needs a resampler
// (e.g. `rubato`) on each end — out of scope for this change.
#[cfg(target_os = "windows")]
const PREFERRED_RATE: u32 = 44_100;
#[cfg(not(target_os = "windows"))]
const PREFERRED_RATE: u32 = SAMPLE_RATE_HZ;

const PREFERRED: cpal::StreamConfig = cpal::StreamConfig {
    channels: 1,
    sample_rate: cpal::SampleRate(PREFERRED_RATE),
    buffer_size: cpal::BufferSize::Default,
};

/// Output is opened in **stereo** (not mono like the capture side) so
/// the balance knob has two channels to pan between. With a 1-channel
/// stream the OS up-mixes our single channel into both ears and we
/// lose all L/R control — exactly the "balance does nothing" symptom.
/// Our content is mono; the output callback writes the same sample to
/// L and R (scaled by the equal-power pan). If the device rejects this
/// config we fall back to its native default, which is virtually
/// always ≥2-channel for speakers/headphones anyway.
const PREFERRED_OUTPUT: cpal::StreamConfig = cpal::StreamConfig {
    channels: 2,
    sample_rate: cpal::SampleRate(PREFERRED_RATE),
    buffer_size: cpal::BufferSize::Default,
};

fn open_input(
    dev: &cpal::Device,
    mic_tx: UnboundedSender<Vec<i16>>,
    gain: Arc<AtomicU32>,
    level: Arc<AtomicU32>,
    spectrum: Arc<Mutex<VecDeque<f32>>>,
) -> Result<cpal::Stream> {
    // First try our platform-preferred mono f32 config.
    match build_input_stream::<f32>(
        dev,
        &PREFERRED,
        1,
        mic_tx.clone(),
        gain.clone(),
        level.clone(),
        spectrum.clone(),
    ) {
        Ok(s) => {
            info!("input: 1 ch @ {PREFERRED_RATE} Hz f32 (preferred)");
            return Ok(s);
        }
        Err(e) => {
            info!(error = %e, "preferred input config rejected, querying device default");
        }
    }

    let supported = dev
        .default_input_config()
        .context("query default input config")?;
    let channels = supported.channels();
    let rate = supported.sample_rate().0;
    let format = supported.sample_format();
    let cfg = supported.config();
    warn!("input: using device default {channels} ch @ {rate} Hz {format:?}");
    // Rate mismatch is fine — the stream builder resamples to wire rate
    // (48 kHz) before sending, so cross-OS clients see consistent timing.

    match format {
        cpal::SampleFormat::F32 => {
            build_input_stream::<f32>(dev, &cfg, channels, mic_tx, gain, level, spectrum)
        }
        cpal::SampleFormat::I16 => {
            build_input_stream::<i16>(dev, &cfg, channels, mic_tx, gain, level, spectrum)
        }
        cpal::SampleFormat::U16 => {
            build_input_stream::<u16>(dev, &cfg, channels, mic_tx, gain, level, spectrum)
        }
        other => Err(anyhow!("unsupported input sample format: {other:?}")),
    }
}

fn open_output(
    dev: &cpal::Device,
    playback: PlaybackBuf,
    gain: Arc<AtomicU32>,
    balance: Arc<AtomicU32>,
    level: Arc<AtomicU32>,
    spectrum: Arc<Mutex<VecDeque<f32>>>,
) -> Result<cpal::Stream> {
    match build_output_stream::<f32>(
        dev,
        &PREFERRED_OUTPUT,
        2,
        playback.clone(),
        gain.clone(),
        balance.clone(),
        level.clone(),
        spectrum.clone(),
    ) {
        Ok(s) => {
            info!("output: 2 ch @ {PREFERRED_RATE} Hz f32 (preferred)");
            return Ok(s);
        }
        Err(e) => {
            info!(error = %e, "preferred output config rejected, querying device default");
        }
    }

    let supported = dev
        .default_output_config()
        .context("query default output config")?;
    let channels = supported.channels();
    let rate = supported.sample_rate().0;
    let format = supported.sample_format();
    let cfg = supported.config();
    warn!("output: using device default {channels} ch @ {rate} Hz {format:?}");
    // Rate mismatch is fine — the stream builder resamples from wire
    // rate (48 kHz) to the device rate on the fly.

    match format {
        cpal::SampleFormat::F32 => build_output_stream::<f32>(
            dev, &cfg, channels, playback, gain, balance, level, spectrum,
        ),
        cpal::SampleFormat::I16 => build_output_stream::<i16>(
            dev, &cfg, channels, playback, gain, balance, level, spectrum,
        ),
        cpal::SampleFormat::U16 => build_output_stream::<u16>(
            dev, &cfg, channels, playback, gain, balance, level, spectrum,
        ),
        other => Err(anyhow!("unsupported output sample format: {other:?}")),
    }
}

/// Tiny stateful linear resampler. Used at the boundary between cpal's
/// native device rate and the wire's canonical 48 kHz so cross-OS calls
/// stay timing-synchronized: a frame represents 10 ms of real time on
/// both ends, regardless of who's at 44.1 / 48 / 96 kHz natively.
///
/// Linear interpolation is plenty for voice — far cheaper than a proper
/// FIR resampler and the audible difference is well below the noise
/// floor of a typical microphone. We carry `last` across calls so
/// interpolation across chunk boundaries doesn't click.
struct LinearResampler {
    /// `input_rate / output_rate`. Each output sample advances this many
    /// positions in the input stream.
    step: f64,
    /// Position of the next output sample in input-index space. Carried
    /// across calls — when this exceeds the input length we shift it
    /// relative to the *next* chunk (subtract the current chunk length).
    pos: f64,
    /// Last input sample from the previous chunk, used as the "left"
    /// sample when `pos < 0` on the next call.
    last: i16,
}

impl LinearResampler {
    fn new(in_rate: u32, out_rate: u32) -> Self {
        Self {
            step: in_rate as f64 / out_rate as f64,
            pos: 0.0,
            last: 0,
        }
    }

    fn pass_through(&self) -> bool {
        (self.step - 1.0).abs() < 1e-9
    }

    /// Append resampled output to `out`. Does NOT clear `out` — caller
    /// chooses whether to reuse the buffer. Empty input is a no-op.
    fn process(&mut self, input: &[i16], out: &mut Vec<i16>) {
        if input.is_empty() {
            return;
        }
        if self.pass_through() {
            out.extend_from_slice(input);
            self.last = *input.last().unwrap();
            return;
        }
        // We stop one short of `input.len() - 1` so we always have both
        // `input[idx]` and `input[idx+1]` available. The fractional
        // overflow past the end is carried to the next call via `pos`.
        let mut pos = self.pos;
        let max_pos = (input.len() - 1) as f64;
        while pos < max_pos {
            let idx_f = pos.floor();
            let frac = pos - idx_f;
            let idx = idx_f as isize;
            let s0 = if idx < 0 {
                self.last as f64
            } else {
                input[idx as usize] as f64
            };
            let s1 = input[(idx + 1) as usize] as f64;
            let interp = s0 + (s1 - s0) * frac;
            out.push(interp.clamp(i16::MIN as f64, i16::MAX as f64) as i16);
            pos += self.step;
        }
        self.last = *input.last().unwrap();
        // Shift pos relative to the *next* chunk's first sample.
        // Result lies in `[max_pos - input.len(), step)` ≈ `[-1, step)`.
        self.pos = pos - input.len() as f64;
    }
}

#[allow(clippy::too_many_arguments)]
fn build_input_stream<T>(
    dev: &cpal::Device,
    cfg: &cpal::StreamConfig,
    channels: u16,
    mic_tx: UnboundedSender<Vec<i16>>,
    gain: Arc<AtomicU32>,
    level: Arc<AtomicU32>,
    spectrum: Arc<Mutex<VecDeque<f32>>>,
) -> Result<cpal::Stream>
where
    T: SizedSample + Send + 'static,
    f32: FromSample<T>,
{
    let ch = channels as usize;
    let dev_rate = cfg.sample_rate.0;
    // Resample capture → wire rate (always 48 kHz) so frames sent over
    // UDP carry a consistent 10 ms of real time. Without this, a 44.1 kHz
    // capture would accumulate 480 samples in 10.88 ms — and a 48 kHz
    // peer would drain them in 10 ms, causing periodic clicks.
    let mut resampler = LinearResampler::new(dev_rate, SAMPLE_RATE_HZ);
    let mut accum: Vec<i16> = Vec::with_capacity(FRAME_SAMPLES);
    let mut downmixed: Vec<i16> = Vec::with_capacity(2048);
    let mut wire: Vec<i16> = Vec::with_capacity(2048);
    // Parallel `f32` buffer holding the post-gain mono samples — fed
    // to the spectrum ring so the UI doesn't have to re-convert i16
    // → f32 just to FFT.
    let mut spec_samples: Vec<f32> = Vec::with_capacity(2048);
    let stream = dev.build_input_stream(
        cfg,
        move |data: &[T], _: &cpal::InputCallbackInfo| {
            // Read the current gain once per callback — it's stable
            // across the ~10 ms callback duration and reading every
            // sample is unnecessary overhead.
            let g = f32::from_bits(gain.load(Ordering::Relaxed));
            // Interleaved samples; downmix N channels to mono by averaging.
            // While we're at it, track the |max| sample so the UI can
            // render a real mic-level waveform during TX, AND copy
            // the f32 amplitude into the spectrum ring for the
            // histogram view.
            downmixed.clear();
            spec_samples.clear();
            let mut peak: f32 = 0.0;
            for frame in data.chunks_exact(ch) {
                let sum: f32 = frame.iter().map(|&s| s.to_sample::<f32>()).sum();
                let avg = (sum / ch as f32) * g;
                let clamped = avg.clamp(-1.0, 1.0);
                let abs = clamped.abs();
                if abs > peak {
                    peak = abs;
                }
                let v = (clamped * i16::MAX as f32) as i16;
                downmixed.push(v);
                spec_samples.push(clamped);
            }
            blend_level(&level, peak);
            push_spectrum(&spectrum, &spec_samples);
            // Resample to wire rate, then chunk into 480-sample frames.
            wire.clear();
            resampler.process(&downmixed, &mut wire);
            for &v in &wire {
                accum.push(v);
                if accum.len() >= FRAME_SAMPLES {
                    let frame = std::mem::replace(&mut accum, Vec::with_capacity(FRAME_SAMPLES));
                    let _ = mic_tx.send(frame);
                }
            }
        },
        |e| warn!(error = %e, "input stream error"),
        None,
    )?;
    Ok(stream)
}

#[allow(clippy::too_many_arguments)]
#[allow(clippy::too_many_arguments)]
fn build_output_stream<T>(
    dev: &cpal::Device,
    cfg: &cpal::StreamConfig,
    channels: u16,
    playback: PlaybackBuf,
    gain: Arc<AtomicU32>,
    balance: Arc<AtomicU32>,
    level: Arc<AtomicU32>,
    spectrum: Arc<Mutex<VecDeque<f32>>>,
) -> Result<cpal::Stream>
where
    T: SizedSample + FromSample<f32> + Send + 'static,
{
    let ch = channels as usize;
    let dev_rate = cfg.sample_rate.0;
    // The playback ring stores wire-rate (48 kHz) samples; we resample
    // to the device's native rate as we drain. Mirror of the input
    // path — keeps the ring's 10-ms-per-480-samples timing intact and
    // device-rate concerns isolated to the callback.
    let mut resampler = LinearResampler::new(SAMPLE_RATE_HZ, dev_rate);
    let mut wire_chunk: Vec<i16> = Vec::with_capacity(512);
    let mut resampled: Vec<i16> = Vec::with_capacity(1024);
    let mut ready: std::collections::VecDeque<i16> =
        std::collections::VecDeque::with_capacity(4096);
    // Hoisted so we don't reallocate inside the audio-thread callback
    // every ~10 ms. Same trick the input builder uses.
    let mut spec_samples: Vec<f32> = Vec::with_capacity(2048);
    let stream = dev.build_output_stream(
        cfg,
        move |data: &mut [T], _: &cpal::OutputCallbackInfo| {
            let g = f32::from_bits(gain.load(Ordering::Relaxed));
            // Equal-power stereo pan from balance ∈ [-1, 1]. At center
            // both channels get ~0.707 (so perceived loudness stays
            // constant across the sweep); at the extremes one channel
            // reaches unity and the other silence — a full mono-earpiece
            // pan. Only applied on 2-channel devices; mono / >2-ch
            // outputs replicate the sample as before (`bal` unused).
            let bal = f32::from_bits(balance.load(Ordering::Relaxed)).clamp(-1.0, 1.0);
            let pan_angle = (bal + 1.0) * (std::f32::consts::FRAC_PI_4); // 0..π/2
            let (l_gain, r_gain) = (pan_angle.cos(), pan_angle.sin());
            let stereo = ch == 2;
            let frames_needed = data.len() / ch;

            // Refill `ready` from the wire-rate playback ring, resampling
            // as we go. We pull chunks rather than one sample at a time
            // so the resampler's per-call fixed costs amortize well.
            while ready.len() < frames_needed {
                wire_chunk.clear();
                {
                    // Mix one chunk across all active sources (peers +
                    // local beeps). On half-duplex this is a single buffer.
                    let mut mixer = playback.lock().unwrap();
                    mixer.mix_into(&mut wire_chunk, 256);
                }
                if wire_chunk.is_empty() {
                    break; // all sources dry, callback will pad with silence
                }
                resampled.clear();
                resampler.process(&wire_chunk, &mut resampled);
                ready.extend(resampled.iter().copied());
            }

            // Pop one mono sample, replicate it across N output channels.
            // Track the |max| f32 amplitude we wrote so the UI can
            // render a real waveform during RX. Empty pops contribute
            // 0 to the peak, so silence decays naturally via
            // `blend_level`. Same loop also fills the spectrum ring
            // with the post-gain mono `f32`s.
            let mut peak: f32 = 0.0;
            spec_samples.clear();
            for frame in data.chunks_mut(ch) {
                let mono = ready.pop_front().unwrap_or(0);
                let f = ((mono as f32 / i16::MAX as f32) * g).clamp(-1.0, 1.0);
                let abs = f.abs();
                if abs > peak {
                    peak = abs;
                }
                spec_samples.push(f);
                if stereo {
                    // Channel 0 = left, 1 = right. Balance only touches
                    // the stereo case; the meter/spectrum keep tracking
                    // the pre-pan mono level so they read the same
                    // regardless of where the audio is panned.
                    frame[0] = T::from_sample(f * l_gain);
                    frame[1] = T::from_sample(f * r_gain);
                } else {
                    let s: T = T::from_sample(f);
                    for slot in frame.iter_mut() {
                        *slot = s;
                    }
                }
            }
            blend_level(&level, peak);
            push_spectrum(&spectrum, &spec_samples);
        },
        |e| warn!(error = %e, "output stream error"),
        None,
    )?;
    Ok(stream)
}

/// Append wire-rate (48 kHz) PCM into the playback ring. Caps the queue
/// at 500 ms to prevent latency from snowballing if we receive faster
/// than the speaker drains. Used for self-generated audio that must play
/// out in full (roger beeps, tone previews) — see [`push_voice`] for the
/// latency-managed path used by incoming voice.
pub fn push_playback(buf: &PlaybackBuf, samples: &[i16]) {
    let mut guard = buf.lock().unwrap();
    let now = Instant::now();
    let slot = guard
        .senders
        .entry(LOCAL_SENDER_ID)
        .or_insert_with(|| SenderBuf {
            buf: VecDeque::with_capacity(SAMPLE_RATE_HZ as usize / 2),
            last: now,
        });
    slot.last = now;
    // 500 ms at wire rate.
    let cap = (SAMPLE_RATE_HZ / 2) as usize;
    for &s in samples {
        if slot.buf.len() >= cap {
            slot.buf.pop_front();
        }
        slot.buf.push_back(s);
    }
}

/// Soft target backlog for the incoming-voice path: ~60 ms of jitter
/// cushion at wire rate. After an overflow we trim back to this rather
/// than leaving the queue deep.
const VOICE_TARGET_SAMPLES: usize = (SAMPLE_RATE_HZ as usize * 60) / 1000;
/// Hard ceiling for the voice backlog: ~120 ms. Beyond this, mouth-to-ear
/// latency is worse than the glitch from skipping ahead, so we catch up.
const VOICE_MAX_SAMPLES: usize = (SAMPLE_RATE_HZ as usize * 120) / 1000;

/// Append incoming voice into the playback ring with active latency
/// management. In steady state (matched capture/playback clocks, low
/// jitter) the backlog stays naturally small. But a scheduling hiccup or
/// a clump of UDP packets can leave the queue deep — and the plain
/// [`push_playback`] cap (500 ms) would let that latency *persist* for
/// the rest of the transmission, since nothing trims it back down. That's
/// the "delay" you hear once playback falls behind.
///
/// Here, when the backlog runs past [`VOICE_MAX_SAMPLES`] we drop the
/// oldest samples to snap back to [`VOICE_TARGET_SAMPLES`] — a one-time
/// catch-up skip that keeps voice tight rather than carrying a growing
/// delay. For half-duplex push-to-talk this is the right trade: a brief
/// skip during a hiccup beats a permanently laggy channel.
/// `sender_id` is the per-session routing id from the S2C header — each
/// talker gets their own jitter buffer so concurrent full-duplex streams
/// stay independent (and Opus decoder state never crosses senders). The
/// 60/120 ms catch-up is applied per buffer.
pub fn push_voice(buf: &PlaybackBuf, sender_id: u32, samples: &[i16]) {
    let mut guard = buf.lock().unwrap();
    let now = Instant::now();
    // Opportunistically drop drained buffers from talkers who've gone
    // quiet, so the map doesn't grow one entry per session ever heard.
    guard.senders.retain(|&id, sb| {
        id == sender_id
            || id == LOCAL_SENDER_ID
            || !sb.buf.is_empty()
            || now.duration_since(sb.last) < SENDER_IDLE_EVICT
    });
    let slot = guard.senders.entry(sender_id).or_insert_with(|| SenderBuf {
        buf: VecDeque::with_capacity(VOICE_MAX_SAMPLES),
        last: now,
    });
    slot.last = now;
    slot.buf.extend(samples.iter().copied());
    if slot.buf.len() > VOICE_MAX_SAMPLES {
        let drop = slot.buf.len() - VOICE_TARGET_SAMPLES;
        slot.buf.drain(..drop);
    }
}

/// Render a [`BeepPattern`] into i16 PCM at wire rate (48 kHz). Each
/// step is synthesised independently with a short linear fade in/out
/// at the seams so back-to-back notes don't click:
///
/// * [`BeepStep::Tone`] — a sine at the requested frequency.
/// * [`BeepStep::Rest`] — silence (zeros) for the step's duration.
/// * [`BeepStep::Noise`] — uniform white noise from a tiny inline
///   xorshift PRNG, passed through an RBJ-style bandpass biquad
///   centred at `center_hz` with a Q derived from
///   `center_hz / bandwidth_hz`. Output is rescaled so the post-
///   filter peak roughly matches the tone path's level.
///
/// Push the result through `push_playback` to play it locally — the
/// output callback handles the device-rate conversion.
pub fn beep_pattern(steps: &[BeepStep], amplitude: f32) -> Vec<i16> {
    let rate = SAMPLE_RATE_HZ;
    let amp = i16::MAX as f32 * amplitude.clamp(0.0, 1.0);
    let total_samples: usize = steps
        .iter()
        .map(|s| (rate as f32 * s.duration_ms() as f32 / 1000.0) as usize)
        .sum();
    let mut out = Vec::with_capacity(total_samples);

    // Inline xorshift PRNG state — keeps noise reproducible-ish and
    // avoids pulling in the `rand` crate just for one feature. The
    // seed is arbitrary but non-zero (xorshift jams on 0).
    let mut rng_state: u32 = 0x1234_5678;

    for step in steps {
        let len = (rate as f32 * step.duration_ms() as f32 / 1000.0) as usize;
        // 5 ms ramp at each end, but clamp to a quarter of the step
        // so 50 ms notes still get a fade without losing all sustain.
        let fade = ((rate as f32 * 0.005) as usize).min(len / 4);
        let envelope = |i: usize| -> f32 {
            if fade > 0 && i < fade {
                i as f32 / fade as f32
            } else if fade > 0 && i + fade > len {
                (len.saturating_sub(i)) as f32 / fade as f32
            } else {
                1.0
            }
        };

        match *step {
            BeepStep::Rest { .. } => {
                out.extend(std::iter::repeat_n(0_i16, len));
            }
            BeepStep::Tone { freq_hz, .. } => {
                out.extend((0..len).map(|i| {
                    let t = i as f32 / rate as f32;
                    let env = envelope(i);
                    let sample = (2.0 * std::f32::consts::PI * freq_hz * t).sin() * amp * env;
                    sample as i16
                }));
            }
            BeepStep::Noise {
                center_hz,
                bandwidth_hz,
                ..
            } => {
                // RBJ bandpass biquad with constant 0 dB peak gain.
                // Q = center / bandwidth; clamp center to Nyquist
                // safe range and bandwidth to >0 so we don't divide
                // by zero on a hand-edited preset.
                let f0 = center_hz.clamp(20.0, rate as f32 * 0.45);
                let bw = bandwidth_hz.max(1.0);
                let q = f0 / bw;
                let omega = 2.0 * std::f32::consts::PI * f0 / rate as f32;
                let alpha = omega.sin() / (2.0 * q);
                let b0 = alpha;
                let b1 = 0.0_f32;
                let b2 = -alpha;
                let a0 = 1.0 + alpha;
                let a1 = -2.0 * omega.cos();
                let a2 = 1.0 - alpha;
                let (b0, b1, b2) = (b0 / a0, b1 / a0, b2 / a0);
                let (a1, a2) = (a1 / a0, a2 / a0);

                // Bandpass cuts a lot of energy — naive output is
                // very quiet relative to a tone at the same amp. The
                // ~3.5× scale puts the post-filter peak roughly in
                // line with a sine of the same frequency at the
                // requested amplitude. Hard-clamped at i16 below.
                let post_filter_gain = 3.5_f32;

                let mut x1 = 0.0_f32;
                let mut x2 = 0.0_f32;
                let mut y1 = 0.0_f32;
                let mut y2 = 0.0_f32;
                out.extend((0..len).map(|i| {
                    let raw = next_white(&mut rng_state); // [-1, 1]
                    let y = b0 * raw + b1 * x1 + b2 * x2 - a1 * y1 - a2 * y2;
                    x2 = x1;
                    x1 = raw;
                    y2 = y1;
                    y1 = y;
                    let env = envelope(i);
                    let sample =
                        (y * post_filter_gain * amp * env).clamp(i16::MIN as f32, i16::MAX as f32);
                    sample as i16
                }));
            }
        }
    }
    out
}

/// One step of a 32-bit xorshift PRNG, used by [`beep_pattern`] for
/// the noise variant. Returns the new state.
#[inline]
fn xorshift32(state: &mut u32) -> u32 {
    let mut x = *state;
    x ^= x << 13;
    x ^= x >> 17;
    x ^= x << 5;
    *state = x;
    x
}

/// Draw a uniform sample in `[-1.0, 1.0)` from the xorshift PRNG.
#[inline]
fn next_white(state: &mut u32) -> f32 {
    (xorshift32(state) as f32 / u32::MAX as f32) * 2.0 - 1.0
}

#[cfg(test)]
mod beep_tests {
    //! These tests exercise the pure-function audio helpers — the
    //! ones with no cpal dependency. They protect against
    //! regressions in:
    //!
    //!   * Wire-rate frame sizing (a 10 ms step at 48 kHz produces
    //!     exactly 480 samples). Off-by-one here would clip every
    //!     beep.
    //!   * Rests producing zero-valued PCM, not random garbage.
    //!   * The PRNG used by the Noise variant being deterministic
    //!     for a fixed initial state — important because clients
    //!     in the same room must hear the *same* hiss, not each
    //!     a different stream.
    //!   * Preset table lookups.
    use super::*;
    use toki_proto::wire::SAMPLE_RATE_HZ;

    #[test]
    fn push_voice_keeps_small_backlog_intact() {
        // Below the cap, nothing is dropped — every sample is preserved.
        let buf: PlaybackBuf = Arc::new(Mutex::new(Mixer::default()));
        push_voice(&buf, 1, &vec![7i16; VOICE_TARGET_SAMPLES]);
        assert_eq!(
            buf.lock().unwrap().sender_buf(1).unwrap().len(),
            VOICE_TARGET_SAMPLES
        );
    }

    #[test]
    fn push_voice_trims_to_target_when_backlog_exceeds_cap() {
        let buf: PlaybackBuf = Arc::new(Mutex::new(Mixer::default()));
        // Overflow the hard ceiling in one shot.
        push_voice(&buf, 1, &vec![1i16; VOICE_MAX_SAMPLES + 5000]);
        // It snaps back to the target, not just shaves the overflow —
        // so latency can't sit pinned at the ceiling.
        assert_eq!(
            buf.lock().unwrap().sender_buf(1).unwrap().len(),
            VOICE_TARGET_SAMPLES
        );
    }

    #[test]
    fn push_voice_catch_up_drops_oldest_keeps_newest() {
        let buf: PlaybackBuf = Arc::new(Mutex::new(Mixer::default()));
        // Fill near the cap with a marker value, then push fresh audio
        // that tips it over. The retained window must be the *newest*
        // audio (the catch-up discards the stale front).
        push_voice(&buf, 1, &vec![1i16; VOICE_MAX_SAMPLES - 10]);
        push_voice(&buf, 1, &vec![2i16; 100]);
        let guard = buf.lock().unwrap();
        let sb = guard.sender_buf(1).unwrap();
        assert_eq!(sb.len(), VOICE_TARGET_SAMPLES);
        assert_eq!(*sb.back().unwrap(), 2, "freshest sample retained");
    }

    #[test]
    fn mixer_sums_two_concurrent_senders() {
        // Full-duplex core: two talkers' streams add sample-for-sample,
        // and mixing stops once both buffers are dry.
        let buf: PlaybackBuf = Arc::new(Mutex::new(Mixer::default()));
        push_voice(&buf, 1, &[100, 100, 100]);
        push_voice(&buf, 2, &[10, 20, 30]);
        let mut out = Vec::new();
        buf.lock().unwrap().mix_into(&mut out, 8);
        assert_eq!(out, vec![110, 120, 130], "per-position sum of both senders");
    }

    #[test]
    fn beep_pattern_lengths_match_durations() {
        // 100 ms at 48 kHz = 4800 samples per step.
        let steps = vec![
            BeepStep::Tone {
                freq_hz: 1000.0,
                duration_ms: 100,
            },
            BeepStep::Rest { duration_ms: 100 },
        ];
        let pcm = beep_pattern(&steps, 0.5);
        assert_eq!(pcm.len(), 9600);
        // The samples that fall under the Rest must be exactly 0 —
        // no PRNG noise, no leftover phase from the tone.
        for sample in &pcm[4800..] {
            assert_eq!(*sample, 0);
        }
    }

    #[test]
    fn beep_pattern_total_duration_matches_helper() {
        let steps = &[
            BeepStep::Tone {
                freq_hz: 440.0,
                duration_ms: 50,
            },
            BeepStep::Rest { duration_ms: 200 },
            BeepStep::Tone {
                freq_hz: 880.0,
                duration_ms: 50,
            },
        ];
        let pattern = BeepPattern { steps };
        assert_eq!(pattern.total_duration_ms(), 300);
    }

    #[test]
    fn beep_pattern_zero_amplitude_is_silent() {
        let steps = vec![BeepStep::Tone {
            freq_hz: 1000.0,
            duration_ms: 50,
        }];
        let pcm = beep_pattern(&steps, 0.0);
        assert!(pcm.iter().all(|&s| s == 0));
    }

    #[test]
    fn beep_pattern_noise_is_deterministic_across_runs() {
        // Same input must give same output. If the PRNG seed ever
        // moves to a random/time source, peers in the same room
        // would each render a different waveform — confusing to
        // debug, and the fingerprint hashing in the test below
        // would break.
        let steps = vec![BeepStep::Noise {
            center_hz: 1000.0,
            bandwidth_hz: 500.0,
            duration_ms: 50,
        }];
        let a = beep_pattern(&steps, 0.2);
        let b = beep_pattern(&steps, 0.2);
        assert_eq!(a, b);
    }

    #[test]
    fn beep_pattern_handles_sub_fade_lengths() {
        // A 1 ms step is shorter than the 5 ms target fade. The
        // renderer clamps fade to len/4 — verify it doesn't
        // crash and still produces samples.
        let steps = vec![BeepStep::Tone {
            freq_hz: 1000.0,
            duration_ms: 1,
        }];
        let pcm = beep_pattern(&steps, 0.5);
        let expected_samples = SAMPLE_RATE_HZ as usize / 1000;
        assert_eq!(pcm.len(), expected_samples);
    }

    #[test]
    fn xorshift_advances_state_and_is_nonzero() {
        let mut state = 1;
        let a = xorshift32(&mut state);
        let b = xorshift32(&mut state);
        assert_ne!(a, b);
        assert_ne!(state, 1);
    }

    #[test]
    fn beep_preset_by_id_falls_back_to_default() {
        let unknown = BeepPreset::by_id("does-not-exist");
        // Default lives at index 0, so the fallback equals it.
        assert_eq!(unknown.id, BeepPreset::ALL[0].id);
    }

    #[test]
    fn beep_preset_index_of_unknown_id_is_zero() {
        assert_eq!(BeepPreset::index_of("garbage"), 0);
        assert_eq!(BeepPreset::index_of(BeepPreset::ALL[0].id), 0);
    }

    #[test]
    fn beep_params_current_preset_matches_index() {
        let bp = BeepParams::new(0, 0.5);
        assert_eq!(bp.current_preset().id, BeepPreset::ALL[0].id);
        if BeepPreset::ALL.len() > 1 {
            bp.set_preset_index(1);
            assert_eq!(bp.current_preset().id, BeepPreset::ALL[1].id);
        }
    }

    #[test]
    fn beep_params_volume_round_trips() {
        let bp = BeepParams::new(0, 0.25);
        assert_eq!(bp.volume(), 0.25);
        bp.set_volume(0.75);
        assert_eq!(bp.volume(), 0.75);
    }
}
