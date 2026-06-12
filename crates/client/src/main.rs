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
mod dsp;
mod hotkey;
mod identity;
mod runtime;
mod state;
mod telemetry;
mod theme;
mod update;

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

    // rustls 0.23 requires the process to install a default
    // `CryptoProvider` before any TLS handshake. Ring is the
    // backend; install once at startup so the gRPC connector
    // doesn't panic on first connect.
    let _ = rustls::crypto::ring::default_provider().install_default();

    // Strip widget — landscape, always-on-top, OS-chromed for v1 so the
    // user has a real titlebar to drag/close/minimize. The design's
    // borderless chassis with hand-drawn traffic-light dots is in the
    // spec as a follow-up; this gets the look right first, then we can
    // strip the decorations later.
    let mut viewport = egui::ViewportBuilder::default()
        .with_inner_size([theme::WIDGET_W, theme::WIDGET_H])
        .with_min_inner_size([480.0, theme::WIDGET_H])
        .with_max_inner_size([760.0, theme::WIDGET_H])
        .with_resizable(true)
        .with_title("Toki");
    if let Some(icon) = load_window_icon() {
        viewport = viewport.with_icon(icon);
    }
    let options = eframe::NativeOptions {
        viewport,
        ..Default::default()
    };

    /// Decode the bundled window-icon PNG into the raw RGBA buffer
    /// `egui::IconData` expects. Logged-and-skipped on failure rather
    /// than fatal — a missing icon is a cosmetic issue, not a reason
    /// to refuse to launch.
    ///
    /// Uses the 256 × 256 PNG since most modern desktop shells request
    /// either 256 or scale up from it; the 16/32 px versions in the
    /// bundle are only used inside platform packaging files
    /// (Toki.icns, Toki.ico) where the OS picks the best size itself.
    fn load_window_icon() -> Option<egui::IconData> {
        const PNG: &[u8] = include_bytes!("../assets/icon/toki-icon-256.png");
        match image::load_from_memory_with_format(PNG, image::ImageFormat::Png) {
            Ok(img) => {
                let rgba = img.into_rgba8();
                let (w, h) = rgba.dimensions();
                Some(egui::IconData {
                    rgba: rgba.into_raw(),
                    width: w,
                    height: h,
                })
            }
            Err(e) => {
                tracing::warn!(error = %e, "could not decode bundled window icon");
                None
            }
        }
    }

    eframe::run_native(
        "Toki",
        options,
        Box::new(|cc| {
            // egui's defaults are bright and blue-tinted; reset to the
            // dark-OLED aesthetic the design calls for. Individual
            // widgets paint their own backgrounds, so this mostly just
            // ensures the window-level frame doesn't show.
            cc.egui_ctx.set_visuals(egui::Visuals::dark());
            // Embed the project's TTFs as the default UI / mono fonts
            // *before* the app paints its first frame, so we never show
            // a single frame in Ubuntu-Light then snap to the brand
            // face on frame 2.
            app::register_fonts(&cc.egui_ctx);
            // Hand the app a Context clone so the update checker's worker
            // thread can request a repaint when a check completes.
            Ok(Box::new(TokiApp::new(cc.egui_ctx.clone())))
        }),
    )
}
