use std::collections::HashMap;
use std::time::Duration;

use eframe::egui;
use tokio::sync::mpsc::UnboundedSender;

use crate::audio::{self, AudioControl, AudioDevices, AudioGains};
use crate::config::{self, HotkeyConfig};
use crate::hotkey::{self, InstalledHotkey};
use crate::runtime::{self, Cmd};
use crate::state::{self, ConnState, SharedState};

pub struct TokiApp {
    state: SharedState,
    cmd_tx: UnboundedSender<Cmd>,

    // form inputs
    server: String,
    display_name: String,

    ptt_held: bool,

    // Persisted user preferences (hotkey + audio device selection).
    config: config::Config,

    // Global PTT input state. Always present (the struct's lazy-spawn
    // logic handles the "unavailable" case internally) — UI greys out
    // the bind affordance when `hotkey.available()` is false.
    hotkey: InstalledHotkey,

    // True while Settings is waiting for the user to press any
    // key/button to bind. Polled each frame against the listener's
    // captured-input slot.
    recording: bool,

    // Snapshot of cpal devices at startup. We don't auto-refresh.
    audio_devices: AudioDevices,
    // Sender to the audio thread for hot-swap.
    audio_control: AudioControl,
    // Live atomic gain handles read by the audio callbacks. Cheap to
    // mutate from the UI thread — no stream restart required.
    audio_gains: AudioGains,
    // Whether the Audio Devices window is currently open.
    devices_window_open: bool,
}

impl TokiApp {
    pub fn new() -> Self {
        let state = state::shared();
        let config = config::Config::load();

        // Spawn audio first (main thread — cpal on macOS/Windows wants to
        // be here). The runtime then consumes the mic stream and playback
        // ring through its select! loop.
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

        // Resolve the persisted binding, falling back to the
        // default (Backquote) if the config has nothing parseable.
        let initial = config.hotkey.to_input().or_else(|| {
            tracing::warn!(
                "no parseable PTT input in config, using default ({:?})",
                hotkey::DEFAULT_KEY
            );
            Some(hotkey::Input::Key(hotkey::DEFAULT_KEY))
        });

        let installed = hotkey::install(cmd_tx.clone(), initial);

        Self {
            state,
            cmd_tx,
            server: config.connection.server.clone(),
            display_name: config.connection.display_name.clone(),
            ptt_held: false,
            config,
            hotkey: installed,
            recording: false,
            audio_devices: devices,
            audio_control: control,
            audio_gains: gains,
            devices_window_open: false,
        }
    }

