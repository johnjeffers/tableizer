//! Tableizer — cross-platform desktop shell.
//!
//! A native window via `eframe` (winit + wgpu + egui). Opens a delimited file passed as the first
//! CLI argument. `CsvTable::open` returns instantly and indexes in the background, so the first
//! screen is immediate; the view renders a **virtualised** scroll that fetches only the visible rows
//! from the engine's [`tableizer_core::ViewportSource`], and the row count grows as indexing
//! progresses (`docs/spec.md` §2, §7).

use std::path::{Path, PathBuf};

use eframe::egui;
use tableizer_core::{
    CancellationToken, ColumnId, CsvTable, RowCount, RowRange, ViewportRequest, ViewportSource,
    parse::Dialect,
};

fn main() -> eframe::Result<()> {
    let path = std::env::args_os().nth(1).map(PathBuf::from);
    let native_options = eframe::NativeOptions::default();
    eframe::run_native(
        "Tableizer",
        native_options,
        Box::new(move |_cc| Ok(Box::new(TableizerApp::new(path)))),
    )
}

/// What the window is currently showing.
enum View {
    Empty,
    Loaded { path: PathBuf, table: CsvTable },
    Failed { path: PathBuf, error: String },
}

struct TableizerApp {
    view: View,
}

impl TableizerApp {
    fn new(path: Option<PathBuf>) -> Self {
        // `open` is instant (mmap + head parse); the index builds in the background.
        let view = match path {
            Some(path) => match CsvTable::open(&path, Dialect::default()) {
                Ok(table) => View::Loaded { path, table },
                Err(error) => View::Failed {
                    path,
                    error: error.to_string(),
                },
            },
            None => View::Empty,
        };
        Self { view }
    }
}

impl eframe::App for TableizerApp {
    fn ui(&mut self, ui: &mut egui::Ui, _frame: &mut eframe::Frame) {
        match &self.view {
            View::Empty => {
                ui.heading("Tableizer");
                ui.label("Open a file by passing its path as the first argument: `tableizer <file.csv>`.");
            }
            View::Failed { path, error } => {
                ui.heading("Could not open file");
                ui.label(format!("{}: {error}", path.display()));
            }
            View::Loaded { path, table } => show_table(ui, path, table),
        }
    }
}

/// Render the loaded table with a virtualised vertical scroll — only the visible rows are fetched.
/// While the background index builds, the row count shows as a growing lower bound.
fn show_table(ui: &mut egui::Ui, path: &Path, table: &CsvTable) {
    let (total, indexing) = match table.row_count() {
        RowCount::Exact(n) => (n as usize, false),
        RowCount::AtLeast(n) => (n as usize, true),
    };
    let columns: Vec<ColumnId> = (0..table.schema().columns.len() as u32)
        .map(ColumnId)
        .collect();

    ui.horizontal(|ui| {
        ui.heading("Tableizer");
        if indexing {
            ui.label(format!("{}  ·  indexing… ≥ {total} rows", path.display()));
            ui.spinner();
            ui.ctx().request_repaint(); // refresh the growing count + newly-available rows
        } else {
            ui.label(format!(
                "{}  ·  {total} rows  ·  {} cols",
                path.display(),
                columns.len()
            ));
        }
    });

    let row_height = ui.text_style_height(&egui::TextStyle::Monospace);
    egui::ScrollArea::vertical()
        .auto_shrink([false, false])
        .show_rows(ui, row_height, total, |ui, range| {
            let len = u32::try_from(range.end - range.start).unwrap_or(u32::MAX);
            let request = ViewportRequest {
                rows: RowRange {
                    start: range.start as u64,
                    len,
                },
                columns: columns.clone(),
            };
            // Virtualised: only the visible window crosses the seam; the rest of the file is untouched.
            let viewport = table
                .fetch(&request, &CancellationToken::new())
                .unwrap_or_default();
            for cells in &viewport.rows {
                let line = cells
                    .iter()
                    .map(|c| String::from_utf8_lossy(&c.0).into_owned())
                    .collect::<Vec<_>>()
                    .join("  |  ");
                ui.monospace(line);
            }
        });
}
