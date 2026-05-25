use std::collections::{HashMap, HashSet};
use std::time::Duration;

use eframe::egui;
use tokio::sync::mpsc::UnboundedSender;

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
}

impl TokiApp {
    pub fn new() -> Self {
        let state = state::shared();
        let cmd_tx = runtime::spawn(state.clone());
        Self {
            state,
            cmd_tx,
            server: "http://127.0.0.1:50051".into(),
            display_name: "anon".into(),
            channel: "general".into(),
            ptt_held: false,
        }
    }
}

impl eframe::App for TokiApp {
    fn update(&mut self, ctx: &egui::Context, _: &mut eframe::Frame) {
        // The runtime updates state asynchronously; keep the GUI repainting
        // so log lines, presence, and PTT indicators stay live.
        ctx.request_repaint_after(Duration::from_millis(33));

        let snap = self.snapshot();

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
                        let speaking = if is_self {
                            snap.transmitting
                        } else {
                            snap.speaking.contains(id)
                        };
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

            ui.separator();
            ui.add_space(8.0);

            // ── PTT ─────────────────────────────────────────────────
            let ptt_active = snap.transmitting;
            let label = if ptt_active {
                "● TRANSMITTING"
            } else {
                "Hold to talk\n(mouse or SPACE)"
            };
            let fill = if ptt_active {
                egui::Color32::from_rgb(220, 30, 30)
            } else {
                egui::Color32::from_gray(70)
            };
            let btn = egui::Button::new(egui::RichText::new(label).color(egui::Color32::WHITE).strong())
                .fill(fill)
                .min_size(egui::vec2(220.0, 80.0));
            let resp = ui.add_enabled(connected, btn);

            let pressed_now = connected
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
    }
}

impl TokiApp {
    fn snapshot(&self) -> StateSnapshot {
        let s = self.state.lock().unwrap();
        StateSnapshot {
            connection: s.connection.clone(),
            self_id: s.self_id.clone(),
            members: s.members.clone(),
            speaking: s.speaking.clone(),
            transmitting: s.transmitting,
            log: s.log.iter().rev().take(120).cloned().collect(),
        }
    }
}

struct StateSnapshot {
    connection: ConnState,
    self_id: Option<String>,
    members: HashMap<String, String>,
    speaking: HashSet<String>,
    transmitting: bool,
    log: Vec<String>,
}
