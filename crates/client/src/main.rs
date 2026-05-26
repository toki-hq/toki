// Suppress the console window on Windows release builds — the spec
// asks for a GUI-only launch (no flash of cmd.exe behind the widget,
// no terminal staying attached). Debug builds keep the default
// console subsystem so `cargo run` still streams tracing output.
//
// macOS and Linux: this attribute is a no-op there. Launching from a
// terminal still streams output; launching from a file manager / dock
// already detaches stdio.
#![cfg_attr(
    all(target_os = "windows", not(debug_assertions)),
    windows_subsystem = "windows"
)]

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
    // In a Windows release build the process has no console attached,
    // so writes to stderr go nowhere. `tracing_subscriber::fmt` still
    // initializes fine; the writes are silently dropped. If we ever
    // need release-build diagnostics, route this to a rolling file
    // appender in `dirs::data_dir()` instead of stderr.
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