    /// Render the floating Audio Devices window. Each combo box reflects
    /// the persisted config; changing a selection sends the swap command
    /// to the audio thread and saves the new preference.
    fn show_devices_window(&mut self, ctx: &egui::Context) {
        if !self.devices_window_open {
            return;
        }
        let mut open = self.devices_window_open;
        egui::Window::new("Audio devices")
            .open(&mut open)
            .resizable(false)
            .collapsible(false)
            .show(ctx, |ui| {
                ui.add_space(4.0);

                // ── Input ───────────────────────────────────────────
                ui.label("Input");
                let prev_input = self.config.audio.input_device.clone();
                let input_label = self
                    .config
                    .audio
                    .input_device
                    .as_deref()
                    .unwrap_or("(system default)");
                egui::ComboBox::from_id_salt("audio_input")
                    .selected_text(input_label)
                    .width(280.0)
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
                if prev_input != self.config.audio.input_device {
                    self.audio_control
                        .set_input(self.config.audio.input_device.clone());
                    self.config.save();
                }

                // Input gain slider. Range 0.0 – 2.0 with snapping at
                // 1.0 to make "passthrough" easy to hit. Persisted on
                // drag-release, not every frame, to avoid hammering
                // the config file.
                ui.horizontal(|ui| {
                    ui.label("Input volume");
                    let mut gain = self.config.audio.input_gain;
                    let resp = ui.add(
                        egui::Slider::new(&mut gain, 0.0..=2.0)
                            .show_value(false)
                            .clamping(egui::SliderClamping::Always),
                    );
                    ui.monospace(format!("{:>3.0}%", gain * 100.0));
                    if resp.changed() {
                        self.config.audio.input_gain = gain;
                        self.audio_gains.set_input(gain);
                    }
                    if resp.drag_stopped() || resp.lost_focus() {
                        self.config.save();
                    }
                });

                ui.add_space(8.0);

                // ── Output ──────────────────────────────────────────
                ui.label("Output");
                let prev_output = self.config.audio.output_device.clone();
                let output_label = self
                    .config
                    .audio
                    .output_device
                    .as_deref()
                    .unwrap_or("(system default)");
                egui::ComboBox::from_id_salt("audio_output")
                    .selected_text(output_label)
                    .width(280.0)
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
                if prev_output != self.config.audio.output_device {
                    self.audio_control
                        .set_output(self.config.audio.output_device.clone());
                    self.config.save();
                }

                ui.horizontal(|ui| {
                    ui.label("Output volume");
                    let mut gain = self.config.audio.output_gain;
                    let resp = ui.add(
                        egui::Slider::new(&mut gain, 0.0..=2.0)
                            .show_value(false)
                            .clamping(egui::SliderClamping::Always),
                    );
                    ui.monospace(format!("{:>3.0}%", gain * 100.0));
                    if resp.changed() {
                        self.config.audio.output_gain = gain;
                        self.audio_gains.set_output(gain);
                    }
                    if resp.drag_stopped() || resp.lost_focus() {
                        self.config.save();
                    }
                });

                ui.add_space(8.0);
                ui.separator();
                ui.weak(
                    "Changes apply immediately. If a saved device is missing on startup, the host default is used.",
                );
            });
        self.devices_window_open = open;
    }

    /// Pretty label for the currently-bound PTT input. Returns `(none)`
    /// if the config doesn't parse into a known input.
    fn hotkey_label(&self) -> String {
        match self.config.hotkey.to_input() {
            Some(i) => hotkey::format(i),
            None => "(none)".into(),
        }
    }

    /// Poll the rdev listener for a captured keystroke and apply it as
    /// the new binding. The listener accepts presses from anywhere
    /// (Toki window need not be focused), and suppresses PTT firing
    /// for the recording press so binding doesn't transmit.
    /// Poll the rdev listener for any captured input — keyboard or
    /// mouse — and apply it as the new PTT binding. The listener
    /// accepts presses from anywhere (Toki window need not be focused)
    /// and swallows the recording press itself so binding doesn't
    /// transmit.
    fn try_capture_input(&mut self) {
        let Some(input) = self.hotkey.take_recorded() else {
            return;
        };
        match self.hotkey.rebind(input) {
            Ok(()) => {
                self.config.hotkey = HotkeyConfig::from_input(input);
                self.config.save();
                let label = hotkey::format(input);
                self.state
                    .lock()
                    .unwrap()
                    .log(format!("global PTT set to {label}"));
            }
            Err(e) => {
                self.state
                    .lock()
                    .unwrap()
                    .log(format!("rebind failed: {e}"));
            }
        }
        self.recording = false;
    }

    /// Sync the (trimmed) server + name form fields into the persisted
    /// config and write to disk. No-op when nothing changed — avoids
    /// touching the file on every focus-out.
    fn persist_connection_form(&mut self) {
        let server = self.server.trim().to_string();
        let display_name = self.display_name.trim().to_string();
        if self.config.connection.server == server
            && self.config.connection.display_name == display_name
        {
            return;
        }
        self.config.connection.server = server;
        self.config.connection.display_name = display_name;
        self.config.save();
    }
}

