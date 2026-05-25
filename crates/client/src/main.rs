mod app;
mod audio;
mod runtime;
mod state;

use eframe::egui;
use tracing_subscriber::EnvFilter;

use crate::app::TokiApp;

fn main() -> eframe::Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(EnvFilter::try_from_default_env().unwrap_or_else(|_| "info".into()))
        .init();

    let options = eframe::NativeOptions {
        viewport: egui::ViewportBuilder::default()
            .with_inner_size([560.0, 540.0])
            .with_min_inner_size([420.0, 420.0])
            .with_title("Toki"),
        ..Default::default()
    };

    eframe::run_native(
        "Toki",
        options,
        Box::new(|_cc| Ok(Box::new(TokiApp::new()))),
    )
}
