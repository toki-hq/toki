//! Phosphor theme tokens — colors, sizes, typography for the Strip layout.
//!
//! Values come from `design/design-tokens.md`. Three other themes (Tactical,
//! Cyber, Stealth) are documented in the spec but only Phosphor ships
//! today; recoloring is a matter of swapping the `primary` / `primary_dim` /
//! `primary_ink` / `primary_glow` / `tx` / `tx_glow` block below.
//!
//! All hex values are the design doc's published approximations of the
//! source OKLCH coordinates — egui doesn't render OKLCH natively, and the
//! sRGB drift is within a couple of values on each channel.

use eframe::egui::Color32;

// ── Phosphor accents ────────────────────────────────────────────────────
/// `oklch(0.86 0.18 145)` — RX / idle / brand / frequency text.
pub const PRIMARY: Color32 = Color32::from_rgb(0x7f, 0xff, 0x90);
/// `oklch(0.55 0.14 145)` — secondary OLED labels, MHz unit, idle chip.
pub const PRIMARY_DIM: Color32 = Color32::from_rgb(0x3f, 0xa3, 0x58);
/// `oklch(0.32 0.10 145)` — inactive waveform, chevron borders, dividers.
pub const PRIMARY_INK: Color32 = Color32::from_rgb(0x1d, 0x5d, 0x2f);
/// Outer glow on text/icons — `primary` at 45% alpha.
pub const PRIMARY_GLOW: Color32 = Color32::from_rgba_premultiplied(0x39, 0x73, 0x41, 0x73);

/// `oklch(0.82 0.17 75)` — transmit indicator (amber).
pub const TX: Color32 = Color32::from_rgb(0xff, 0xba, 0x4d);
/// `tx` at 50% alpha — outer glow when TX.
pub const TX_GLOW: Color32 = Color32::from_rgba_premultiplied(0x80, 0x5d, 0x27, 0x80);

/// `oklch(0.68 0.22 25)` — busy/collision (red).
pub const WARN: Color32 = Color32::from_rgb(0xff, 0x5c, 0x5c);
/// `warn` at ~35% alpha — used as the *glow color* behind any red
/// surface. Sibling of `TX_GLOW` / `PRIMARY_GLOW`. Kept dimmer than
/// the others (35% vs 50%) because the eye picks red glows out of a
/// dark UI much more easily — at parity it reads as alarming.
pub const WARN_GLOW: Color32 = Color32::from_rgba_premultiplied(0x39, 0x14, 0x14, 0x5a);

// ── Hardware shell (theme-independent) ──────────────────────────────────
pub const SHELL: Color32 = Color32::from_rgb(0x0a, 0x0b, 0x0a);
pub const SHELL_TOP: Color32 = Color32::from_rgb(0x1a, 0x1c, 0x1e);
pub const SHELL_BOTTOM: Color32 = Color32::from_rgb(0x05, 0x05, 0x05);
pub const SHELL_EDGE: Color32 = Color32::from_rgb(0x2a, 0x2d, 0x30);

// ── Sound-drawer surfaces (from the design handoff) ──────────────────────
/// Drawer panel gradient (top → bottom): `#15171a` → `#0a0b0a`.
pub const DRAWER_TOP: Color32 = Color32::from_rgb(0x15, 0x17, 0x1a);
pub const DRAWER_BOTTOM: Color32 = Color32::from_rgb(0x0a, 0x0b, 0x0a);
/// Fader track + ticks (white at low alpha, premultiplied).
pub const FADER_TRACK: Color32 = Color32::from_rgba_premultiplied(0x0d, 0x0d, 0x0d, 0x0d);
pub const FADER_TICK: Color32 = Color32::from_rgba_premultiplied(0x1a, 0x1a, 0x1a, 0x1a);
/// The centre tick of a centre-origin (balance) fader — brighter so the
/// detent reads. `rgba(255,255,255,0.28)`.
pub const FADER_TICK_CENTER: Color32 = Color32::from_rgba_premultiplied(0x47, 0x47, 0x47, 0x47);
/// Fader thumb slug gradient + its knurl lines.
pub const FADER_THUMB_TOP: Color32 = Color32::from_rgb(0x34, 0x38, 0x3c);
pub const FADER_THUMB_BOTTOM: Color32 = Color32::from_rgb(0x0c, 0x0d, 0x0e);
pub const FADER_KNURL: Color32 = Color32::from_rgba_premultiplied(0x38, 0x38, 0x38, 0x38);

pub const OLED: Color32 = Color32::from_rgb(0x04, 0x06, 0x04);
pub const OLED_RIM: Color32 = Color32::from_rgb(0x0e, 0x12, 0x0e);

/// `oklch(0.78 0.02 150)` — primary UI text.
pub const INK: Color32 = Color32::from_rgb(0xbf, 0xc5, 0xbf);
/// `oklch(0.55 0.02 150)` — secondary text.
pub const INK_DIM: Color32 = Color32::from_rgb(0x87, 0x8c, 0x87);
/// `oklch(0.38 0.02 150)` — tertiary text / hints.
pub const INK_MUTE: Color32 = Color32::from_rgb(0x5d, 0x61, 0x5d);

