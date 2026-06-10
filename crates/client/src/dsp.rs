//! Capture-side voice DSP: noise suppression + automatic gain control.
//!
//! Sits between the mic frame stream and the Opus encoder, operating on
//! the same 480-sample / 48 kHz mono frames the wire uses — one frame
//! in, one frame out, no rebuffering. Two independent stages, each with
//! a live toggle (some operators *want* the raw CB-radio character):
//!
//!   1. **Noise suppression** — RNNoise (via the pure-Rust `nnnoiseless`
//!      port): a small RNN that estimates per-band gains to isolate
//!      speech from steady background noise (fans, hum, keyboard hash).
//!      It also emits a per-frame voice-activity probability, which we
//!      publish through [`DspParams`] — the level/VAD primitive a future
//!      VOX feature reads.
//!   2. **AGC** — a simple feedback loop that eases the frame RMS toward
//!      a fixed target so quiet mics stop whispering and hot mics stop
//!      shouting. Adapts only while speech is present (no pumping the
//!      noise floor up during pauses), descends fast / ascends slow, and
//!      caps the per-frame peak so a sudden shout can't be boosted into
//!      clipping.
//!
//! The runtime feeds *every* mic frame through [`Dsp::process`] while a
//! session is live — not just while PTT is held — so the denoiser's RNN
//! state and the AGC gain are already settled when a transmission
//! starts, instead of converging audibly over its first half-second.

use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};
use std::sync::Arc;

use nnnoiseless::DenoiseState;

use toki_proto::wire::FRAME_SAMPLES;

// The whole module leans on the wire frame and RNNoise's native frame
// being the same shape. If either constant ever moves, fail the build
// rather than feeding the model misaligned windows.
const _: () = assert!(nnnoiseless::FRAME_SIZE == FRAME_SAMPLES);

/// RMS level the AGC steers speech toward, in i16 sample units.
/// ≈ −18 dBFS — loud enough to sit comfortably above the noise floor,
/// with ~8× of headroom before clipping for consonant peaks.
const TARGET_RMS: f32 = 4000.0;
/// AGC gain bounds: at most −6 dB of cut (a screaming mic is mostly
/// the user's business) and +18 dB of boost (beyond that we'd mainly
/// amplify the mic's self-noise).
const AGC_MIN_GAIN: f32 = 0.5;
const AGC_MAX_GAIN: f32 = 8.0;
/// Per-frame approach rates toward the desired gain. Descend fast
/// (≈50 ms to recover from a sudden shout), ascend slow (≈0.5 s time
/// constant) so trailing-off sentences don't drag the noise floor up.
const AGC_ATTACK: f32 = 0.35;
const AGC_RELEASE: f32 = 0.02;
/// Frames with RMS below this (≈ −44 dBFS) are treated as silence:
/// the AGC holds its gain rather than adapting to nothing.
const ACTIVITY_RMS_FLOOR: f32 = 200.0;
/// RNNoise VAD probability above which a frame counts as speech for
/// AGC adaptation purposes.
const VAD_ACTIVE: f32 = 0.5;
/// Post-AGC peak ceiling, just under i16::MAX. The desired gain is
/// capped each frame so `peak × gain` stays below this — a brick
/// limiter that keeps the slow-moving loop from boosting a transient
/// into hard clipping.
const PEAK_CEILING: f32 = 32_600.0;

/// Lock-free DSP controls shared between the UI thread (writes the
/// toggles), the runtime loop (reads them each frame, writes the VAD),
/// and any future VAD consumer. Same `Arc`-of-atomics pattern as
/// `audio::AudioGains` — the per-frame path stays mutex-free.
#[derive(Clone)]
pub struct DspParams {
    noise_suppression: Arc<AtomicBool>,
    agc: Arc<AtomicBool>,
    /// Latest voice-activity estimate in `[0.0, 1.0]`. RNNoise's VAD
    /// probability when suppression is on; a coarse RMS-derived 0/1
    /// when it's off. Published every processed frame.
    vad: Arc<AtomicU32>,
}

impl DspParams {
    pub fn new(noise_suppression: bool, agc: bool) -> Self {
        Self {
            noise_suppression: Arc::new(AtomicBool::new(noise_suppression)),
            agc: Arc::new(AtomicBool::new(agc)),
            vad: Arc::new(AtomicU32::new(0)),
        }
    }

    pub fn set_noise_suppression(&self, on: bool) {
        self.noise_suppression.store(on, Ordering::Relaxed);
    }

    pub fn set_agc(&self, on: bool) {
        self.agc.store(on, Ordering::Relaxed);
    }

