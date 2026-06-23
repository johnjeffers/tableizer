//! Tableizer — cross-platform desktop shell.
//!
//! A native window via `eframe` (winit + wgpu + egui). Opens a delimited file passed as the first
//! CLI argument: the **dialect** (delimiter + header) and **text encoding** are auto-detected and
//! user-overridable. `CsvTable::open` returns instantly and indexes in the background, so the first
//! screen is immediate. Rows render in a virtualised **`egui_table`** grid (header names, sticky
//! header, sticky first column, resizable columns, column show/hide, header drag-to-reorder) that
//! prefetches only the visible rows from the engine's [`tableizer_core::ViewportSource`] seam.
//!
//! Encoding is a *display* concern: cells stay raw bytes in the engine (byte fidelity); the app
//! decodes them via the selected encoding for rendering.
//!
//! GUI glue with no headless test seam (the engine it drives is unit-tested), except the pure
//! `reorder` and `decode_field` helpers, which have their own tests.

use std::io::Read;
use std::path::{Path, PathBuf};

use eframe::egui;
use encoding_rs::Encoding;
use tableizer_core::{
    CancellationToken, Cell, ColumnId, CsvTable, RowCount, RowRange, Schema, ViewportRequest,
    ViewportSource, parse::Dialect,
};

fn main() -> eframe::Result<()> {
    let path = std::env::args_os().nth(1).map(PathBuf::from);
    let native_options = eframe::NativeOptions::default();
    eframe::run_native(
        "Tableizer",
        native_options,
        Box::new(move |cc| {
            // Explicit theme so text/background contrast is deterministic across platforms.
            cc.egui_ctx.set_visuals(egui::Visuals::dark());
            Ok(Box::new(TableizerApp::new(path)))
        }),
    )
}

/// Read a head sample and auto-detect the dialect (delimiter + header); fall back to the default.
fn sniff_file(path: &Path) -> Dialect {
    let mut head = vec![0u8; 64 * 1024];
    let read = std::fs::File::open(path)
        .and_then(|mut f| f.read(&mut head))
        .unwrap_or(0);
    head.truncate(read);
    Dialect::sniff(&head)
}

/// Detect the text encoding from a leading byte-order mark; default to UTF-8.
fn detect_encoding(path: &Path) -> &'static Encoding {
    let mut bom = [0u8; 4];
    let n = std::fs::File::open(path)
        .and_then(|mut f| f.read(&mut bom))
        .unwrap_or(0);
    Encoding::for_bom(&bom[..n]).map_or(encoding_rs::UTF_8, |(enc, _)| enc)
}

/// Decode raw field bytes to display text in `encoding`, dropping a leading BOM the decoder surfaces.
fn decode_field(bytes: &[u8], encoding: &'static Encoding) -> String {
    let (text, _, _) = encoding.decode(bytes);
    text.strip_prefix('\u{feff}').unwrap_or(&text).to_owned()
}

/// Case-insensitive substring match. `query_lower` must already be lowercased (an empty query never
/// matches, so an empty search box highlights nothing).
fn cell_matches(text: &str, query_lower: &str) -> bool {
    !query_lower.is_empty() && text.to_lowercase().contains(query_lower)
}

/// Open `path` behind the `ViewportSource` seam (the app is format-agnostic).
fn open_table(path: &Path, dialect: Dialect) -> Result<Box<dyn ViewportSource>, String> {
    CsvTable::open(path, dialect)
        .map(|t| Box::new(t) as Box<dyn ViewportSource>)
        .map_err(|e| e.to_string())
}

fn delimiter_name(delimiter: u8) -> &'static str {
    match delimiter {
        b',' => "Comma",
        b'\t' => "Tab",
        b';' => "Semicolon",
        b'|' => "Pipe",
        _ => "Custom",
    }
}

fn column_name(schema: &Schema, id: ColumnId, encoding: &'static Encoding) -> String {
    schema
        .columns
        .get(id.0 as usize)
        .map(|c| decode_field(&c.name, encoding))
        .unwrap_or_else(|| format!("col{}", id.0))
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
        table: Box<dyn ViewportSource>,
        layout: GridLayout,
        dialect: Dialect,
        encoding: &'static Encoding,
        search: String,
    },
    Failed {
        path: PathBuf,
        error: String,
    },
}

/// Recent-files list, persisted in the OS *config* dir (separate from the index cache in the state dir).
mod recent {
    use std::path::{Path, PathBuf};

    fn file() -> Option<PathBuf> {
        let base = directories::BaseDirs::new()?;
        Some(base.config_dir().join("tableizer").join("recent.txt"))
    }

    pub fn load() -> Vec<PathBuf> {
        let Some(f) = file() else {
            return Vec::new();
        };
        std::fs::read_to_string(f)
            .map(|s| s.lines().map(PathBuf::from).collect())
            .unwrap_or_default()
    }

