//! cpal capture/playback running on a dedicated OS thread.
//!
//! cpal's `Stream` is neither `Send` nor `Sync` on most platforms, so we
//! spawn a thread that owns both streams and parks forever — the streams
//! themselves push/pull samples through channels and a shared ring buffer.

use std::collections::VecDeque;
use std::sync::{Arc, Mutex};

use anyhow::{Result, anyhow};
use cpal::traits::{DeviceTrait, HostTrait, StreamTrait};
use tokio::sync::mpsc::{UnboundedReceiver, unbounded_channel};
use tracing::{info, warn};

use toki_proto::wire::{FRAME_SAMPLES, SAMPLE_RATE_HZ};

/// Shared FIFO of decoded i16 samples ready for the speaker callback to
/// consume. We deliberately keep it simple (Mutex<VecDeque>) — the cap below
/// bounds latency build-up if the receive side is faster than playback.
pub type PlaybackBuf = Arc<Mutex<VecDeque<i16>>>;

pub struct AudioHandle {
    /// 10 ms mono i16 frames captured from the default input device.
    pub mic_rx: UnboundedReceiver<Vec<i16>>,
    pub playback: PlaybackBuf,
}

pub fn spawn() -> Result<AudioHandle> {
    type InitResult = Result<(UnboundedReceiver<Vec<i16>>, PlaybackBuf)>;
    let (init_tx, init_rx) = std::sync::mpsc::channel::<InitResult>();

    std::thread::Builder::new()
        .name("toki-audio".into())
        .spawn(move || {
            let streams = match build_streams() {
                Ok((streams, mic_rx, playback)) => {
                    let _ = init_tx.send(Ok((mic_rx, playback)));
                    streams
                }
                Err(e) => {
                    let _ = init_tx.send(Err(e));
                    return;
                }
            };
            // Keep the cpal streams alive for the lifetime of the process.
            let _keep = streams;
            loop {
                std::thread::park();
            }
        })?;

    init_rx.recv()?.map(|(mic_rx, playback)| AudioHandle { mic_rx, playback })
}

fn build_streams() -> Result<((cpal::Stream, cpal::Stream), UnboundedReceiver<Vec<i16>>, PlaybackBuf)> {
    let host = cpal::default_host();
    let input_dev = host
        .default_input_device()
        .ok_or_else(|| anyhow!("no default input device"))?;
    let output_dev = host
        .default_output_device()
        .ok_or_else(|| anyhow!("no default output device"))?;

    let config = cpal::StreamConfig {
        channels: 1,
        sample_rate: cpal::SampleRate(SAMPLE_RATE_HZ),
        buffer_size: cpal::BufferSize::Default,
    };

    info!(
        input = %input_dev.name().unwrap_or_else(|_| "?".into()),
        output = %output_dev.name().unwrap_or_else(|_| "?".into()),
        "opening audio @ {SAMPLE_RATE_HZ} Hz mono, {FRAME_SAMPLES}-sample frames"
    );

    let (mic_tx, mic_rx) = unbounded_channel::<Vec<i16>>();
    let playback: PlaybackBuf = Arc::new(Mutex::new(VecDeque::with_capacity(
        SAMPLE_RATE_HZ as usize / 2,
    )));

    // ── Capture ────────────────────────────────────────────────────────
    // cpal hands us variable-sized buffers; we accumulate into 10 ms frames
    // and forward each complete frame to the runtime.
    let mut accum: Vec<i16> = Vec::with_capacity(FRAME_SAMPLES);
    let input_stream = input_dev.build_input_stream(
        &config,
        move |data: &[f32], _: &cpal::InputCallbackInfo| {
            for &s in data {
                let v = (s.clamp(-1.0, 1.0) * i16::MAX as f32) as i16;
                accum.push(v);
                if accum.len() >= FRAME_SAMPLES {
                    let frame = std::mem::replace(&mut accum, Vec::with_capacity(FRAME_SAMPLES));
                    // unbounded_send is non-blocking — safe to call from the
                    // audio callback. If the receiver was dropped we'd just
                    // discard the frame, which is the right behavior.
                    let _ = mic_tx.send(frame);
                }
            }
        },
        |e| warn!(error = %e, "input stream error"),
        None,
    )?;

    // ── Playback ───────────────────────────────────────────────────────
    let playback_for_out = playback.clone();
    let output_stream = output_dev.build_output_stream(
        &config,
        move |data: &mut [f32], _: &cpal::OutputCallbackInfo| {
            let mut buf = playback_for_out.lock().unwrap();
            for slot in data.iter_mut() {
                *slot = buf
                    .pop_front()
                    .map(|s| s as f32 / i16::MAX as f32)
                    .unwrap_or(0.0);
            }
        },
        |e| warn!(error = %e, "output stream error"),
        None,
    )?;

    input_stream.play()?;
    output_stream.play()?;

    Ok(((input_stream, output_stream), mic_rx, playback))
}

/// Append decoded PCM into the playback ring. Caps the queue at 500 ms to
/// prevent latency from snowballing if we receive faster than the speaker
/// drains.
pub fn push_playback(buf: &PlaybackBuf, samples: &[i16]) {
    let mut guard = buf.lock().unwrap();
    let cap = (SAMPLE_RATE_HZ / 2) as usize;
    for &s in samples {
        if guard.len() >= cap {
            guard.pop_front();
        }
        guard.push_back(s);
    }
}
