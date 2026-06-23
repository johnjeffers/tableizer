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
    CancellationToken, Cell, ColumnId, CsvTable, Direction, ExportScope, FilterSpec, InferredType,
    RowCount, RowRange, Schema, SortKey, ViewSpec, ViewportRequest, ViewportSource, parse::Dialect,
};

/// Every UI color in one place. [`theme`] builds the base egui theme from these constants, and the
/// toolbar/grid use the accent constants directly — nothing else hardcodes a color. Retheme the app
/// by editing this module.
mod palette {
    use eframe::egui::{Color32, Visuals};

    /// Window / panel background.
    // pub const BACKGROUND: Color32 = Color32::from_gray(27);
    pub const BACKGROUND: Color32 = Color32::from_gray(248);
    /// Primary text (cells, labels, menus).
    pub const TEXT: Color32 = Color32::from_gray(210);
    /// Column-header bar background.
    pub const HEADER_BG: Color32 = Color32::from_gray(60);
    /// Column-header text.
    pub const HEADER_TEXT: Color32 = Color32::WHITE;
    /// Keyboard-selected row highlight (also egui's text/widget selection fill).
    pub const ROW_SELECTED: Color32 = Color32::from_rgb(40, 55, 85);
    /// Search-match cell highlight.
    pub const SEARCH_MATCH: Color32 = Color32::from_gray(210);
    /// Data-quality warning badge (e.g. ragged rows).
    pub const WARNING: Color32 = Color32::from_rgb(230, 170, 60);
    /// Error text (e.g. invalid filter regex).
    pub const ERROR: Color32 = Color32::from_rgb(230, 100, 100);

    /// The base egui theme, derived from the palette. Starts from the dark preset (for sensible
    /// per-widget-state shading) and overrides the colors that define the app's look.
    pub fn theme() -> Visuals {
        let mut visuals = Visuals::dark();
        visuals.panel_fill = BACKGROUND;
        visuals.window_fill = BACKGROUND;
        visuals.override_text_color = Some(TEXT);
        visuals.selection.bg_fill = ROW_SELECTED;
        visuals
    }
}

