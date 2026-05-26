//! The Strip widget — landscape walkie-talkie UI per `design/`.
//!
//! Architecture: the runtime owns network + audio state; the GUI reads
//! a snapshot each frame and paints accordingly. All custom widgets
//! (waveform, knob, PTT button, OLED panels) are painted directly via
//! `Painter` rather than composed from egui's built-in widgets — the
//! design has too much custom chrome (glows, scanlines, gradients) to
//! be expressible through the default widget set.

use std::collections::VecDeque;
use std::time::{Duration, Instant};

use eframe::egui::{
    self, Align2, Color32, CornerRadius, FontFamily, FontId, Pos2, Rect, Sense, Shape, Stroke,
    StrokeKind, Vec2,
};

use crate::audio::{self, AudioControl, AudioDevices, AudioGains};
use crate::config::{self, HotkeyConfig};
use crate::hotkey::{self, InstalledHotkey};
use crate::runtime::{self, Cmd};
use crate::state::{self, ConnState, SharedState};
use crate::theme as T;

/// Logical UI state derived from the runtime snapshot + local hold flag.
/// Mirrors the four states in `design/behavior-spec.md`.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum RadioState {
    Idle,
    Tx,
    Rx,
    Busy,
}

pub struct TokiApp {
    state: SharedState,
    cmd_tx: tokio::sync::mpsc::UnboundedSender<Cmd>,

    config: config::Config,
    hotkey: InstalledHotkey,
    recording: bool,

    audio_devices: AudioDevices,
    audio_control: AudioControl,
    audio_gains: AudioGains,

    // ── UI-only state ───────────────────────────────────────────────
    /// True when the user is holding either the PTT key/button or
    /// click-and-holding the on-screen button. Used to detect `busy`
    /// (PTT pressed while another holder is active).
    ptt_held: bool,
    /// `Some(t)` while in TX — tracks the 30-second cap.
    tx_start: Option<Instant>,
    /// Settings overlay open?
    show_settings: bool,
    /// Mute toggle (output gate; separate from volume so unmuting
    /// restores the previous gain).
    muted: bool,
    /// Pre-mute output gain, so toggling mute round-trips cleanly.
    gain_before_mute: f32,
    /// Display-only channel selection. Toki's wire protocol is single-
    /// room — the chevron buttons cycle which of the 8 design channels
    /// is shown, but every client still joins the same backend room.
    channel_idx: usize,
    /// 64-sample scrolling buffer for the waveform.
    waveform: VecDeque<f32>,
    /// Last frame time, for animation pacing independent of repaint rate.
    last_tick: Instant,
    /// Counter for the synthesized waveform's phase modulation.
    wave_phase: f32,
}