    pub fn add(recent: &mut Vec<PathBuf>, path: &Path) {
        recent.retain(|p| p != path);
        recent.insert(0, path.to_path_buf());
        recent.truncate(10);
        let Some(f) = file() else {
            return;
        };
        if let Some(dir) = f.parent() {
            let _ = std::fs::create_dir_all(dir);
        }
        let body = recent
            .iter()
            .filter_map(|p| p.to_str())
            .collect::<Vec<_>>()
            .join("\n");
        let _ = std::fs::write(f, body);
    }
}

/// Format a byte count for the cache display.
fn human_bytes(n: u64) -> String {
    const UNITS: [&str; 5] = ["B", "KB", "MB", "GB", "TB"];
    if n < 1024 {
        return format!("{n} B");
    }
    let mut size = n as f64;
    let mut unit = 0;
    while size >= 1024.0 && unit < UNITS.len() - 1 {
        size /= 1024.0;
        unit += 1;
    }
    format!("{size:.1} {}", UNITS[unit])
}

struct TableizerApp {
    view: View,
    recent: Vec<PathBuf>,
}

impl TableizerApp {
    fn new(path: Option<PathBuf>) -> Self {
        let mut app = Self {
            view: View::Empty,
            recent: recent::load(),
        };
        if let Some(path) = path {
            app.open_path(path);
        }
        app
    }

    fn open_path(&mut self, path: PathBuf) {
        let dialect = sniff_file(&path);
        let encoding = detect_encoding(&path);
        self.view = match open_table(&path, dialect) {
            Ok(table) => {
                let layout = GridLayout::new(table.schema().columns.len());
                recent::add(&mut self.recent, &path);
                View::Loaded {
                    path,
                    table,
                    layout,
                    dialect,
                    encoding,
                    search: String::new(),
                }
            }
            Err(error) => View::Failed { path, error },
        };
    }
}

impl eframe::App for TableizerApp {
    fn ui(&mut self, ui: &mut egui::Ui, _frame: &mut eframe::Frame) {
        let mut to_open: Option<PathBuf> = None;

        ui.horizontal(|ui| {
            ui.menu_button("File", |ui| {
                if self.recent.is_empty() {
                    ui.label("(no recent files)");
                }
                for path in &self.recent {
                    if ui.button(path.display().to_string()).clicked() {
                        to_open = Some(path.clone());
                        ui.close();
                    }
                }
            });
            ui.menu_button("Cache", |ui| {
                ui.label(format!(
                    "Index cache: {}",
                    human_bytes(tableizer_core::cache::total_size())
                ));
                if ui.button("Clear cache").clicked() {
                    tableizer_core::cache::clear();
                    ui.close();
                }
            });
        });
        ui.separator();

        match &mut self.view {
            View::Empty => {
                ui.heading("Tableizer");
                ui.label("Open a file via a CLI argument, or pick one from the File menu.");
                if !self.recent.is_empty() {
                    ui.add_space(8.0);
                    ui.label("Recent:");
                    for path in &self.recent {
                        if ui.button(path.display().to_string()).clicked() {
                            to_open = Some(path.clone());
                        }
                    }
                }
            }
            View::Failed { path, error } => {
                ui.heading("Could not open file");
                ui.label(format!("{}: {error}", path.display()));
            }
            View::Loaded {
                path,
                table,
                layout,
                dialect,
                encoding,
                search,
            } => show_table(ui, path, table, layout, dialect, encoding, search),
        }

        if let Some(path) = to_open {
            self.open_path(path);
        }
    }
}

/// Render the loaded table: a toolbar (status + dialect/encoding override + columns) and the grid.
fn show_table(
    ui: &mut egui::Ui,
    path: &Path,
    table: &mut Box<dyn ViewportSource>,
    layout: &mut GridLayout,
    dialect: &mut Dialect,
    encoding: &mut &'static Encoding,
    search: &mut String,
) {
    let (total, indexing) = match table.row_count() {
        RowCount::Exact(n) => (n, false),
        RowCount::AtLeast(n) => (n, true),
    };

    let dialect_before = *dialect;
    ui.horizontal(|ui| {
        ui.heading("Tableizer");
        if indexing {
            ui.label(format!("{}  ·  indexing… ≥ {total} rows", path.display()));
            ui.spinner();
            ui.ctx().request_repaint();
        } else {
            ui.label(format!("{}  ·  {total} rows", path.display()));
        }
        ui.separator();
        dialect_menu(ui, dialect);
        ui.checkbox(&mut dialect.has_header, "Header row");
        encoding_menu(ui, encoding);
        ui.menu_button("Columns", |ui| {
            for (i, shown) in layout.visible.iter_mut().enumerate() {
                ui.checkbox(
                    shown,
                    column_name(table.schema(), ColumnId(i as u32), encoding),
                );
            }
        });
        ui.separator();
        ui.label("Find:");
        ui.add(
            egui::TextEdit::singleline(search)
                .hint_text("substring")
                .desired_width(160.0),
        );
    });
    ui.separator();

    // A dialect change re-opens the file (column count may change), so skip rendering this frame.
    // (Encoding is display-only — no re-open needed.)
    if *dialect != dialect_before {
        if let Ok(reopened) = open_table(path, *dialect) {
            *table = reopened;
            *layout = GridLayout::new(table.schema().columns.len());
        }
        return;
    }

    // Snapshot the (menu-applied) encoding as a plain Copy value for rendering.
    let encoding: &'static Encoding = encoding;

    let displayed = layout.displayed();
    if displayed.is_empty() {
        ui.label("All columns hidden.");
        return;
    }
    let headers: Vec<String> = displayed
        .iter()
        .map(|&c| column_name(table.schema(), c, encoding))
        .collect();

    let table_columns: Vec<egui_table::Column> = (0..displayed.len())
        .map(|_| egui_table::Column::new(140.0).resizable(true))
        .collect();
    let mut delegate = GridDelegate {
        table: table.as_ref(),
        columns: displayed,
        headers,
        encoding,
        search: search.to_lowercase(),
        cache_start: 0,
        cache: Vec::new(),
        pending_reorder: None,
    };

    egui_table::Table::new()
        .id_salt("tableizer-grid")
        .num_rows(total)
        .columns(table_columns)
        .num_sticky_cols(1)
        .headers(vec![egui_table::HeaderRow::new(22.0)])
        .show(ui, &mut delegate);

    if let Some((dragged, before)) = delegate.pending_reorder {
        reorder(&mut layout.order, dragged, before);
    }
}