/// Topbar divider, settings row borders.
pub const DIVIDER: Color32 = Color32::from_rgba_premultiplied(0x14, 0x14, 0x14, 0x14);

/// 1 px white-at-6% highlight along the chassis top edge.
pub const HIGHLIGHT: Color32 = Color32::from_rgba_premultiplied(0x0f, 0x0f, 0x0f, 0x0f);

// ── PTT button gradient stops (idle / tx / busy) ────────────────────────
// egui doesn't render gradients natively; we paint a solid mid-tone and
// rely on the inset border + highlight to give the impression of depth.
pub const PTT_IDLE_TOP: Color32 = Color32::from_rgb(0x23, 0x26, 0x27);
pub const PTT_IDLE_BOTTOM: Color32 = Color32::from_rgb(0x13, 0x15, 0x16);
pub const PTT_TX_TOP: Color32 = Color32::from_rgb(0x6b, 0x49, 0x1c);
pub const PTT_TX_BOTTOM: Color32 = Color32::from_rgb(0x3f, 0x2a, 0x10);
pub const PTT_BUSY_TOP: Color32 = Color32::from_rgb(0x6b, 0x1f, 0x1f);
pub const PTT_BUSY_BOTTOM: Color32 = Color32::from_rgb(0x3e, 0x12, 0x12);
// Muted: the button is inert (we can't transmit — operator member-mute
// or channel-mute). Darker and flatter than IDLE with a faint red cast,
// so it reads as "disabled, and not because you're busy".
pub const PTT_MUTED_TOP: Color32 = Color32::from_rgb(0x1a, 0x14, 0x15);
pub const PTT_MUTED_BOTTOM: Color32 = Color32::from_rgb(0x0e, 0x0a, 0x0b);

// ── Spacing scale (px) ──────────────────────────────────────────────────
pub const PAD_OUTER: f32 = 12.0;
pub const GAP_ROW: f32 = 8.0;
pub const OLED_PAD_X: f32 = 12.0;
pub const OLED_PAD_Y: f32 = 10.0;

// ── Radii ───────────────────────────────────────────────────────────────
pub const RADIUS_WIDGET: f32 = 16.0;
pub const RADIUS_OLED: f32 = RADIUS_WIDGET - 8.0; // 8
pub const RADIUS_PTT: f32 = RADIUS_WIDGET - 6.0; // 10
pub const RADIUS_BUTTON: f32 = 6.0;
pub const RADIUS_CHEVRON: f32 = 4.0;

// ── Dimensions ──────────────────────────────────────────────────────────
pub const WIDGET_W: f32 = 640.0;
/// Height of the radio *body* (the chassis with topbar/main/bottom rows).
/// The sound drawer is a sibling mounted below it; the full window height
/// is `WIDGET_H + DRAWER_GAP + DRAWER_HANDLE_H` when the drawer is folded
/// and additionally `+ DRAWER_BODY_H` when it's open (see [`window_h`]).
pub const WIDGET_H: f32 = 260.0;
pub const TOPBAR_H: f32 = 26.0;
pub const BOTTOM_H: f32 = 60.0;
pub const PTT_H: f32 = 56.0;
pub const OLED_LEFT_W: f32 = 200.0;
pub const ICON_BTN_D: f32 = 28.0;

// ── Sound drawer ────────────────────────────────────────────────────────
// A foldable panel below the radio body holding the VOL / BAL / MIC
// faders (replaces the old rotary knobs). Mirrors the design handoff:
// always-visible handle, collapsible body of three slider rows.
/// Vertical gap between the radio body and the drawer (the "unit" gap).
pub const DRAWER_GAP: f32 = 6.0;
/// Height of the always-visible handle (the fold/unfold button).
pub const DRAWER_HANDLE_H: f32 = 34.0;
/// Height of the collapsible body when open: three 20 px slider rows with
/// 12 px gaps plus the top border and inner padding (≈ the handoff's
/// measured 103 px open body, rounded up for the border + breathing room).
pub const DRAWER_BODY_H: f32 = 112.0;
/// Drawer corner radius: `max(6, widget_radius − 6)` per the handoff.
pub const DRAWER_RADIUS: f32 = RADIUS_WIDGET - 6.0;
/// One slider row: label (30) + fader (flex) + value readout (44).
pub const DRAWER_ROW_H: f32 = 20.0;
pub const DRAWER_LABEL_W: f32 = 30.0;
pub const DRAWER_VALUE_W: f32 = 44.0;
pub const DRAWER_PAD_X: f32 = 14.0;
/// Fader (HSlider) geometry: thin track + knurled thumb. The hit-area
/// height is the row height (`DRAWER_ROW_H`).
pub const FADER_TRACK_H: f32 = 4.0;
pub const FADER_THUMB_W: f32 = 12.0;
pub const FADER_THUMB_H: f32 = 18.0;