    pub fn noise_suppression(&self) -> bool {
        self.noise_suppression.load(Ordering::Relaxed)
    }

    pub fn agc(&self) -> bool {
        self.agc.load(Ordering::Relaxed)
    }

    /// Latest voice-activity estimate, `[0.0, 1.0]`. No UI consumer
    /// yet — this is the hook a future VOX (voice-activated transmit)
    /// threshold reads.
    #[allow(dead_code)]
    pub fn vad(&self) -> f32 {
        f32::from_bits(self.vad.load(Ordering::Relaxed))
    }

    fn set_vad(&self, v: f32) {
        self.vad.store(v.to_bits(), Ordering::Relaxed);
    }
}

/// The stateful processor, owned by the runtime loop. Holds the RNNoise
/// state and the AGC's current gain across frames.
pub struct Dsp {
    params: DspParams,
    denoise: Box<DenoiseState<'static>>,
    /// Scratch buffers in RNNoise's convention: f32 samples in the
    /// *i16 value range* (±32768), not the ±1.0 float-PCM range.
    in_buf: [f32; FRAME_SAMPLES],
    out_buf: [f32; FRAME_SAMPLES],
    /// Current AGC gain, carried across frames.
    gain: f32,
}

impl Dsp {
    pub fn new(params: DspParams) -> Self {
        Self {
            params,
            denoise: DenoiseState::new(),
            in_buf: [0.0; FRAME_SAMPLES],
            out_buf: [0.0; FRAME_SAMPLES],
            gain: 1.0,
        }
    }

    /// Run the enabled stages over one wire frame, in place. With both
    /// stages off this is a bit-exact passthrough (no f32 round-trip),
    /// so disabling the DSP really does mean "raw mic".
    ///
    /// Frames of unexpected length (shouldn't happen — the capture
    /// side accumulates exactly `FRAME_SAMPLES`) pass through untouched
    /// rather than feeding the model a misaligned window.
    pub fn process(&mut self, frame: &mut [i16]) {
        if frame.len() != FRAME_SAMPLES {
            return;
        }
        let ns = self.params.noise_suppression();
        let agc = self.params.agc();
        if !ns && !agc {
            // Fully bypassed: leave the samples alone but keep the VAD
            // readout alive with a cheap RMS-derived estimate so a
            // consumer never sees a frozen stale value.
            let rms = rms(frame.iter().map(|&s| s as f32));
            self.params
                .set_vad(if rms > ACTIVITY_RMS_FLOOR { 1.0 } else { 0.0 });
            return;
        }

        for (dst, &s) in self.in_buf.iter_mut().zip(frame.iter()) {
            *dst = s as f32;
        }

        let vad = if ns {
            self.denoise
                .process_frame(&mut self.out_buf, &self.in_buf)
                .clamp(0.0, 1.0)
        } else {
            self.out_buf.copy_from_slice(&self.in_buf);
            if rms(self.out_buf.iter().copied()) > ACTIVITY_RMS_FLOOR {
                1.0
            } else {
                0.0
            }
        };
        self.params.set_vad(vad);

        if agc {
            self.apply_agc(vad);
        }

        for (dst, &v) in frame.iter_mut().zip(self.out_buf.iter()) {
            *dst = v.clamp(i16::MIN as f32, i16::MAX as f32) as i16;
        }
    }

    /// One AGC step over `out_buf`. Feedback design: measure the frame,
    /// nudge the gain toward `TARGET_RMS / measured`, apply with a
    /// per-sample linear ramp from the previous gain so a step change
    /// never lands as a click ("zipper noise").
    fn apply_agc(&mut self, vad: f32) {
        let frame_rms = rms(self.out_buf.iter().copied());
        let frame_peak = self
            .out_buf
            .iter()
            .fold(0.0_f32, |acc, &v| acc.max(v.abs()));

        let mut desired = if frame_rms > ACTIVITY_RMS_FLOOR && vad >= VAD_ACTIVE {
            (TARGET_RMS / frame_rms).clamp(AGC_MIN_GAIN, AGC_MAX_GAIN)
        } else {
            // Silence or non-speech: hold the current gain. Adapting
            // here would slowly crank the gain to max during pauses
            // and greet the next sentence with a blast of noise.
            self.gain
        };
        // Brick limiter: never let this frame's peak through above the
        // ceiling, whatever the RMS logic wanted.
        if frame_peak * desired > PEAK_CEILING {
            desired = (PEAK_CEILING / frame_peak).max(AGC_MIN_GAIN);
        }

        let alpha = if desired < self.gain {
            AGC_ATTACK
        } else {
            AGC_RELEASE
        };
        let new_gain = self.gain + (desired - self.gain) * alpha;

        let n = self.out_buf.len() as f32;
        for (i, v) in self.out_buf.iter_mut().enumerate() {
            let g = self.gain + (new_gain - self.gain) * ((i + 1) as f32 / n);
            *v *= g;
        }
        self.gain = new_gain;
    }