fn dialect_menu(ui: &mut egui::Ui, dialect: &mut Dialect) {
    ui.menu_button(
        format!("Delimiter: {}", delimiter_name(dialect.delimiter)),
        |ui| {
            for (label, byte) in [
                ("Comma", b','),
                ("Tab", b'\t'),
                ("Semicolon", b';'),
                ("Pipe", b'|'),
            ] {
                if ui
                    .selectable_label(dialect.delimiter == byte, label)
                    .clicked()
                {
                    dialect.delimiter = byte;
                    ui.close();
                }
            }
        },
    );
}

fn encoding_menu(ui: &mut egui::Ui, encoding: &mut &'static Encoding) {
    ui.menu_button(format!("Encoding: {}", encoding.name()), |ui| {
        for choice in [
            encoding_rs::UTF_8,
            encoding_rs::WINDOWS_1252,
            encoding_rs::UTF_16LE,
            encoding_rs::UTF_16BE,
        ] {
            if ui
                .selectable_label(std::ptr::eq(*encoding, choice), choice.name())
                .clicked()
            {
                *encoding = choice;
                ui.close();
            }
        }
    });
}

/// Bridges `egui_table`'s pull-based rendering to the engine: `prepare` fetches the visible row
/// window once, and `cell_ui` reads from that cache — so only visible rows ever cross the seam.
struct GridDelegate<'a> {
    table: &'a dyn ViewportSource,
    /// Visible source columns, in display order.
    columns: Vec<ColumnId>,
    /// Display names aligned with `columns`.
    headers: Vec<String>,
    encoding: &'static Encoding,
    /// Lowercased search query; cells containing it are highlighted (empty = no highlight).
    search: String,
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
        let idx = cell.col_range.start;
        let (Some(&col_id), Some(name)) = (self.columns.get(idx), self.headers.get(idx)) else {
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
                        egui::RichText::new(name.as_str())
                            .strong()
                            .color(egui::Color32::WHITE),
                    )
                    .selectable(false),
                );
            })
            .response;
        if response.hovered() {
            ui.ctx().set_cursor_icon(egui::CursorIcon::Grab);
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
        let Some(cell_value) = value else {
            return;
        };
        let text = decode_field(&cell_value.0, self.encoding);

        // Highlight cells containing the search query.
        if cell_matches(&text, &self.search) {
            ui.painter().rect_filled(
                ui.max_rect(),
                egui::CornerRadius::ZERO,
                egui::Color32::from_rgb(90, 80, 30),
            );
        }
        let response = ui.add(
            egui::Label::new(egui::RichText::new(text.as_str()).monospace())
                .selectable(false)
                .sense(egui::Sense::click()),
        );
        // Right-click a cell to copy its value to the clipboard.
        response.context_menu(|ui| {
            if ui.button("Copy").clicked() {
                ui.ctx().copy_text(text.clone());
                ui.close();
            }
        });
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

    #[test]
    fn decode_field_strips_utf8_bom() {
        let bytes = [0xEF, 0xBB, 0xBF, b'n', b'a', b'm', b'e'];
        assert_eq!(decode_field(&bytes, encoding_rs::UTF_8), "name");
    }

    #[test]
    fn decode_field_handles_windows_1252_smart_quotes() {
        // 0x93/0x94 are “ ” in Windows-1252 but invalid UTF-8.
        let bytes = [0x93, b'h', b'i', 0x94];
        assert_eq!(
            decode_field(&bytes, encoding_rs::WINDOWS_1252),
            "\u{201c}hi\u{201d}"
        );
    }

    #[test]
    fn cell_matches_is_case_insensitive_and_ignores_empty_query() {
        assert!(cell_matches("Hello World", "world"));
        assert!(!cell_matches("Hello", "xyz"));
        assert!(!cell_matches("Hello", "")); // empty query highlights nothing
    }
}