/// Total window height for the current drawer fold state. Folded shows
/// just the handle below the body; open adds the slider body. eframe is
/// driven to this size via `ViewportCommand::InnerSize` when the state
/// flips (see `TokiApp::update`).
pub fn window_h(drawer_open: bool) -> f32 {
    let base = WIDGET_H + DRAWER_GAP + DRAWER_HANDLE_H;
    if drawer_open {
        base + DRAWER_BODY_H
    } else {
        base
    }
}

// ── Constants from the spec ─────────────────────────────────────────────
pub const TX_LIMIT_MS: u32 = 30_000;
/// Number of bars in the center-OLED spectrum histogram. 32 reads as
/// "chunky and radio-y" at the panel's typical 260–360 px width; 64
/// looked busy. The FFT window is 4× wider (128 bins of useful
/// spectrum), so each bar averages 4 consecutive bins.
pub const SPECTRUM_BARS: usize = 32;
/// Number of input samples per FFT. A power of two so rustfft picks
/// the cheap radix-2 plan. At 48 kHz this covers ~5.3 ms of audio
/// per analysis frame — plenty of detail for a visualizer that
/// updates at ~30 Hz.
pub const SPECTRUM_FFT_LEN: usize = 256;

// ── Frequency band ──────────────────────────────────────────────────────
// Toki uses the PMR446-adjacent 446.00–448.00 MHz band, with 0.05 MHz
// (50 kHz) channel spacing — 41 distinct channels. Each frequency maps
// to its own logical room on the server; the chevrons in the UI cycle
// between them and send `ChangeFrequency` to the server.
/// How long the user must stay on a frequency (no further chevron
/// clicks) before we actually join the room on the server. Mirrors
/// how a real walkie-talkie's user interface "settles" before
/// committing — fast scans through nearby frequencies don't generate
/// a join-leave storm on the server.
pub const FREQ_DEBOUNCE: std::time::Duration = std::time::Duration::from_millis(450);

pub const FREQ_MIN_MHZ: f32 = 446.00;
/// Upper band edge — documented for clarity; the runtime never compares
/// against this directly (we use `FREQ_CHANNEL_COUNT` to bound the
/// selector). Kept here so the relationship `MIN + (COUNT-1)*STEP = MAX`
/// is obvious to anyone reading the module.
#[allow(dead_code)]
pub const FREQ_MAX_MHZ: f32 = 448.00;
pub const FREQ_STEP_MHZ: f32 = 0.05;
pub const FREQ_CHANNEL_COUNT: usize = 41; // (max - min) / step + 1

/// MHz value of channel `idx` in `[0, FREQ_CHANNEL_COUNT)`. Clamps
/// out-of-range indices to the nearest endpoint to be defensive — the
/// UI should never produce one, but config files might.
pub fn frequency_of(idx: usize) -> f32 {
    let i = idx.min(FREQ_CHANNEL_COUNT - 1);
    FREQ_MIN_MHZ + i as f32 * FREQ_STEP_MHZ
}

/// Inverse of [`frequency_of`]: given a label like `"446.05"`, return
/// the channel index. Used to seed the UI from saved config.
pub fn channel_of_label(s: &str) -> Option<usize> {
    let f: f32 = s.parse().ok()?;
    let i = ((f - FREQ_MIN_MHZ) / FREQ_STEP_MHZ).round() as i32;
    if (0..FREQ_CHANNEL_COUNT as i32).contains(&i) {
        Some(i as usize)
    } else {
        None
    }
}

/// Canonical wire string for a frequency, e.g. `"446.05"`. We always
/// use 2 decimals — the band's step is 50 kHz, so any extra precision
/// would be spurious.
pub fn frequency_label(freq_mhz: f32) -> String {
    format!("{freq_mhz:.2}")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn frequency_of_endpoints() {
        assert!((frequency_of(0) - FREQ_MIN_MHZ).abs() < 1e-3);
        assert!((frequency_of(FREQ_CHANNEL_COUNT - 1) - FREQ_MAX_MHZ).abs() < 1e-3);
    }

    #[test]
    fn frequency_of_clamps_overflow() {
        // The UI shouldn't produce an out-of-range index, but
        // hand-edited configs might — defensive clamp prevents a
        // panic at startup.
        let last = frequency_of(FREQ_CHANNEL_COUNT - 1);
        let overflow = frequency_of(FREQ_CHANNEL_COUNT + 100);
        assert!((overflow - last).abs() < 1e-3);
    }

    #[test]
    fn channel_of_label_round_trips_with_frequency_of() {
        for idx in 0..FREQ_CHANNEL_COUNT {
            let label = frequency_label(frequency_of(idx));
            let parsed = channel_of_label(&label).expect("label should parse back");
            assert_eq!(parsed, idx, "channel {idx} did not round-trip");
        }
    }

    #[test]
    fn channel_of_label_rejects_out_of_band() {
        assert!(channel_of_label("445.50").is_none());
        assert!(channel_of_label("448.50").is_none());
        assert!(channel_of_label("not-a-number").is_none());
    }
}