    /// Current AGC gain — exposed for tests only.
    #[cfg(test)]
    fn current_gain(&self) -> f32 {
        self.gain
    }
}

/// Root-mean-square of a sample stream (i16-range f32 units).
fn rms(samples: impl ExactSizeIterator<Item = f32>) -> f32 {
    let n = samples.len();
    if n == 0 {
        return 0.0;
    }
    (samples.map(|v| v * v).sum::<f32>() / n as f32).sqrt()
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A full wire frame of a sine at `amp` (i16 units), phase-continuous
    /// across calls via `phase`.
    fn sine_frame(amp: f32, phase: &mut f32) -> Vec<i16> {
        let step = 2.0 * std::f32::consts::PI * 440.0 / 48_000.0;
        (0..FRAME_SAMPLES)
            .map(|_| {
                let s = (*phase).sin() * amp;
                *phase += step;
                s as i16
            })
            .collect()
    }

    /// Deterministic white-noise frame from a xorshift PRNG, same
    /// generator family the beep renderer uses.
    fn noise_frame(amp: f32, state: &mut u32) -> Vec<i16> {
        (0..FRAME_SAMPLES)
            .map(|_| {
                let mut x = *state;
                x ^= x << 13;
                x ^= x >> 17;
                x ^= x << 5;
                *state = x;
                ((x as f32 / u32::MAX as f32 * 2.0 - 1.0) * amp) as i16
            })
            .collect()
    }

    fn frame_rms(frame: &[i16]) -> f32 {
        rms(frame.iter().map(|&s| s as f32))
    }

    #[test]
    fn params_toggles_round_trip() {
        let p = DspParams::new(true, false);
        assert!(p.noise_suppression());
        assert!(!p.agc());
        p.set_noise_suppression(false);
        p.set_agc(true);
        assert!(!p.noise_suppression());
        assert!(p.agc());
    }

    #[test]
    fn bypass_is_bit_exact() {
        // Both stages off must not touch the samples at all — not even
        // an i16 → f32 → i16 round trip.
        let mut dsp = Dsp::new(DspParams::new(false, false));
        let mut phase = 0.0;
        let original = sine_frame(12_345.0, &mut phase);
        let mut frame = original.clone();
        dsp.process(&mut frame);
        assert_eq!(frame, original);
    }

    #[test]
    fn bypass_still_publishes_activity() {
        let params = DspParams::new(false, false);
        let mut dsp = Dsp::new(params.clone());
        let mut phase = 0.0;
        let mut loud = sine_frame(8_000.0, &mut phase);
        dsp.process(&mut loud);
        assert_eq!(params.vad(), 1.0);
        let mut silence = vec![0i16; FRAME_SAMPLES];
        dsp.process(&mut silence);
        assert_eq!(params.vad(), 0.0);
    }

    #[test]
    fn wrong_length_frame_passes_through() {
        let mut dsp = Dsp::new(DspParams::new(true, true));
        let original = vec![1000i16; FRAME_SAMPLES / 2];
        let mut frame = original.clone();
        dsp.process(&mut frame);
        assert_eq!(frame, original);
    }

    #[test]
    fn agc_boosts_quiet_signal_toward_target() {
        // NS off so the test exercises the AGC in isolation (a sine
        // isn't speech — RNNoise would rightly suppress it).
        let mut dsp = Dsp::new(DspParams::new(false, true));
        let mut phase = 0.0;
        // Quiet input: RMS ≈ 566, needs ≈ 7× of boost (within bounds).
        let mut last_rms = 0.0;
        for _ in 0..400 {
            let mut frame = sine_frame(800.0, &mut phase);
            dsp.process(&mut frame);
            last_rms = frame_rms(&frame);
        }
        assert!(
            (last_rms - TARGET_RMS).abs() < TARGET_RMS * 0.25,
            "converged RMS {last_rms} not near target {TARGET_RMS}"
        );
    }

    #[test]
    fn agc_attenuates_loud_signal_toward_target() {
        let mut dsp = Dsp::new(DspParams::new(false, true));
        let mut phase = 0.0;
        // Hot input: RMS ≈ 5657, needs ≈ 0.7× (within bounds). Attack
        // is fast so a few dozen frames is plenty.
        let mut last_rms = 0.0;
        for _ in 0..50 {
            let mut frame = sine_frame(8_000.0, &mut phase);
            dsp.process(&mut frame);
            last_rms = frame_rms(&frame);
        }
        assert!(
            (last_rms - TARGET_RMS).abs() < TARGET_RMS * 0.25,
            "converged RMS {last_rms} not near target {TARGET_RMS}"
        );
    }

    #[test]
    fn agc_boost_is_clamped() {
        let mut dsp = Dsp::new(DspParams::new(false, true));
        let mut phase = 0.0;
        // Barely above the activity floor: the unclamped desired gain
        // would be ~13×; the loop must stop at AGC_MAX_GAIN.
        for _ in 0..600 {
            let mut frame = sine_frame(430.0, &mut phase);
            dsp.process(&mut frame);
        }
        assert!(dsp.current_gain() <= AGC_MAX_GAIN + 1e-3);
        assert!(dsp.current_gain() > AGC_MAX_GAIN * 0.9);
    }

    #[test]
    fn agc_holds_gain_through_silence() {
        let mut dsp = Dsp::new(DspParams::new(false, true));
        let mut phase = 0.0;
        for _ in 0..200 {
            let mut frame = sine_frame(800.0, &mut phase);
            dsp.process(&mut frame);
        }
        let settled = dsp.current_gain();
        assert!(settled > 2.0, "gain should have risen well above unity");
        // A long pause must not crank the gain further (no noise-floor
        // pumping between sentences).
        for _ in 0..300 {
            let mut frame = vec![0i16; FRAME_SAMPLES];
            dsp.process(&mut frame);
        }
        assert!((dsp.current_gain() - settled).abs() < 1e-6);
    }

    #[test]
    fn agc_never_boosts_a_transient_into_clipping() {
        let mut dsp = Dsp::new(DspParams::new(false, true));
        let mut phase = 0.0;
        // Settle high gain on a quiet passage…
        for _ in 0..400 {
            let mut frame = sine_frame(800.0, &mut phase);
            dsp.process(&mut frame);
        }
        assert!(dsp.current_gain() > 4.0);
        // …then slam a near-full-scale transient through. The peak
        // limiter must cap the very first loud frame below the ceiling
        // instead of multiplying 30k samples by ~7×.
        let mut frame = sine_frame(30_000.0, &mut phase);
        dsp.process(&mut frame);
        let peak = frame.iter().map(|&s| (s as f32).abs()).fold(0.0, f32::max);
        assert!(
            peak <= PEAK_CEILING * 1.01,
            "post-AGC peak {peak} blew past the ceiling"
        );
    }

    #[test]
    fn noise_suppression_engages_and_reports_vad() {
        // We deliberately do NOT assert a suppression *ratio* here:
        // RNNoise's behaviour on synthetic input is a model property,
        // not a contract (measured: it treats pure white noise as ~94%
        // "voice" because flat spectra don't occur in real noise, and
        // attenuates it only mildly). What we pin instead is the
        // plumbing: the denoiser actually runs (output differs from
        // input), it doesn't blow the level up, and the published VAD
        // is a sane probability.
        let mut dsp = Dsp::new(DspParams::new(true, false));
        let mut rng = 0x1234_5678_u32;
        let mut changed = false;
        let mut in_rms = 0.0;
        let mut out_rms = 0.0;
        for _ in 0..50 {
            let mut frame = noise_frame(3_000.0, &mut rng);
            let original = frame.clone();
            in_rms = frame_rms(&frame);
            dsp.process(&mut frame);
            out_rms = frame_rms(&frame);
            changed |= frame != original;
        }
        assert!(changed, "denoiser never modified the signal");
        assert!(
            out_rms < in_rms * 1.5,
            "denoiser amplified noise: {in_rms} -> {out_rms}"
        );
        let vad = dsp.params.vad();
        assert!((0.0..=1.0).contains(&vad), "vad {vad} out of range");
    }

    #[test]
    fn noise_suppression_keeps_silence_silent() {
        let mut dsp = Dsp::new(DspParams::new(true, false));
        let mut out_rms = f32::MAX;
        for _ in 0..20 {
            let mut frame = vec![0i16; FRAME_SAMPLES];
            dsp.process(&mut frame);
            out_rms = frame_rms(&frame);
        }
        assert!(out_rms < 50.0, "digital silence came out at RMS {out_rms}");
    }
}
