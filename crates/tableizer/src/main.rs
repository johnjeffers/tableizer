//! Tableizer — cross-platform desktop shell.
//!
//! A native window via `eframe` (winit + wgpu + egui). Today it renders a placeholder; the
//! virtualized grid (egui_table) wired to the engine's [`tableizer_core::ViewportSource`] lands in
//! Phase 0 (see `docs/spec.md` §7).

use eframe::egui;

fn main() -> eframe::Result<()> {
    let native_options = eframe::NativeOptions::default();
    eframe::run_native(
        "Tableizer",
        native_options,
        Box::new(|_cc| Ok(Box::new(TableizerApp::new()))),
    )
}

/// The root application state. Will hold the open table's `ViewportSource`, the active view
/// (filter/sort/projection), and the grid widget state once Phase 0 lands.
struct TableizerApp {
    // Placeholder until the engine is wired in.
    _private: (),
}

impl TableizerApp {
    fn new() -> Self {
        Self { _private: () }
    }
}

impl eframe::App for TableizerApp {
    // egui 0.34 hands the app a `Ui` for the central panel directly (replacing the old
    // `update(&Context)` + `CentralPanel::show` pattern).
    fn ui(&mut self, ui: &mut egui::Ui, _frame: &mut eframe::Frame) {
        ui.heading("Tableizer");
        ui.label(
            "Cross-platform desktop shell (winit + wgpu + egui). \
             Engine + virtualized grid wiring is next (Phase 0).",
        );
    }
}