impl eframe::App for TokiApp {
    fn update(&mut self, ctx: &egui::Context, _: &mut eframe::Frame) {
        // The runtime updates state asynchronously; keep the GUI repainting
        // so log lines, presence, and PTT indicators stay live.
        ctx.request_repaint_after(Duration::from_millis(33));

        // If the Settings panel is waiting for a key, grab it before
        // anything else this frame so the new binding shows up everywhere
        // on the same render pass.
        if self.recording {
            self.try_capture_input();
        }

        let snap = self.snapshot();
        let hotkey_label = self.hotkey_label();

        egui::TopBottomPanel::top("top").show(ctx, |ui| {
            ui.add_space(4.0);
            ui.heading("Toki");
            ui.label(match &snap.connection {
                ConnState::Disconnected => "● disconnected".to_string(),
                ConnState::Connecting => "◌ connecting…".to_string(),
                ConnState::Connected => format!(
                    "◉ connected as {}",
                    snap.self_id.clone().unwrap_or_default()
                ),
                ConnState::Failed(e) => format!("✗ failed: {e}"),
            });
            ui.add_space(4.0);
        });

        egui::SidePanel::right("members")
            .min_width(160.0)
            .show(ctx, |ui| {
                ui.add_space(4.0);
                ui.strong("Members");
                ui.separator();
                if snap.members.is_empty() {
                    ui.weak("(none yet)");
                } else {
                    for (id, name) in &snap.members {
                        let is_self = snap.self_id.as_deref() == Some(id.as_str());
                        let speaking = snap.holder.as_deref() == Some(id.as_str());
                        let marker = if speaking { "🔊" } else { "  " };
                        let suffix = if is_self { " (you)" } else { "" };
                        ui.label(format!("{marker} {name}{suffix}"));
                    }
                }
            });

        egui::CentralPanel::default().show(ctx, |ui| {
            let connected = matches!(snap.connection, ConnState::Connected);
            let connecting = matches!(snap.connection, ConnState::Connecting);

            ui.add_enabled_ui(!connected && !connecting, |ui| {
                let mut form_dirty = false;
                egui::Grid::new("conn_form")
                    .num_columns(2)
                    .spacing([8.0, 6.0])
                    .show(ui, |ui| {
                        ui.label("Server");
                        let r = ui.text_edit_singleline(&mut self.server);
                        if r.lost_focus() {
                            form_dirty = true;
                        }
                        ui.end_row();
                        ui.label("Name");
                        let r = ui.text_edit_singleline(&mut self.display_name);
                        if r.lost_focus() {
                            form_dirty = true;
                        }
                        ui.end_row();
                    });
                // Persist on focus loss (Tab out, click away) so an
                // edited value survives a quit-without-connecting.
                if form_dirty {
                    self.persist_connection_form();
                }
            });

            ui.add_space(6.0);
            ui.horizontal(|ui| {
                if connected {
                    if ui.button("Disconnect").clicked() {
                        let _ = self.cmd_tx.send(Cmd::Disconnect);
                    }
                } else {
                    let label = if connecting { "Connecting…" } else { "Connect" };
                    if ui
                        .add_enabled(!connecting, egui::Button::new(label))
                        .clicked()
                    {
                        // Persist whatever the user typed: clicking
                        // Connect is the strongest "I want these
                        // values" signal we'll ever get.
                        self.persist_connection_form();
                        let _ = self.cmd_tx.send(Cmd::Connect {
                            server: self.server.trim().to_string(),
                            display_name: self.display_name.trim().to_string(),
                        });
                    }
                }
            });

            ui.add_space(4.0);
            egui::CollapsingHeader::new("Settings")
                .default_open(false)
                .show(ui, |ui| {
                    let listener_available = self.hotkey.available();
                    ui.horizontal(|ui| {
                        ui.label("Global PTT:");
                        if self.recording {
                            ui.colored_label(
                                egui::Color32::from_rgb(220, 180, 30),
                                "press any key or mouse button…",
                            );
                            if ui.button("Cancel").clicked() {
                                self.recording = false;
                                self.hotkey.cancel_recording();
                            }
                        } else {
                            ui.monospace(format!("[ {hotkey_label} ]"));
                            let bind = ui.add_enabled(
                                listener_available,
                                egui::Button::new("Bind"),
                            );
                            if bind.clicked() {
                                if self.hotkey.start_recording() {
                                    self.recording = true;
                                } else {
                                    self.state
                                        .lock()
                                        .unwrap()
                                        .log("global PTT unavailable on this system");
                                }
                            }
                            if !listener_available {
                                bind.on_hover_text(
                                    "Global PTT unavailable on this system (Wayland or missing permissions).",
                                );
                            }
                        }
                    });
                    ui.horizontal(|ui| {
                        ui.label("Audio devices:");
                        if ui.button("Open…").clicked() {
                            self.devices_window_open = !self.devices_window_open;
                        }
                    });
                });

            ui.separator();
            ui.add_space(8.0);

            // ── PTT ─────────────────────────────────────────────────
            // Walkie-talkie semantics: the button is locked out while
            // somebody else holds the floor. Our own transmitting state
            // is derived from the server-confirmed holder, never from the
            // local press — so a denied request never lights up red.
            let self_id = snap.self_id.as_deref();
            let is_transmitting = self_id.is_some() && snap.holder.as_deref() == self_id;
            let locked_by_other = snap.holder.is_some() && !is_transmitting;
            let other_name = if locked_by_other {
                snap.holder
                    .as_ref()
                    .and_then(|id| snap.members.get(id))
                    .cloned()
                    .unwrap_or_else(|| "someone".into())
            } else {
                String::new()
            };

            let ptt_enabled = connected && !locked_by_other;
            let label = if locked_by_other {
                format!("🔒 {other_name} is talking")
            } else if is_transmitting {
                "● TRANSMITTING".to_string()
            } else {
                // The on-button hint is just the configured global PTT.
                // No SPACE fallback — the bound input is the only key.
                format!("Hold to talk\n({hotkey_label})")
            };
            let fill = if is_transmitting {
                egui::Color32::from_rgb(220, 30, 30)
            } else if locked_by_other {
                egui::Color32::from_gray(40)
            } else {
                egui::Color32::from_gray(70)
            };
            let btn = egui::Button::new(
                egui::RichText::new(label).color(egui::Color32::WHITE).strong(),
            )
            .fill(fill)
            .min_size(egui::vec2(240.0, 80.0));
            let resp = ui.add_enabled(ptt_enabled, btn);

            // PTT is driven by the bound global input (via runtime
            // `Cmd::PttDown` / `Cmd::PttUp`) OR by clicking and
            // holding the button. No SPACE fallback — the bound key
            // is the single source of PTT, per the design.
            let pressed_now = ptt_enabled && resp.is_pointer_button_down_on();
            if pressed_now != self.ptt_held {
                self.ptt_held = pressed_now;
                let _ = self.cmd_tx.send(if pressed_now {
                    Cmd::PttDown
                } else {
                    Cmd::PttUp
                });
            }

            ui.add_space(12.0);
            ui.separator();
            ui.strong("Log");
            egui::ScrollArea::vertical()
                .max_height(180.0)
                .auto_shrink([false; 2])
                .show(ui, |ui| {
                    for line in &snap.log {
                        ui.monospace(line);
                    }
                });
        });

        // Render the Audio Devices window on top of everything else.
        self.show_devices_window(ctx);
    }
}

impl TokiApp {
    fn snapshot(&self) -> StateSnapshot {
        let s = self.state.lock().unwrap();
        StateSnapshot {
            connection: s.connection.clone(),
            self_id: s.self_id.clone(),
            members: s.members.clone(),
            holder: s.holder.clone(),
            log: s.log.iter().rev().take(120).cloned().collect(),
        }
    }
}

struct StateSnapshot {
    connection: ConnState,
    self_id: Option<String>,
    members: HashMap<String, String>,
    holder: Option<String>,
    log: Vec<String>,
}
