//! Tableizer — cross-platform desktop shell.
//!
//! A native window via `eframe` (winit + wgpu + egui). Opens a delimited file passed as the first
//! CLI argument: the **dialect** (delimiter + header) and **text encoding** are auto-detected and
//! user-overridable. `CsvTable::open` returns instantly and indexes in the background, so the first
//! screen is immediate. Rows render in a virtualised **`egui_table`** grid (header names, sticky
//! header, resizable columns, click-to-sort, column show/hide, header drag-to-reorder) that
//! prefetches only the visible rows from the engine's [`tableizer_core::ViewportSource`] seam.
//!
//! Encoding is a *display* concern: cells stay raw bytes in the engine (byte fidelity); the app
//! decodes them via the selected encoding for rendering.
//!
//! GUI glue with no headless test seam (the engine it drives is unit-tested), except the pure
//! `reorder` and `decode_field` helpers, which have their own tests.

mod app;
mod fonts;
#[cfg(target_os = "macos")]
mod macos_open;
mod model;
mod persist;
mod theme;
mod ui;

use eframe::egui;

use crate::app::TableizerApp;

/// The window / taskbar icon, decoded once from the committed master PNG (`assets/icon.png`). The
/// macOS `.app` bundle uses `icon.icns` instead; this covers Linux/Windows and bare-binary runs.
fn app_icon() -> egui::IconData {
    eframe::icon_data::from_png_bytes(include_bytes!("../../../assets/icon.png"))
        .expect("bundled assets/icon.png is a valid PNG")
}

fn main() -> eframe::Result<()> {
    // The first CLI arg is a local path or a remote URL (e.g. `s3://bucket/data.csv`).
    let target = std::env::args_os()
        .nth(1)
        .map(|a| a.to_string_lossy().into_owned());
    // Register the macOS open-documents handler as early as possible, to give a cold-launch file the
    // best chance of being caught before AppKit dispatches it (see macos_open.rs).
    #[cfg(target_os = "macos")]
    macos_open::install();
    let native_options = eframe::NativeOptions {
        // Initial size on first launch; the `persistence` feature restores the last geometry after.
        viewport: egui::ViewportBuilder::default()
            .with_inner_size([1280.0, 800.0])
            .with_min_inner_size([640.0, 400.0])
            .with_icon(app_icon()),
        ..Default::default()
    };
    eframe::run_native(
        "Tableizer",
        native_options,
        Box::new(move |cc| {
            let mut app = TableizerApp::new(target);
            app.install_fonts(&cc.egui_ctx); // chrome + table fonts; re-installed on change in `ui`
            // Let the open-documents handler wake an idle UI to drain a received file. (Re-asserting
            // the handler after launch is handled by the run-loop observer in `install`.)
            #[cfg(target_os = "macos")]
            macos_open::set_repaint_ctx(cc.egui_ctx.clone());
            // The theme (`theme` module) is resolved and applied each frame in `App::ui`.
            Ok(Box::new(app))
        }),
    )
}
