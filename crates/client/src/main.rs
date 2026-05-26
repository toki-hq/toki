mod app;
mod audio;
mod config;
mod hotkey;
mod runtime;
mod state;
mod theme;

use eframe::egui;
use tracing_subscriber::EnvFilter;

use crate::app::TokiApp;

fn main() -> eframe::Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(EnvFilter::try_from_default_env().unwrap_or_else(|_| "info".into()))
        .init();

    // Strip widget — landscape, always-on-top, OS-chromed for v1 so the
    // user has a real titlebar to drag/close/minimize. The design's
    // borderless chassis with hand-drawn traffic-light dots is in the
    // spec as a follow-up; this gets the look right first, then we can
    // strip the decorations later.
    let options = eframe::NativeOptions {
        viewport: egui::ViewportBuilder::default()
            .with_inner_size([theme::WIDGET_W, theme::WIDGET_H])
            .with_min_inner_size([480.0, theme::WIDGET_H])
            .with_max_inner_size([760.0, theme::WIDGET_H])
            .with_resizable(true)
            .with_title("Toki"),
        ..Default::default()
    };

    eframe::run_native(
        "Toki",
        options,
        Box::new(|cc| {
            // egui's defaults are bright and blue-tinted; reset to the
            // dark-OLED aesthetic the design calls for. Individual
            // widgets paint their own backgrounds, so this mostly just
            // ensures the window-level frame doesn't show.
            cc.egui_ctx.set_visuals(egui::Visuals::dark());
            Ok(Box::new(TokiApp::new()))
        }),
    )
}