impl TokiApp {
    pub fn new() -> Self {
        let state = state::shared();
        let config = config::Config::load();

        let audio_handle = audio::spawn(
            config.audio.input_device.clone(),
            config.audio.output_device.clone(),
            config.audio.input_gain,
            config.audio.output_gain,
        )
        .expect("audio init failed");
        let audio::AudioHandle {
            mic_rx,
            playback,
            devices,
            control,
            gains,
        } = audio_handle;

        let cmd_tx = runtime::spawn(state.clone(), mic_rx, playback);

        let initial = config.hotkey.to_input().or_else(|| {
            tracing::warn!(
                "no parseable PTT input in config, using default ({:?})",
                hotkey::DEFAULT_KEY
            );
            Some(hotkey::Input::Key(hotkey::DEFAULT_KEY))
        });
        let installed = hotkey::install(cmd_tx.clone(), initial);

        // Seed the channel index from the saved frequency. If the
        // string is bogus, fall back to the middle of the band.
        let channel_idx = T::channel_of_label(&config.connection.frequency)
            .unwrap_or(T::FREQ_CHANNEL_COUNT / 2);
        // Normalize the saved frequency label in case it had drift,
        // so the wire string and the displayed value agree.
        let frequency = T::frequency_label(T::frequency_of(channel_idx));

        // Auto-connect on launch using the saved server/name. The user
        // expects "walkie-talkies stay on" — Toki should be live as
        // soon as the window opens, not require a Connect click first.
        let server = config.connection.server.trim().to_string();
        let display_name = config.connection.display_name.trim().to_string();
        if !server.is_empty() && !display_name.is_empty() {
            let _ = cmd_tx.send(Cmd::Connect {
                server,
                display_name,
                frequency: frequency.clone(),
            });
        }

        Self {
            state,
            cmd_tx,
            config,
            hotkey: installed,
            recording: false,
            audio_devices: devices,
            audio_control: control,
            audio_gains: gains,
            ptt_held: false,
            tx_start: None,
            show_settings: false,
            muted: false,
            gain_before_mute: 1.0,
            channel_idx,
            waveform: VecDeque::from(vec![0.0; T::WAVEFORM_SAMPLES]),
            last_tick: Instant::now(),
            wave_phase: 0.0,
        }
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
            log_tail: s.log.iter().next_back().cloned().unwrap_or_default(),
        }
    }

    fn radio_state(&self, snap: &StateSnapshot) -> RadioState {
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

    /// Advance the synthesized waveform one frame's worth of samples
    /// based on the current radio state. Both TX and RX animate; idle
    /// stays flat.
    fn tick_waveform(&mut self, st: RadioState) {
        let now = Instant::now();
        let dt = now.duration_since(self.last_tick).as_secs_f32().min(0.1);
        self.last_tick = now;
        self.wave_phase += dt * 6.0;
        let active = matches!(st, RadioState::Tx | RadioState::Rx);
        // Synthesize an RMS-ish envelope. When we plumb real mic/playback
        // levels through the runtime we can replace this with measured
        // values. For now it's enough to give the panel life when active.
        let target = if active {
            let env = 0.55 + 0.35 * (self.wave_phase * 0.21).sin() * (self.wave_phase * 0.13).cos();
            let grain =
                0.5 + 0.5 * (self.wave_phase * 4.1).sin() * (self.wave_phase * 2.9).cos();
            (env.abs() * (0.55 + 0.45 * grain.abs())).clamp(0.0, 1.0)
        } else {
            0.0
        };
        self.waveform.pop_front();
        self.waveform.push_back(target);
    }

    fn ensure_min_one_frame(&self, ctx: &egui::Context) {
        // 33 ms ≈ 30 fps — fast enough that waveform scrolls smoothly
        // and the TX countdown ticks visibly.
        ctx.request_repaint_after(Duration::from_millis(33));
    }

    /// Count of members on the current frequency (us + others), read
    /// directly from the shared state. Used for the activity light.
    fn snapshot_members_count(&self) -> usize {
        self.state.lock().unwrap().members.len()
    }

    /// Persist the new channel selection to config and tell the
    /// runtime to ChangeFrequency on the server.
    fn send_frequency_change(&mut self) {
        let freq = T::frequency_label(T::frequency_of(self.channel_idx));
        self.config.connection.frequency = freq.clone();
        self.config.save();
        let _ = self.cmd_tx.send(Cmd::ChangeFrequency(freq));
    }
}

#[derive(Default)]
struct StateSnapshot {
    connection: ConnState,
    holder: Option<String>,
    holder_name: String,
    is_transmitting: bool,
    #[allow(dead_code)]
    log_tail: String,
}

// ════════════════════════════════════════════════════════════════════════
// Painting helpers
// ════════════════════════════════════════════════════════════════════════

fn font_mono(size: f32) -> FontId {
    FontId::new(size, FontFamily::Monospace)
}

/// One row in the settings overlay: fixed-width label + arbitrary
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
        Stroke::new(1.0, Color32::from_rgba_premultiplied(0x06, 0x06, 0x06, 0x0a)),
    );
    ui.add_space(2.0);
}

