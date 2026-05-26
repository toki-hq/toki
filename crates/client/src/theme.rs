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

// ── Hardware shell (theme-independent) ──────────────────────────────────
pub const SHELL: Color32 = Color32::from_rgb(0x0a, 0x0b, 0x0a);
pub const SHELL_TOP: Color32 = Color32::from_rgb(0x1a, 0x1c, 0x1e);
pub const SHELL_BOTTOM: Color32 = Color32::from_rgb(0x05, 0x05, 0x05);
pub const SHELL_EDGE: Color32 = Color32::from_rgb(0x2a, 0x2d, 0x30);

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

// ── Spacing scale (px) ──────────────────────────────────────────────────
pub const PAD_OUTER: f32 = 12.0;
pub const GAP_ROW: f32 = 8.0;
pub const GAP_BOTTOM: f32 = 12.0; // between knob and PTT
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
pub const WIDGET_H: f32 = 260.0;
pub const TOPBAR_H: f32 = 26.0;
pub const BOTTOM_H: f32 = 60.0;
pub const PTT_H: f32 = 56.0;
pub const KNOB_D: f32 = 42.0;
pub const OLED_LEFT_W: f32 = 200.0;
pub const ICON_BTN_D: f32 = 28.0;

// ── Constants from the spec ─────────────────────────────────────────────
pub const TX_LIMIT_MS: u32 = 30_000;
pub const WAVEFORM_SAMPLES: usize = 64;

/// 8 fixed display channels (see `design-tokens.md`). The proto layer
/// dropped channel addressing — every Toki client joins the same single
/// room — so the channel selector here is purely cosmetic for v1:
/// chevrons cycle the displayed channel without affecting routing.
pub struct ChannelDisplay {
    pub freq: f32,
    pub name: &'static str,
}

pub const CHANNELS: [ChannelDisplay; 8] = [
    ChannelDisplay { freq: 462.5625, name: "GENERAL" },
    ChannelDisplay { freq: 462.5875, name: "DESIGN" },
    ChannelDisplay { freq: 462.6125, name: "ENGINEER" },
    ChannelDisplay { freq: 462.6375, name: "OPS" },
    ChannelDisplay { freq: 462.6625, name: "FIELD-1" },
    ChannelDisplay { freq: 462.6875, name: "FIELD-2" },
    ChannelDisplay { freq: 462.7125, name: "STANDBY" },
    ChannelDisplay { freq: 467.5625, name: "EMERGENCY" },
];
