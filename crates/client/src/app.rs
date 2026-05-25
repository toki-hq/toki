use std::collections::HashMap;
use std::time::Duration;

use eframe::egui;
use tokio::sync::mpsc::UnboundedSender;

use crate::audio::{self, AudioControl, AudioDevices};
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
    channel: String,

    ptt_held: bool,

    // Persisted user preferences (hotkey + audio device selection).
    config: config::Config,

    // Global PTT hotkey. `None` if the platform doesn't support it (e.g.
    // Wayland) or registration failed — in-window SPACE still works.
    hotkey: Option<InstalledHotkey>,

    // True while the Settings panel is waiting for the user to press a key
    // to bind. Captured in the next `update()` that sees a Key event.
    recording_hotkey: bool,

    // Snapshot of cpal devices at startup. We don't auto-refresh.
    audio_devices: AudioDevices,
    // Sender to the audio thread for hot-swap.
    audio_control: AudioControl,
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
        )
        .expect("audio init failed");
        let audio::AudioHandle {
            mic_rx,
            playback,
            devices,
            control,
        } = audio_handle;

        let cmd_tx = runtime::spawn(state.clone(), mic_rx, playback);

        // Build the initial HotKey from the saved config, falling back to
        // the default (backtick) if the persisted value won't parse.
        let initial = config.hotkey.to_hotkey().unwrap_or_else(|| {
            tracing::warn!(key = %config.hotkey.key, "invalid hotkey in config, using default");
            hotkey::HotKey::new(None, hotkey::DEFAULT_KEY)
        });

        let installed = hotkey::install(cmd_tx.clone(), initial);

        Self {
            state,
            cmd_tx,
            server: "http://127.0.0.1:50051".into(),
            display_name: "anon".into(),
            channel: "general".into(),
            ptt_held: false,
            config,
            hotkey: installed,
            recording_hotkey: false,
            audio_devices: devices,
            audio_control: control,
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

                ui.add_space(8.0);
                ui.separator();
                ui.weak(
                    "Changes apply immediately. If a saved device is missing on startup, the host default is used.",
                );
            });
        self.devices_window_open = open;
    }

    /// Pretty label for the currently-bound global hotkey, e.g. `` ` `` or
    /// `Ctrl+F8`. Falls back to the raw `Code` string if the persisted
    /// value won't parse — that path is only reachable if someone hand-
    /// edited the config to nonsense.
    fn hotkey_label(&self) -> String {
        use std::str::FromStr;
        match hotkey::Code::from_str(&self.config.hotkey.key) {
            Ok(code) => hotkey::format(code, self.config.hotkey.modifiers()),
            Err(_) => self.config.hotkey.key.clone(),
        }
    }

    /// While the Settings panel is in "press a key" mode, look for the
    /// next non-repeat Key event and rebind. If the user presses an
    /// unsupported key we stay in recording mode (silent ignore — let
    /// them try again).
    fn try_capture_hotkey(&mut self, ctx: &egui::Context) {
        let captured: Option<(egui::Key, egui::Modifiers)> = ctx.input(|i| {
            i.events.iter().find_map(|e| {
                if let egui::Event::Key {
                    key,
                    pressed: true,
                    repeat: false,
                    modifiers,
                    ..
                } = e
                {
                    Some((*key, *modifiers))
                } else {
                    None
                }
            })
        });

        let Some((key, mods)) = captured else { return };
        let Some(code) = hotkey::from_egui_key(key) else {
            return; // unsupported key — keep recording
        };
        let gmods = hotkey::from_egui_modifiers(mods);

        let Some(installed) = self.hotkey.as_mut() else {
            self.recording_hotkey = false;
            return;
        };
        match installed.rebind(code, gmods) {
            Ok(()) => {
                self.config.hotkey = HotkeyConfig::from_parts(code, gmods);
                self.config.save();
                let label = hotkey::format(code, gmods);
                self.state
                    .lock()
                    .unwrap()
                    .log(format!("global PTT key set to {label}"));
                self.recording_hotkey = false;
            }
            Err(e) => {
                self.state
                    .lock()
                    .unwrap()
                    .log(format!("rebind failed: {e}"));
                self.recording_hotkey = false;
            }
        }
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
        if self.recording_hotkey {
            self.try_capture_hotkey(ctx);
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
                egui::Grid::new("conn_form")
                    .num_columns(2)
                    .spacing([8.0, 6.0])
                    .show(ui, |ui| {
                        ui.label("Server");
                        ui.text_edit_singleline(&mut self.server);
                        ui.end_row();
                        ui.label("Name");
                        ui.text_edit_singleline(&mut self.display_name);
                        ui.end_row();
                        ui.label("Channel");
                        ui.text_edit_singleline(&mut self.channel);
                        ui.end_row();
                    });
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
                        let _ = self.cmd_tx.send(Cmd::Connect {
                            server: self.server.trim().to_string(),
                            display_name: self.display_name.trim().to_string(),
                            channel: self.channel.trim().to_string(),
                        });
                    }
                }
            });

            ui.add_space(4.0);
            egui::CollapsingHeader::new("Settings")
                .default_open(false)
                .show(ui, |ui| {
                    ui.horizontal(|ui| {
                        ui.label("Global PTT:");
                        if self.hotkey.is_none() {
                            ui.weak("(unavailable on this system)");
                        } else if self.recording_hotkey {
                            ui.colored_label(
                                egui::Color32::from_rgb(220, 180, 30),
                                "press any key…",
                            );
                            if ui.button("Cancel").clicked() {
                                self.recording_hotkey = false;
                            }
                        } else {
                            ui.monospace(format!("[ {hotkey_label} ]"));
                            if ui.button("Change").clicked() {
                                self.recording_hotkey = true;
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
            // somebody else holds the channel. Our own transmitting state
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
            } else if self.hotkey.is_some() {
                format!("Hold to talk\n(SPACE here, {hotkey_label} anywhere)")
            } else {
                "Hold to talk\n(SPACE)".to_string()
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

            let pressed_now = ptt_enabled
                && (resp.is_pointer_button_down_on()
                    || ctx.input(|i| i.key_down(egui::Key::Space)));
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
