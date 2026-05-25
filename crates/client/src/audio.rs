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
//!   - Mono output samples are replicated across all device channels.
//!   - Non-f32 sample formats (i16, u16) are converted in the callback.
//!   - Sample-rate mismatch is logged but NOT resampled — both clients
//!     should run at the same device rate for clean audio. Mixed rates
//!     (e.g. macOS 48 kHz ↔ Windows 44.1 kHz) will play at the wrong pitch.

use std::collections::VecDeque;
use std::sync::{Arc, Mutex};

use anyhow::{Context, Result, anyhow};
use cpal::traits::{DeviceTrait, HostTrait, StreamTrait};
use cpal::{FromSample, SizedSample};
use tokio::sync::mpsc::{UnboundedReceiver, UnboundedSender, unbounded_channel};
use tracing::{info, warn};

use toki_proto::wire::{FRAME_SAMPLES, SAMPLE_RATE_HZ};

pub type PlaybackBuf = Arc<Mutex<VecDeque<i16>>>;

pub struct AudioHandle {
    pub mic_rx: UnboundedReceiver<Vec<i16>>,
    pub playback: PlaybackBuf,
    /// Device names visible to cpal at startup. We don't auto-refresh —
    /// the user can restart the app if they hot-plug new hardware.
    pub devices: AudioDevices,
    /// Send `Set{Input,Output}` commands here to hot-swap the active
    /// streams. Cheap and `Clone`-able.
    pub control: AudioControl,
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
) -> Result<AudioHandle> {
    let (mic_tx, mic_rx) = unbounded_channel::<Vec<i16>>();
    let playback: PlaybackBuf = Arc::new(Mutex::new(VecDeque::with_capacity(
        PREFERRED_RATE as usize / 2,
    )));
    let (cmd_tx, cmd_rx) = std::sync::mpsc::channel::<AudioCmd>();

    let (init_tx, init_rx) = std::sync::mpsc::channel::<AudioDevices>();

    let mic_tx_for_thread = mic_tx;
    let playback_for_thread = playback.clone();

    std::thread::Builder::new()
        .name("toki-audio".into())
        .spawn(move || {
            let host = cpal::default_host();
            let _ = init_tx.send(enumerate(&host));

            let mut input_stream =
                open_input_for(&host, initial_input.as_deref(), &mic_tx_for_thread);
            let mut output_stream =
                open_output_for(&host, initial_output.as_deref(), &playback_for_thread);

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
                        input_stream =
                            open_input_for(&host, name.as_deref(), &mic_tx_for_thread);
                    }
                    Ok(AudioCmd::SetOutput(name)) => {
                        drop(output_stream.take());
                        output_stream =
                            open_output_for(&host, name.as_deref(), &playback_for_thread);
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
            if let Some(d) = iter.into_iter().find(|d| d.name().ok().as_deref() == Some(want)) {
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
            if let Some(d) = iter.into_iter().find(|d| d.name().ok().as_deref() == Some(want)) {
                return Some(d);
            }
        }
        warn!(name = %want, "saved output device not found, falling back to host default");
    }
    host.default_output_device()
}

fn open_input_for(
    host: &cpal::Host,
    name: Option<&str>,
    mic_tx: &UnboundedSender<Vec<i16>>,
) -> Option<cpal::Stream> {
    let device = pick_input(host, name)?;
    let device_name = device.name().unwrap_or_else(|_| "?".into());
    let stream = match open_input(&device, mic_tx.clone()) {
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

fn open_output_for(
    host: &cpal::Host,
    name: Option<&str>,
    playback: &PlaybackBuf,
) -> Option<cpal::Stream> {
    let device = pick_output(host, name)?;
    let device_name = device.name().unwrap_or_else(|_| "?".into());
    let stream = match open_output(&device, playback.clone()) {
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

fn open_input(dev: &cpal::Device, mic_tx: UnboundedSender<Vec<i16>>) -> Result<cpal::Stream> {
    // First try our platform-preferred mono f32 config.
    match build_input_stream::<f32>(dev, &PREFERRED, 1, mic_tx.clone()) {
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
    if rate != PREFERRED_RATE {
        warn!(
            "input rate {rate} ≠ preferred {PREFERRED_RATE}; cross-OS clients on different device rates will hear pitch-shifted audio"
        );
    }

    match format {
        cpal::SampleFormat::F32 => build_input_stream::<f32>(dev, &cfg, channels, mic_tx),
        cpal::SampleFormat::I16 => build_input_stream::<i16>(dev, &cfg, channels, mic_tx),
        cpal::SampleFormat::U16 => build_input_stream::<u16>(dev, &cfg, channels, mic_tx),
        other => Err(anyhow!("unsupported input sample format: {other:?}")),
    }
}

fn open_output(dev: &cpal::Device, playback: PlaybackBuf) -> Result<cpal::Stream> {
    match build_output_stream::<f32>(dev, &PREFERRED, 1, playback.clone()) {
        Ok(s) => {
            info!("output: 1 ch @ {PREFERRED_RATE} Hz f32 (preferred)");
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
    if rate != PREFERRED_RATE {
        warn!(
            "output rate {rate} ≠ preferred {PREFERRED_RATE}; cross-OS clients on different device rates will hear pitch-shifted audio"
        );
    }

    match format {
        cpal::SampleFormat::F32 => build_output_stream::<f32>(dev, &cfg, channels, playback),
        cpal::SampleFormat::I16 => build_output_stream::<i16>(dev, &cfg, channels, playback),
        cpal::SampleFormat::U16 => build_output_stream::<u16>(dev, &cfg, channels, playback),
        other => Err(anyhow!("unsupported output sample format: {other:?}")),
    }
}

fn build_input_stream<T>(
    dev: &cpal::Device,
    cfg: &cpal::StreamConfig,
    channels: u16,
    mic_tx: UnboundedSender<Vec<i16>>,
) -> Result<cpal::Stream>
where
    T: SizedSample + Send + 'static,
    f32: FromSample<T>,
{
    let ch = channels as usize;
    let mut accum: Vec<i16> = Vec::with_capacity(FRAME_SAMPLES);
    let stream = dev.build_input_stream(
        cfg,
        move |data: &[T], _: &cpal::InputCallbackInfo| {
            // Interleaved samples; downmix N channels to mono by averaging.
            for frame in data.chunks_exact(ch) {
                let sum: f32 = frame.iter().map(|&s| s.to_sample::<f32>()).sum();
                let avg = sum / ch as f32;
                let v = (avg.clamp(-1.0, 1.0) * i16::MAX as f32) as i16;
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

fn build_output_stream<T>(
    dev: &cpal::Device,
    cfg: &cpal::StreamConfig,
    channels: u16,
    playback: PlaybackBuf,
) -> Result<cpal::Stream>
where
    T: SizedSample + FromSample<f32> + Send + 'static,
{
    let ch = channels as usize;
    let stream = dev.build_output_stream(
        cfg,
        move |data: &mut [T], _: &cpal::OutputCallbackInfo| {
            let mut buf = playback.lock().unwrap();
            // Pop one mono sample, replicate it across N output channels.
            for frame in data.chunks_mut(ch) {
                let mono = buf.pop_front().unwrap_or(0);
                let f = mono as f32 / i16::MAX as f32;
                let s: T = T::from_sample(f);
                for slot in frame.iter_mut() {
                    *slot = s;
                }
            }
        },
        |e| warn!(error = %e, "output stream error"),
        None,
    )?;
    Ok(stream)
}

/// Append decoded PCM into the playback ring. Caps the queue at 500 ms to
/// prevent latency from snowballing if we receive faster than the speaker
/// drains.
pub fn push_playback(buf: &PlaybackBuf, samples: &[i16]) {
    let mut guard = buf.lock().unwrap();
    // 500 ms at the local device rate.
    let cap = (PREFERRED_RATE / 2) as usize;
    for &s in samples {
        if guard.len() >= cap {
            guard.pop_front();
        }
        guard.push_back(s);
    }
}

/// Generate a single-tone "roger beep" with a short linear fade in/out so it
/// doesn't click. The result is plain i16 PCM at the project sample rate —
/// push it through `push_playback` to play it locally.
pub fn beep(freq_hz: f32, duration_ms: u32, amplitude: f32) -> Vec<i16> {
    // Generated at the local device rate so the beep is the right
    // duration and frequency for the stream it'll be mixed into.
    let total = (PREFERRED_RATE as f32 * duration_ms as f32 / 1000.0) as usize;
    let fade = (PREFERRED_RATE as f32 * 0.005) as usize; // 5 ms ramp
    let amp = i16::MAX as f32 * amplitude.clamp(0.0, 1.0);
    (0..total)
        .map(|i| {
            let t = i as f32 / PREFERRED_RATE as f32;
            let env = if i < fade {
                i as f32 / fade as f32
            } else if i + fade > total {
                (total.saturating_sub(i)) as f32 / fade as f32
            } else {
                1.0
            };
            let sample = (2.0 * std::f32::consts::PI * freq_hz * t).sin() * amp * env;
            sample as i16
        })
        .collect()
}
