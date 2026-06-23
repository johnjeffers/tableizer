//! Tableizer — cross-platform desktop shell.
//!
//! A native window via `eframe` (winit + wgpu + egui). Opens a delimited file passed as the first
//! CLI argument. `CsvTable::open` returns instantly and indexes in the background, so the first
//! screen is immediate. Rows render in a virtualised **`egui_table`** grid (sticky header, sticky
//! first column, resizable columns, column show/hide, **header drag-to-reorder**) that prefetches
//! only the visible rows from the engine's [`tableizer_core::ViewportSource`] — the vehicle for the
//! Phase 0 grid go/no-go spike (`docs/spec.md` §7).
//!
//! This module is GUI glue with no headless test seam (the engine it drives is unit-tested), except
//! the column-reorder logic, which is a pure function with its own tests.

use std::path::{Path, PathBuf};

use eframe::egui;
use tableizer_core::{
    CancellationToken, Cell, ColumnId, CsvTable, RowCount, RowRange, ViewportRequest,
    ViewportSource, parse::Dialect,
};

fn main() -> eframe::Result<()> {
    let path = std::env::args_os().nth(1).map(PathBuf::from);
    let native_options = eframe::NativeOptions::default();
    eframe::run_native(
        "Tableizer",
        native_options,
        Box::new(move |cc| {
            // Set an explicit theme so text/background contrast is deterministic across platforms.
            cc.egui_ctx.set_visuals(egui::Visuals::dark());
            Ok(Box::new(TableizerApp::new(path)))
        }),
    )
}

/// Display order + visibility of columns (app state; persists across frames).
struct GridLayout {
    /// All source columns in display order.
    order: Vec<ColumnId>,
    /// Visibility per source-column index.
    visible: Vec<bool>,
}

impl GridLayout {
    fn new(column_count: usize) -> Self {
        Self {
            order: (0..column_count as u32).map(ColumnId).collect(),
            visible: vec![true; column_count],
        }
    }

    /// Visible columns, in display order — what the grid actually renders.
    fn displayed(&self) -> Vec<ColumnId> {
        self.order
            .iter()
            .copied()
            .filter(|c| self.visible[c.0 as usize])
            .collect()
    }
}

/// Move `dragged` to just before `before` in display order. Pure so the reorder logic is verified
/// independently of the drag-and-drop UI.
fn reorder(order: &mut Vec<ColumnId>, dragged: ColumnId, before: ColumnId) {
    if dragged == before {
        return;
    }
    let Some(from) = order.iter().position(|&c| c == dragged) else {
        return;
    };
    let col = order.remove(from);
    let insert_at = order
        .iter()
        .position(|&c| c == before)
        .unwrap_or(order.len());
    order.insert(insert_at, col);
}

/// What the window is currently showing.
enum View {
    Empty,
    Loaded {
        path: PathBuf,
        table: CsvTable,
        layout: GridLayout,
    },
    Failed {
        path: PathBuf,
        error: String,
    },
}

struct TableizerApp {
    view: View,
}