fn main() -> eframe::Result<()> {
    let path = std::env::args_os().nth(1).map(PathBuf::from);
    let native_options = eframe::NativeOptions {
        // Initial size on first launch; the `persistence` feature restores the last geometry after.
        viewport: egui::ViewportBuilder::default()
            .with_inner_size([1280.0, 800.0])
            .with_min_inner_size([640.0, 400.0]),
        ..Default::default()
    };
    eframe::run_native(
        "Tableizer",
        native_options,
        Box::new(move |cc| {
            // Theme is derived entirely from `palette`, so every color lives in one place.
            cc.egui_ctx.set_visuals(palette::theme());
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
    /// Number of leftmost displayed columns to freeze (keep on-screen while scrolling right).
    frozen: usize,
}

impl GridLayout {
    fn new(column_count: usize) -> Self {
        Self {
            order: (0..column_count as u32).map(ColumnId).collect(),
            visible: vec![true; column_count],
            frozen: 1,
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

/// Sort/filter UI state for the loaded table.
#[derive(Default)]
struct ViewControls {
    /// The find/filter query (also used for in-place highlight).
    search: String,
    /// Interpret the query as a regex.
    regex: bool,
    /// Show only NON-matching rows.
    invert: bool,
    /// Hide non-matching rows (filter) rather than only highlighting them.
    filter_mode: bool,
    /// Active sort, if any.
    sort: Option<SortKey>,
    /// The `ViewSpec` last applied to the engine (to detect changes).
    applied: ViewSpec,
    /// Last error from applying the view (e.g. invalid regex).
    error: Option<String>,
    /// Keyboard-selected display row (highlighted; moved with arrow/page/home/end keys).
    selected_row: Option<u64>,
}

impl ViewControls {
    /// The view the engine should currently have, derived from the controls.
    fn desired(&self) -> ViewSpec {
        let filter = if self.filter_mode && !self.search.is_empty() {
            Some(FilterSpec {
                query: self.search.clone(),
                regex: self.regex,
                invert: self.invert,
            })
        } else {
            None
        };
        ViewSpec {
            filter,
            sort: self.sort,
        }
    }
}

/// A persisted per-file view: column order/visibility/freeze + sort + filter. Saved to the config
/// dir and reapplied when the same file is reopened.
#[derive(Clone, Default, PartialEq, serde::Serialize, serde::Deserialize)]
struct SavedView {
    order: Vec<u32>,
    visible: Vec<bool>,
    frozen: usize,
    /// (column index, ascending?)
    sort: Option<(u32, bool)>,
    /// (query, regex, invert) — present only when a hide-non-matching filter is active.
    filter: Option<(String, bool, bool)>,
}

impl SavedView {
    /// Snapshot the current layout + controls.
    fn snapshot(layout: &GridLayout, view: &ViewControls) -> Self {
        Self {
            order: layout.order.iter().map(|c| c.0).collect(),
            visible: layout.visible.clone(),
            frozen: layout.frozen,
            sort: view
                .sort
                .map(|s| (s.column.0, s.direction == Direction::Ascending)),
            filter: (view.filter_mode && !view.search.is_empty())
                .then(|| (view.search.clone(), view.regex, view.invert)),
        }
    }

    /// Reapply onto a freshly-opened layout + controls (length-checked against the column count).
    fn apply(&self, layout: &mut GridLayout, view: &mut ViewControls) {
        if self.order.len() == layout.order.len() {
            layout.order = self.order.iter().map(|&c| ColumnId(c)).collect();
        }
        if self.visible.len() == layout.visible.len() {
            layout.visible = self.visible.clone();
        }
        layout.frozen = self.frozen.min(layout.visible.len());
        view.sort = self.sort.map(|(c, asc)| SortKey {
            column: ColumnId(c),
            direction: if asc {
                Direction::Ascending
            } else {
                Direction::Descending
            },
        });
        if let Some((query, regex, invert)) = &self.filter {
            view.search = query.clone();
            view.regex = *regex;
            view.invert = *invert;
            view.filter_mode = true;
        }
    }
}

/// A loaded table and all its per-file UI state.
struct LoadedTable {
    path: PathBuf,
    table: Box<dyn ViewportSource>,
    layout: GridLayout,
    dialect: Dialect,
    encoding: &'static Encoding,
    view: ViewControls,
    saved: SavedView,
}

/// What the window is currently showing.
enum View {
    Empty,
    Loaded(Box<LoadedTable>),
    Failed { path: PathBuf, error: String },
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

/// Saved-view persistence in the OS config dir, keyed by source path.
mod views {
    use super::SavedView;
    use std::collections::hash_map::DefaultHasher;
    use std::hash::{Hash, Hasher};
    use std::path::{Path, PathBuf};

    fn file(source: &Path) -> Option<PathBuf> {
        let base = directories::BaseDirs::new()?;
        let mut hasher = DefaultHasher::new();
        source.hash(&mut hasher);
        let name = format!("{:016x}.json", hasher.finish());
        Some(base.config_dir().join("tableizer").join("views").join(name))
    }

    pub fn load(source: &Path) -> Option<SavedView> {
        let data = std::fs::read(file(source)?).ok()?;
        serde_json::from_slice(&data).ok()
    }

    pub fn save(source: &Path, view: &SavedView) {
        let Some(path) = file(source) else {
            return;
        };
        if let Some(dir) = path.parent() {
            let _ = std::fs::create_dir_all(dir);
        }
        if let Ok(data) = serde_json::to_vec_pretty(view) {
            let _ = std::fs::write(path, data);
        }
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
        // UTF-16 is transcoded to UTF-8 by the engine; single-byte encodings default to UTF-8 here and
        // can be switched to Windows-1252 via the Encoding menu.
        let encoding: &'static Encoding = encoding_rs::UTF_8;
        self.view = match open_table(&path, dialect) {
            Ok(table) => {
                let mut layout = GridLayout::new(table.schema().columns.len());
                let mut view = ViewControls::default();
                // Reapply this file's saved view (column order/visibility/freeze + sort + filter).
                let saved = views::load(&path).unwrap_or_default();
                saved.apply(&mut layout, &mut view);
                recent::add(&mut self.recent, &path);
                View::Loaded(Box::new(LoadedTable {
                    path,
                    table,
                    layout,
                    dialect,
                    encoding,
                    view,
                    saved,
                }))
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
            View::Loaded(loaded) => show_table(ui, loaded),
        }

        if let Some(path) = to_open {
            self.open_path(path);
        }
    }

    /// `App::ui` hands us a `Ui` with no background, so the window background *is* the clear color —
    /// this is what makes [`palette::BACKGROUND`] actually paint the app background.
    fn clear_color(&self, _visuals: &egui::Visuals) -> [f32; 4] {
        palette::BACKGROUND.to_normalized_gamma_f32()
    }
}

/// Render the loaded table: a toolbar (status + dialect/encoding override + columns) and the grid.
fn show_table(ui: &mut egui::Ui, loaded: &mut LoadedTable) {
    let LoadedTable {
        path,
        table,
        layout,
        dialect,
        encoding,
        view,
        saved,
    } = loaded;
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
        let quality = table.data_quality();
        if quality.complete && quality.ragged_rows > 0 {
            ui.colored_label(
                palette::WARNING,
                format!("⚠ {} ragged rows", quality.ragged_rows),
            );
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
        ui.label("Freeze:");
        ui.add(egui::DragValue::new(&mut layout.frozen).range(0..=table.schema().columns.len()));
        ui.menu_button("Export", |ui| {
            let mut request: Option<(ExportScope, u8, &str)> = None;
            ui.label("Current view:");
            if ui.button("CSV…").clicked() {
                request = Some((ExportScope::CurrentView, b',', "csv"));
                ui.close();
            }
            if ui.button("TSV…").clicked() {
                request = Some((ExportScope::CurrentView, b'\t', "tsv"));
                ui.close();
            }
            ui.separator();
            ui.label("Source (all rows & columns):");
            if ui.button("CSV…").clicked() {
                request = Some((ExportScope::Source, b',', "csv"));
                ui.close();
            }
            if ui.button("TSV…").clicked() {
                request = Some((ExportScope::Source, b'\t', "tsv"));
                ui.close();
            }
            if let Some((scope, delimiter, extension)) = request {
                export_to_file(
                    table.as_ref(),
                    dialect,
                    encoding,
                    layout,
                    scope,
                    delimiter,
                    extension,
                );
            }
        });
        ui.separator();
        sort_menu(ui, table.schema(), encoding, &mut view.sort);
        ui.separator();
        ui.label("Find:");
        ui.add(
            egui::TextEdit::singleline(&mut view.search)
                .hint_text("substring or regex")
                .desired_width(160.0),
        );
        ui.checkbox(&mut view.filter_mode, "Hide non-matching");
        ui.checkbox(&mut view.regex, "Regex");
        ui.checkbox(&mut view.invert, "Invert");
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

    // Apply the desired view (filter/sort) to the engine when the controls change.
    let desired = view.desired();
    if desired != view.applied {
        view.applied = desired.clone();
        view.error = match table.set_view(&desired) {
            Ok(()) => None,
            Err(error) => Some(error.to_string()),
        };
    }
    if let Some(error) = &view.error {
        ui.colored_label(palette::ERROR, format!("filter error: {error}"));
    }
    if table.view_status().building {
        ui.horizontal(|ui| {
            ui.spinner();
            ui.label("applying view…");
        });
        ui.ctx().request_repaint();
    }

    // Persist this file's view (column layout + sort + filter) whenever it changes.
    let current = SavedView::snapshot(layout, view);
    if current != *saved {
        views::save(path, &current);
        *saved = current;
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
    let frozen = layout.frozen.min(displayed.len());

    // Keyboard navigation: move the selected row (unless the user is typing in a text field).
    let mut scroll_to: Option<u64> = None;
    let typing = ui.ctx().memory(|m| m.focused().is_some());
    if !typing && total > 0 {
        let last = total - 1;
        const PAGE: u64 = 20;
        ui.input(|i| {
            let current = view.selected_row;
            let next = if i.key_pressed(egui::Key::ArrowDown) {
                Some(current.map_or(0, |r| (r + 1).min(last)))
            } else if i.key_pressed(egui::Key::ArrowUp) {
                Some(current.map_or(0, |r| r.saturating_sub(1)))
            } else if i.key_pressed(egui::Key::PageDown) {
                Some(current.map_or(0, |r| (r + PAGE).min(last)))
            } else if i.key_pressed(egui::Key::PageUp) {
                Some(current.map_or(0, |r| r.saturating_sub(PAGE)))
            } else if i.key_pressed(egui::Key::Home) {
                Some(0)
            } else if i.key_pressed(egui::Key::End) {
                Some(last)
            } else {
                None
            };
            if let Some(next) = next {
                view.selected_row = Some(next);
                scroll_to = Some(next);
            }
        });
    }

    let mut delegate = GridDelegate {
        table: table.as_ref(),
        columns: displayed,
        headers,
        encoding,
        search: view.search.to_lowercase(),
        selected_row: view.selected_row,
        cache_start: 0,
        cache: Vec::new(),
        pending_reorder: None,
    };

    let mut grid = egui_table::Table::new()
        .id_salt("tableizer-grid")
        .num_rows(total)
        .columns(table_columns)
        .num_sticky_cols(frozen)
        .headers(vec![egui_table::HeaderRow::new(22.0)]);
    if let Some(row) = scroll_to {
        grid = grid.scroll_to_row(row, Some(egui::Align::Center));
    }
    grid.show(ui, &mut delegate);

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
        for choice in [encoding_rs::UTF_8, encoding_rs::WINDOWS_1252] {
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

fn sort_menu(
    ui: &mut egui::Ui,
    schema: &Schema,
    encoding: &'static Encoding,
    sort: &mut Option<SortKey>,
) {
    let label = match sort {
        Some(s) => format!(
            "Sort: col{} {}",
            s.column.0,
            if s.direction == Direction::Ascending {
                "asc"
            } else {
                "desc"
            }
        ),
        None => "Sort: none".to_string(),
    };
    ui.menu_button(label, |ui| {
        if ui.button("(none)").clicked() {
            *sort = None;
            ui.close();
        }
        ui.separator();
        for i in 0..schema.columns.len() as u32 {
            ui.horizontal(|ui| {
                ui.label(column_name(schema, ColumnId(i), encoding));
                if ui.small_button("asc").clicked() {
                    *sort = Some(SortKey {
                        column: ColumnId(i),
                        direction: Direction::Ascending,
                    });
                    ui.close();
                }
                if ui.small_button("desc").clicked() {
                    *sort = Some(SortKey {
                        column: ColumnId(i),
                        direction: Direction::Descending,
                    });
                    ui.close();
                }
            });
        }
    });
}

/// Export the table to a user-chosen file (native save dialog). Errors are reported to stderr.
fn export_to_file(
    table: &dyn ViewportSource,
    dialect: &Dialect,
    encoding: &'static Encoding,
    layout: &GridLayout,
    scope: ExportScope,
    delimiter: u8,
    extension: &str,
) {
    let schema = table.schema();
    let columns: Vec<ColumnId> = match scope {
        ExportScope::CurrentView => layout.displayed(),
        ExportScope::Source => (0..schema.columns.len() as u32).map(ColumnId).collect(),
    };
    // Write a header row only if the source had one (otherwise column names are synthetic).
    let header: Option<Vec<Vec<u8>>> = dialect.has_header.then(|| {
        columns
            .iter()
            .map(|&c| column_name(schema, c, encoding).into_bytes())
            .collect()
    });
    let Some(path) = rfd::FileDialog::new()
        .set_file_name(format!("export.{extension}"))
        .save_file()
    else {
        return; // user cancelled
    };
    let result = std::fs::File::create(&path)
        .map_err(|e| e.to_string())
        .and_then(|file| {
            tableizer_core::export::export_csv(
                table,
                std::io::BufWriter::new(file),
                delimiter,
                scope,
                &columns,
                header.as_deref(),
                &CancellationToken::new(),
            )
            .map_err(|e| e.to_string())
        });
    if let Err(error) = result {
        eprintln!("export failed: {error}");
    }
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
    /// Keyboard-selected display row to highlight, if any.
    selected_row: Option<u64>,
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
        ui.painter()
            .rect_filled(ui.max_rect(), egui::CornerRadius::ZERO, palette::HEADER_BG);
        // The header is a drag source carrying its column id, and a drop target for reordering.
        let response = ui
            .dnd_drag_source(egui::Id::new(("tz-col", col_id.0)), col_id, |ui| {
                ui.set_min_width(ui.available_width()); // make the whole header cell draggable
                ui.add(
                    egui::Label::new(
                        egui::RichText::new(name.as_str())
                            .strong()
                            .color(palette::HEADER_TEXT),
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

        // Highlight the keyboard-selected row.
        if Some(cell.row_nr) == self.selected_row {
            ui.painter().rect_filled(
                ui.max_rect(),
                egui::CornerRadius::ZERO,
                palette::ROW_SELECTED,
            );
        }
        // Highlight cells containing the search query (over the selection).
        if cell_matches(&text, &self.search) {
            ui.painter().rect_filled(
                ui.max_rect(),
                egui::CornerRadius::ZERO,
                palette::SEARCH_MATCH,
            );
        }
        // Right-align numeric columns; show empty (null) cells as a faint placeholder.
        let col_id = self
            .columns
            .get(cell.col_nr)
            .copied()
            .unwrap_or(ColumnId(0));
        let numeric = matches!(
            self.table
                .schema()
                .columns
                .get(col_id.0 as usize)
                .map(|c| c.inferred),
            Some(InferredType::Integer) | Some(InferredType::Float)
        );
        let label = if text.is_empty() {
            egui::Label::new(egui::RichText::new("·").weak())
        } else {
            egui::Label::new(egui::RichText::new(text.as_str()).monospace())
        }
        .selectable(false)
        .sense(egui::Sense::click());
        let layout = if numeric {
            egui::Layout::right_to_left(egui::Align::Center)
        } else {
            egui::Layout::left_to_right(egui::Align::Center)
        };
        let response = ui.with_layout(layout, |ui| ui.add(label)).inner;
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
