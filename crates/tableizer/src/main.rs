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
mod model;
mod persist;
mod theme;
mod ui;

use std::path::PathBuf;

use eframe::egui;

use crate::app::TableizerApp;

/// The window / taskbar icon, decoded once from the committed master PNG (`assets/icon.png`). The
/// macOS `.app` bundle uses `icon.icns` instead; this covers Linux/Windows and bare-binary runs.
fn app_icon() -> egui::IconData {
    eframe::icon_data::from_png_bytes(include_bytes!("../../../assets/icon.png"))
        .expect("bundled assets/icon.png is a valid PNG")
}

fn main() -> eframe::Result<()> {
    let path = std::env::args_os().nth(1).map(PathBuf::from);
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
            let mut app = TableizerApp::new(path);
            app.install_fonts(&cc.egui_ctx); // chrome + table fonts; re-installed on change in `ui`
            // The theme (`theme` module) is resolved and applied each frame in `App::ui`.
            Ok(Box::new(app))
        }),
    )
}
