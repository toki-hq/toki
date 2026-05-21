use std::sync::{Arc, Mutex};

use eframe::egui;
use tracing_subscriber::EnvFilter;

#[derive(Default, Clone)]
struct UiState {
    server: String,
    display_name: String,
    channel: String,
    connected: bool,
    transmitting: bool,
    log: Vec<String>,
}

struct TokiApp {
    state: Arc<Mutex<UiState>>,
}

impl Default for TokiApp {
    fn default() -> Self {
        let state = UiState {
            server: "http://127.0.0.1:50051".into(),
            display_name: "anon".into(),
            channel: "general".into(),
            ..Default::default()
        };
        Self {
            state: Arc::new(Mutex::new(state)),
        }
    }
}

impl eframe::App for TokiApp {
    fn update(&mut self, ctx: &egui::Context, _: &mut eframe::Frame) {
        egui::CentralPanel::default().show(ctx, |ui| {
            let mut state = self.state.lock().unwrap();

            ui.heading("Toki");
            ui.label("Walkie-talkie over the internet");
            ui.separator();

            ui.horizontal(|ui| {
                ui.label("Server");
                ui.text_edit_singleline(&mut state.server);
            });
            ui.horizontal(|ui| {
                ui.label("Name");
                ui.text_edit_singleline(&mut state.display_name);
            });
            ui.horizontal(|ui| {
                ui.label("Channel");
                ui.text_edit_singleline(&mut state.channel);
            });

            ui.separator();

            let connect_label = if state.connected { "Disconnect" } else { "Connect" };
            if ui.button(connect_label).clicked() {
                state.connected = !state.connected;
                let msg = if state.connected {
                    format!("connected to {} as {}", state.server, state.display_name)
                } else {
                    "disconnected".to_string()
                };
                state.log.push(msg);
            }

            ui.add_space(12.0);

            let ptt = egui::Button::new(if state.transmitting { "● TRANSMITTING" } else { "Hold to talk" })
                .min_size(egui::vec2(220.0, 80.0));
            let resp = ui.add_enabled(state.connected, ptt);
            let now_pressed = resp.is_pointer_button_down_on();
            if now_pressed != state.transmitting {
                state.transmitting = now_pressed;
                state
                    .log
                    .push(if now_pressed { "PTT down".into() } else { "PTT up".into() });
            }

            ui.separator();
            ui.label("Log");
            egui::ScrollArea::vertical().show(ui, |ui| {
                for line in state.log.iter().rev().take(50) {
                    ui.monospace(line);
                }
            });
        });

        ctx.request_repaint();
    }
}

fn main() -> eframe::Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(EnvFilter::try_from_default_env().unwrap_or_else(|_| "info".into()))
        .init();

    let options = eframe::NativeOptions {
        viewport: egui::ViewportBuilder::default()
            .with_inner_size([420.0, 520.0])
            .with_min_inner_size([320.0, 400.0])
            .with_title("Toki"),
        ..Default::default()
    };

    eframe::run_native(
        "Toki",
        options,
        Box::new(|_cc| Ok(Box::<TokiApp>::default())),
    )
}