fn font_ui(size: f32, _weight_600: bool) -> FontId {
    // egui doesn't expose weight per-font without explicit registration;
    // the default Ubuntu-Light is treated as our "Geist" until/unless
    // we embed real font files. We accept the visual fidelity gap.
    FontId::new(size, FontFamily::Proportional)
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
            [Pos2::new(rect.left() + radius, y), Pos2::new(rect.right() - radius, y)],
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
            [Pos2::new(rect.left() + 2.0, y), Pos2::new(rect.right() - 2.0, y)],
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

        let snap = self.snapshot();
        let st = self.radio_state(&snap);
        self.tick_waveform(st);

        // ── Recording: poll the rdev/device_query listener ──────────
        if self.recording {
            if let Some(input) = self.hotkey.take_recorded() {
                if let Err(e) = self.hotkey.rebind(input) {
                    tracing::warn!(error = %e, "rebind failed");
                } else {
                    self.config.hotkey = HotkeyConfig::from_input(input);
                    self.config.save();
                }
                self.recording = false;
            }
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

        let central = egui::CentralPanel::default().frame(
            egui::Frame::NONE.fill(Color32::TRANSPARENT),
        );
        central.show(ctx, |ui| {
            self.paint_strip(ui, &snap, st);
        });
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
            &[
                (0.0, T::SHELL_TOP),
                (0.5, T::SHELL),
                (1.0, T::SHELL_BOTTOM),
            ],
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
        let topbar_rect = Rect::from_min_size(
            inner.min,
            Vec2::new(inner.width(), T::TOPBAR_H),
        );
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

        if self.show_settings {
            self.paint_settings_overlay(ui, rect);
        }
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
            [Pos2::new(divider_x, y_mid - 7.0), Pos2::new(divider_x, y_mid + 7.0)],
            Stroke::new(1.0, T::DIVIDER),
        );

        // Callsign / connection-state label.
        let callsign = match &snap.connection {
            ConnState::Connected => self.config.connection.display_name.to_uppercase(),
            ConnState::Connecting => "CONNECTING…".into(),
            ConnState::Disconnected => "OFFLINE".into(),
            ConnState::Failed(_) => "FAILED".into(),
        };
        painter.text(
            Pos2::new(divider_x + 10.0, y_mid),
            Align2::LEFT_CENTER,
            callsign,
            font_mono(10.0),
            T::INK_DIM,
        );

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

        // Status chip: dot + label.
        let (chip_color, chip_label, chip_glow) = match st {
            RadioState::Tx => (T::TX, "TX", 1.2),
            RadioState::Rx => (T::PRIMARY, "RX", 1.2),
            RadioState::Busy => (T::WARN, "BUSY", 1.0),
            RadioState::Idle => (T::PRIMARY_DIM, "IDLE", 0.3),
        };
        // Label first (we draw right-to-left): place label, then dot.
        let label_w = chip_label.len() as f32 * 6.5 + 8.0;
        x -= label_w;
        painter.text(
            Pos2::new(x, y_mid),
            Align2::LEFT_CENTER,
            chip_label,
            font_mono(10.0),
            T::INK_DIM,
        );
        x -= 12.0;
        glow_dot(painter, Pos2::new(x, y_mid), 3.0, chip_color, chip_glow);
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
            if activity { T::PRIMARY_DIM } else { T::INK_MUTE },
        );

        // ── Frequency readout ──────────────────────────────────────
        // Now the only text on this panel, so we let it dominate:
        // bigger font and centered both horizontally and vertically
        // between the top edge and the chevron row. Width-fit via
        // `egui::Painter::layout_no_wrap` so we know the true glyph
        // advance and don't rely on a fudged "mono char width" guess.
        let freq = T::frequency_of(self.channel_idx);
        let freq_text = T::frequency_label(freq);
        let active_color = if matches!(st, RadioState::Tx) { T::TX } else { T::PRIMARY };
        let active_glow = if matches!(st, RadioState::Tx) { T::TX_GLOW } else { T::PRIMARY_GLOW };

        // Available horizontal space between the panel pads, minus a
        // bit of room for the " MHz" suffix.
        let available_w = rect.width() - 2.0 * pad_x;
        // Try a generous size; if the layout actually overflows we
        // step down. With the band's 3.2-digit numbers (e.g. "447.05")
        // 38 px fits the 200-px-wide regular OLED comfortably.
        let mut font_size = 38.0_f32;
        let unit_font = font_mono(12.0);
        let unit_galley = painter.layout_no_wrap(
            "MHz".to_string(),
            unit_font.clone(),
            T::PRIMARY_DIM,
        );
        let unit_advance = unit_galley.size().x + 6.0; // gap to digits
        loop {
            let g = painter.layout_no_wrap(
                freq_text.clone(),
                font_mono(font_size),
                active_color,
            );
            if g.size().x + unit_advance <= available_w || font_size <= 22.0 {
                break;
            }
            font_size -= 1.0;
        }
        let freq_font = font_mono(font_size);
        let freq_galley = painter.layout_no_wrap(
            freq_text.clone(),
            freq_font.clone(),
            active_color,
        );
        let block_w = freq_galley.size().x + unit_advance;
        // Vertically center between the top edge (after activity-dot
        // row) and the chevron row (≈ bottom edge minus 18 px).
        let band_top = rect.top() + pad_y + 14.0;
        let band_bot = rect.bottom() - pad_y - 22.0;
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
            1.0,
        );
        // "MHz" baseline-aligned to the digits. The digits' baseline
        // sits roughly at (center + font_size * 0.30) for a mono font;
        // good enough that the suffix tracks the readout cleanly.
        let baseline_y = center_y + font_size * 0.30;
        painter.text(
            Pos2::new(freq_left + freq_galley.size().x + 6.0, baseline_y),
            Align2::LEFT_BOTTOM,
            "MHz",
            unit_font,
            T::PRIMARY_DIM,
        );

        // ── Chevron row (no label between them) ────────────────────
        let bottom_y = rect.bottom() - pad_y - 16.0;
        let chev_w = 56.0;
        let chev_h = 28.0;
        let left_chev = Rect::from_min_size(
            Pos2::new(rect.left() + pad_x, bottom_y - chev_h / 2.0),
            Vec2::new(chev_w, chev_h),
        );
        let right_chev = Rect::from_min_size(
            Pos2::new(rect.right() - pad_x - chev_w, bottom_y - chev_h / 2.0),
            Vec2::new(chev_w, chev_h),
        );

        // Chevron clicks switch channels. We disable cycling during TX
        // (you can't change frequency mid-transmission — the design
        // spec calls this out as a hard constraint). Changing in RX
        // is allowed: you simply leave the current peer's room.
        let can_switch = !matches!(st, RadioState::Tx);
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
            self.send_frequency_change();
        }
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

    fn chevron(
        &self,
        ui: &mut egui::Ui,
        painter: &egui::Painter,
        rect: Rect,
        glyph: &str,
    ) -> bool {
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
                    .map(|s| {
                        T::TX_LIMIT_MS as f32 / 1000.0 - s.elapsed().as_secs_f32()
                    })
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

    fn paint_waveform(&self, painter: &egui::Painter, rect: Rect, st: RadioState) {
        let active = matches!(st, RadioState::Tx | RadioState::Rx);
        let color = match st {
            RadioState::Tx => T::TX,
            _ if active => T::PRIMARY,
            _ => T::PRIMARY_INK,
        };
        let alpha = if active { 240 } else { 110 };
        let fill = Color32::from_rgba_unmultiplied(color.r(), color.g(), color.b(), alpha);

        // Dashed center axis.
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

        // Build top edge (left→right) and bottom edge (right→left) for
        // a single closed mirrored shape — one ConvexPolygon draw + a
        // soft halo behind it for the "glow".
        let n = self.waveform.len();
        if n < 2 {
            return;
        }
        let step = rect.width() / (n - 1) as f32;
        let half_h = rect.height() / 2.0;
        let mut top_pts: Vec<Pos2> = Vec::with_capacity(n);
        let mut bot_pts: Vec<Pos2> = Vec::with_capacity(n);
        for (i, &v) in self.waveform.iter().enumerate() {
            let amp = (v * (half_h - 2.0)).max(2.0);
            let x = rect.left() + i as f32 * step;
            top_pts.push(Pos2::new(x, mid_y - amp));
            bot_pts.push(Pos2::new(x, mid_y + amp));
        }
        bot_pts.reverse();
        let mut polygon: Vec<Pos2> = Vec::with_capacity(n * 2);
        polygon.extend(top_pts);
        polygon.extend(bot_pts);

        if active {
            // Halo layer (a slightly bloated version, low alpha).
            let halo = Color32::from_rgba_unmultiplied(color.r(), color.g(), color.b(), 50);
            let halo_pts: Vec<Pos2> = polygon
                .iter()
                .map(|p| {
                    if p.y < mid_y {
                        Pos2::new(p.x, p.y - 2.0)
                    } else {
                        Pos2::new(p.x, p.y + 2.0)
                    }
                })
                .collect();
            painter.add(Shape::convex_polygon(halo_pts, halo, Stroke::NONE));
        }
        painter.add(Shape::convex_polygon(polygon, fill, Stroke::NONE));
    }

    // ── Bottom row: knob + PTT ─────────────────────────────────────
    fn paint_bottom(
        &mut self,
        ui: &mut egui::Ui,
        painter: &egui::Painter,
        rect: Rect,
        st: RadioState,
    ) {
        // Knob on the left, vertical-centered.
        let knob_rect = Rect::from_center_size(
            Pos2::new(rect.left() + T::KNOB_D / 2.0 + 4.0, rect.center().y - 4.0),
            Vec2::splat(T::KNOB_D),
        );
        self.paint_knob(ui, painter, knob_rect);
        // "VOL" caption.
        painter.text(
            Pos2::new(knob_rect.center().x, knob_rect.bottom() + 8.0),
            Align2::CENTER_CENTER,
            "VOL",
            font_mono(8.0),
            T::INK_MUTE,
        );

        // PTT — fills the rest of the row.
        let ptt_x = knob_rect.right() + T::GAP_BOTTOM;
        let ptt_rect = Rect::from_min_size(
            Pos2::new(ptt_x, rect.center().y - T::PTT_H / 2.0),
            Vec2::new(rect.right() - ptt_x, T::PTT_H),
        );
        self.paint_ptt(ui, painter, ptt_rect, st);
    }

    fn paint_knob(&mut self, ui: &mut egui::Ui, painter: &egui::Painter, rect: Rect) {
        let resp = ui.allocate_rect(rect, Sense::click_and_drag());

        // Click-drag adjusts the value relative to the angle change
        // around the knob center. Forgiving sweep (1.4π = full range).
        if resp.dragged() {
            if let Some(pos) = ui.input(|i| i.pointer.interact_pos()) {
                if let Some(prev) = ui.input(|i| i.pointer.press_origin()) {
                    let cx = rect.center().x;
                    let cy = rect.center().y;
                    let a_now = (pos.y - cy).atan2(pos.x - cx);
                    let a_prev = (prev.y - cy).atan2(prev.x - cx);
                    let mut delta = a_now - a_prev;
                    if delta > std::f32::consts::PI {
                        delta -= std::f32::consts::TAU;
                    } else if delta < -std::f32::consts::PI {
                        delta += std::f32::consts::TAU;
                    }
                    // Apply only the per-frame increment so the knob
                    // doesn't snap on each drag start.
                    let increment = delta / (std::f32::consts::PI * 1.4);
                    let new_val =
                        (self.config.audio.output_gain / 2.0 + increment).clamp(0.0, 1.0);
                    let gain = new_val * 2.0;
                    self.config.audio.output_gain = gain;
                    if !self.muted {
                        self.audio_gains.set_output(gain);
                    }
                }
            }
        }
        if resp.drag_stopped() {
            self.config.save();
        }

        // Map output gain [0..2] → display value [0..1] for the knob.
        let v = (self.config.audio.output_gain / 2.0).clamp(0.0, 1.0);
        let angle_deg = -135.0 + if self.muted { 0.0 } else { v } * 270.0;
        let angle = angle_deg.to_radians();

        // Outer disc.
        painter.circle_filled(rect.center(), T::KNOB_D / 2.0, T::SHELL_EDGE);
        painter.circle_stroke(rect.center(), T::KNOB_D / 2.0, Stroke::new(1.0, T::SHELL_BOTTOM));
        // Inner disc.
        painter.circle_filled(rect.center(), T::KNOB_D / 2.0 - 4.0, T::SHELL_TOP);

        // Indicator line at top of inner disc, rotated to angle.
        let r_outer = T::KNOB_D / 2.0 - 6.0;
        let r_inner = T::KNOB_D / 2.0 - 13.0;
        let ind_color = if self.muted { T::INK_MUTE } else { T::PRIMARY };
        let cx = rect.center().x;
        let cy = rect.center().y;
        // The "top" before rotation is -π/2; angle is rotation from that.
        let theta = angle - std::f32::consts::FRAC_PI_2;
        let p_out = Pos2::new(cx + theta.cos() * r_outer, cy + theta.sin() * r_outer);
        let p_in = Pos2::new(cx + theta.cos() * r_inner, cy + theta.sin() * r_inner);
        if !self.muted {
            painter.line_segment(
                [p_out, p_in],
                Stroke::new(3.5, Color32::from_rgba_unmultiplied(
                    ind_color.r(), ind_color.g(), ind_color.b(), 70,
                )),
            );
        }
        painter.line_segment([p_out, p_in], Stroke::new(2.0, ind_color));

        // 11 tick marks every 27° from -135° to +135°.
        for i in 0..11 {
            let a = (-135.0 + i as f32 * 27.0).to_radians();
            let r1 = T::KNOB_D / 2.0 + 2.0;
            let r2 = if i % 5 == 0 { T::KNOB_D / 2.0 + 5.5 } else { T::KNOB_D / 2.0 + 4.0 };
            let lit = !self.muted && (i as f32 / 10.0) <= v + 0.04;
            let color = if lit {
                T::PRIMARY
            } else {
                Color32::from_rgba_premultiplied(0x1e, 0x1e, 0x1e, 0x1e)
            };
            let p1 = Pos2::new(cx + a.cos() * r1, cy + a.sin() * r1);
            let p2 = Pos2::new(cx + a.cos() * r2, cy + a.sin() * r2);
            painter.line_segment(
                [p1, p2],
                Stroke::new(if i % 5 == 0 { 1.6 } else { 1.0 }, color),
            );
        }
    }

    fn paint_ptt(&mut self, ui: &mut egui::Ui, painter: &egui::Painter, rect: Rect, st: RadioState) {
        let connected = matches!(self.snapshot().connection, ConnState::Connected);
        let sense = if connected { Sense::click_and_drag() } else { Sense::hover() };
        let resp = ui.allocate_rect(rect, sense);

        // Click-and-hold semantics: we send PttDown/Up on edge
        // transitions of `ptt_held`. The on-screen button is always
        // clickable when connected — pressing during RX intentionally
        // triggers the `busy` UI state (the server still denies the
        // request, so no audio leaks).
        if connected {
            let was_down = self.ptt_held;
            let is_down = resp.is_pointer_button_down_on();
            if was_down && !ui.input(|i| i.pointer.any_down()) {
                // No pointer anywhere — release.
                self.ptt_held = false;
                let _ = self.cmd_tx.send(Cmd::PttUp);
            } else if is_down != was_down {
                self.ptt_held = is_down;
                let _ = self
                    .cmd_tx
                    .send(if is_down { Cmd::PttDown } else { Cmd::PttUp });
            }
        }

        // Visuals per state.
        let (top, bottom, label, label_color, dot_color, border, glow_intensity) = match st {
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
        glow_dot(painter, Pos2::new(label_x, label_y), 4.5, dot_color, glow_intensity);
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

    // ── Settings overlay ───────────────────────────────────────────
    fn paint_settings_overlay(&mut self, ui: &mut egui::Ui, outer_rect: Rect) {
        let pad = T::PAD_OUTER;
        let rect = Rect::from_min_max(
            Pos2::new(outer_rect.left() + pad, outer_rect.top() + 40.0),
            Pos2::new(outer_rect.right() - pad, outer_rect.bottom() - pad),
        );
        let painter = ui.painter().clone();
        painter.rect(
            rect,
            CornerRadius::same((T::RADIUS_WIDGET - 6.0) as u8),
            Color32::from_rgba_unmultiplied(0x0a, 0x0c, 0x0a, 0xf7),
            Stroke::new(1.0, T::PRIMARY_INK),
            StrokeKind::Inside,
        );

        let inner = rect.shrink(12.0);
        let mut content_ui = ui.new_child(
            egui::UiBuilder::new()
                .max_rect(inner)
                .layout(egui::Layout::top_down(egui::Align::Min)),
        );
        content_ui.set_clip_rect(inner);
        content_ui.style_mut().visuals.override_text_color = Some(T::INK);

        // Header.
        content_ui.horizontal(|ui| {
            ui.label(egui::RichText::new("· SETTINGS ·")
                .color(T::PRIMARY)
                .monospace()
                .size(10.0));
            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                if ui.button("✕").clicked() {
                    self.show_settings = false;
                }
            });
        });
        content_ui.add_space(6.0);

        // ── Connection ──────────────────────────────────────────
        settings_row(&mut content_ui, "SERVER", |ui| {
            let resp = ui.add(
                egui::TextEdit::singleline(&mut self.config.connection.server)
                    .desired_width(200.0)
                    .font(egui::TextStyle::Monospace),
            );
            if resp.lost_focus() {
                self.config.save();
            }
        });
        settings_row(&mut content_ui, "CALLSIGN", |ui| {
            let mut s = self.config.connection.display_name.clone();
            let resp = ui.add(
                egui::TextEdit::singleline(&mut s)
                    .desired_width(160.0)
                    .font(egui::TextStyle::Monospace),
            );
            // Uppercase + cap at 10 chars per the spec.
            s = s.to_uppercase();
            if s.len() > 10 {
                s.truncate(10);
            }
            self.config.connection.display_name = s;
            if resp.lost_focus() {
                self.config.save();
            }
        });

        if !matches!(self.state.lock().unwrap().connection, ConnState::Connected) {
            settings_row(&mut content_ui, "", |ui| {
                if ui.button("CONNECT").clicked() {
                    let frequency =
                        T::frequency_label(T::frequency_of(self.channel_idx));
                    let _ = self.cmd_tx.send(Cmd::Connect {
                        server: self.config.connection.server.trim().to_string(),
                        display_name: self.config.connection.display_name.trim().to_string(),
                        frequency,
                    });
                }
            });
        } else {
            settings_row(&mut content_ui, "", |ui| {
                if ui.button("DISCONNECT").clicked() {
                    let _ = self.cmd_tx.send(Cmd::Disconnect);
                }
            });
        }

        // ── PTT ─────────────────────────────────────────────────
        settings_row(&mut content_ui, "PTT", |ui| {
            if self.recording {
                let progress = self.hotkey.hold_progress();
                ui.colored_label(T::TX, "hold a key for 1s…");
                // Slim progress bar — fills as the user holds. Resets
                // to 0 the moment they release before the 1 s threshold.
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
                let label = self
                    .config
                    .hotkey
                    .to_input()
                    .map(hotkey::format)
                    .unwrap_or_else(|| "(none)".into());
                ui.monospace(label);
                if ui.button("BIND").clicked() && self.hotkey.start_recording() {
                    self.recording = true;
                }
            }
        });

        // ── Audio devices ───────────────────────────────────────
        settings_row(&mut content_ui, "INPUT", |ui| {
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
                self.audio_control.set_input(self.config.audio.input_device.clone());
                self.config.save();
            }
        });
        settings_row(&mut content_ui, "OUTPUT", |ui| {
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
                self.audio_control.set_output(self.config.audio.output_device.clone());
                self.config.save();
            }
        });

        // ── Volume / gains ──────────────────────────────────────
        settings_row(&mut content_ui, "MIC GAIN", |ui| {
            let mut g = self.config.audio.input_gain;
            let resp = ui.add(egui::Slider::new(&mut g, 0.0..=2.0).show_value(false));
            ui.monospace(format!("{:>3.0}%", g * 100.0));
            if resp.changed() {
                self.config.audio.input_gain = g;
                self.audio_gains.set_input(g);
            }
            if resp.drag_stopped() || resp.lost_focus() {
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