impl TableizerApp {
    fn new(path: Option<PathBuf>) -> Self {
        // `open` is instant (mmap + head parse); the index builds in the background.
        let view = match path {
            Some(path) => match CsvTable::open(&path, Dialect::default()) {
                Ok(table) => {
                    let layout = GridLayout::new(table.schema().columns.len());
                    View::Loaded {
                        path,
                        table,
                        layout,
                    }
                }
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
        match &mut self.view {
            View::Empty => {
                ui.heading("Tableizer");
                ui.label("Open a file by passing its path as the first argument: `tableizer <file.csv>`.");
            }
            View::Failed { path, error } => {
                ui.heading("Could not open file");
                ui.label(format!("{}: {error}", path.display()));
            }
            View::Loaded {
                path,
                table,
                layout,
            } => show_table(ui, path, table, layout),
        }
    }
}

/// Render the loaded table as a virtualised `egui_table` grid.
fn show_table(ui: &mut egui::Ui, path: &Path, table: &CsvTable, layout: &mut GridLayout) {
    let (total, indexing) = match table.row_count() {
        RowCount::Exact(n) => (n, false),
        RowCount::AtLeast(n) => (n, true),
    };

    ui.horizontal(|ui| {
        ui.heading("Tableizer");
        if indexing {
            ui.label(format!("{}  ·  indexing… ≥ {total} rows", path.display()));
            ui.spinner();
            ui.ctx().request_repaint(); // refresh the growing count + newly-available rows
        } else {
            ui.label(format!("{}  ·  {total} rows", path.display()));
        }
        ui.menu_button("Columns", |ui| {
            for (i, shown) in layout.visible.iter_mut().enumerate() {
                ui.checkbox(shown, format!("col{i}"));
            }
        });
    });
    ui.separator();

    let displayed = layout.displayed();
    if displayed.is_empty() {
        ui.label("All columns hidden.");
        return;
    }

    let table_columns: Vec<egui_table::Column> = (0..displayed.len())
        .map(|_| egui_table::Column::new(140.0).resizable(true))
        .collect();
    let mut delegate = GridDelegate {
        table,
        columns: displayed,
        cache_start: 0,
        cache: Vec::new(),
        pending_reorder: None,
    };

    egui_table::Table::new()
        .id_salt("tableizer-grid")
        .num_rows(total)
        .columns(table_columns)
        .num_sticky_cols(1) // pin the first column
        .headers(vec![egui_table::HeaderRow::new(22.0)])
        .show(ui, &mut delegate);

    // Apply a header drag-to-reorder, if one happened this frame.
    if let Some((dragged, before)) = delegate.pending_reorder {
        reorder(&mut layout.order, dragged, before);
    }
}

/// Bridges `egui_table`'s pull-based rendering to the engine: `prepare` fetches the visible row
/// window once, and `cell_ui` reads from that cache — so only visible rows ever cross the seam.
struct GridDelegate<'a> {
    table: &'a CsvTable,
    /// Visible source columns, in display order.
    columns: Vec<ColumnId>,
    cache_start: u64,
    cache: Vec<Vec<Cell>>,
    /// Set by `header_cell_ui` when a column header is dropped onto another; applied after `show`.
    pending_reorder: Option<(ColumnId, ColumnId)>,
}

impl egui_table::TableDelegate for GridDelegate<'_> {
    fn prepare(&mut self, info: &egui_table::PrefetchInfo) {
        let range = &info.visible_rows;
        let len = u32::try_from(range.end.saturating_sub(range.start)).unwrap_or(u32::MAX);
        let viewport = self
            .table
            .fetch(
                &ViewportRequest {
                    rows: RowRange {
                        start: range.start,
                        len,
                    },
                    columns: self.columns.clone(),
                },
                &CancellationToken::new(),
            )
            .unwrap_or_default();
        self.cache_start = range.start;
        self.cache = viewport.rows;
    }

    fn header_cell_ui(&mut self, ui: &mut egui::Ui, cell: &egui_table::HeaderCellInfo) {
        let Some(&col_id) = self.columns.get(cell.col_range.start) else {
            return;
        };
        // Paint a distinct header bar so headers are always visible regardless of theme.
        ui.painter().rect_filled(
            ui.max_rect(),
            egui::CornerRadius::ZERO,
            egui::Color32::from_gray(60),
        );

        // The header is a drag source carrying its column id, and a drop target for reordering.
        let response = ui
            .dnd_drag_source(egui::Id::new(("tz-col", col_id.0)), col_id, |ui| {
                ui.set_min_width(ui.available_width()); // make the whole header cell draggable
                ui.add(
                    egui::Label::new(
                        egui::RichText::new(format!("col{}", col_id.0))
                            .strong()
                            .color(egui::Color32::WHITE),
                    )
                    .selectable(false),
                );
            })
            .response;

        if response.hovered() {
            ui.ctx().set_cursor_icon(egui::CursorIcon::Grab); // hint that headers are draggable
        }
        if let Some(dragged) = response.dnd_release_payload::<ColumnId>()
            && *dragged != col_id
        {
            self.pending_reorder = Some((*dragged, col_id));
        }
    }

    fn cell_ui(&mut self, ui: &mut egui::Ui, cell: &egui_table::CellInfo) {
        let value = cell
            .row_nr
            .checked_sub(self.cache_start)
            .and_then(|row| self.cache.get(row as usize))
            .and_then(|row| row.get(cell.col_nr));
        if let Some(cell_value) = value {
            ui.monospace(String::from_utf8_lossy(&cell_value.0));
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reorder_moves_a_column_before_the_target() {
        let mut order = vec![ColumnId(0), ColumnId(1), ColumnId(2), ColumnId(3)];
        reorder(&mut order, ColumnId(3), ColumnId(1));
        assert_eq!(
            order,
            vec![ColumnId(0), ColumnId(3), ColumnId(1), ColumnId(2)]
        );
    }

    #[test]
    fn reorder_dragging_onto_itself_is_a_noop() {
        let mut order = vec![ColumnId(0), ColumnId(1)];
        reorder(&mut order, ColumnId(1), ColumnId(1));
        assert_eq!(order, vec![ColumnId(0), ColumnId(1)]);
    }

    #[test]
    fn displayed_columns_respect_visibility_and_order() {
        let mut layout = GridLayout::new(3);
        layout.visible[1] = false;
        reorder(&mut layout.order, ColumnId(2), ColumnId(0));
        assert_eq!(layout.displayed(), vec![ColumnId(2), ColumnId(0)]);
    }
}
