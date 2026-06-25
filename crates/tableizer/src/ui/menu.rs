//! The menu bar (File / Parsing + the Columns-panel toggle), the right-side Columns panel, and the
//! Export submenu (which only records the request; the app runs the write off the UI thread).

use eframe::egui;
use tableizer_core::{ColumnId, ExportScope, RowCount};

use crate::app::{CLOSE_SHORTCUT, PanelTab, QUIT_SHORTCUT, SETTINGS_SHORTCUT, TableizerApp};
use crate::model::{
    GridLayout, LoadedTable, View, column_name, delimiter_display, delimiter_label, parse_delimiter,
};
use crate::theme;
use std::path::PathBuf;

/// A requested export, set by the Export submenu and carried out (off the UI thread) by the app.
pub(crate) type ExportRequest = (ExportScope, ExportKind);

/// The menu bar: File / Parsing on the left, the Columns-panel toggle pinned to the right.
pub(crate) fn menu_bar(
    ui: &mut egui::Ui,
    app: &mut TableizerApp,
    to_open: &mut Option<PathBuf>,
    to_export: &mut Option<ExportRequest>,
) {
    ui.menu_button("File", |ui| {
        ui.set_min_width(150.0);
        // The built-in browser (start screen) replaces the OS file picker, so there's no "Open…".
        // "Browse Files…" returns to that start screen (closing the current file — safe, read-only).
        if ui.button("Browse Files…").clicked() {
            app.view = View::Empty;
            ui.close();
        }
        ui.menu_button("Open Recent", |ui| {
            if app.recent.is_empty() {
                ui.label("(none)");
            }
            for path in &app.recent {
                let name = path
                    .file_name()
                    .map(|n| n.to_string_lossy().into_owned())
                    .unwrap_or_else(|| path.display().to_string());
                if ui
                    .button(name)
                    .on_hover_text(path.display().to_string())
                    .clicked()
                {
                    *to_open = Some(path.clone());
                    ui.close();
                }
            }
            if !app.recent.is_empty() {
                ui.separator();
                if ui.button("Clear Recents").clicked() {
                    crate::persist::recent::clear(&mut app.recent);
                    ui.close();
                }
            }
        });
        let loaded = matches!(app.view, View::Loaded(_));
        // Export is gated on a *complete* index: exporting mid-build would silently write only the
        // rows indexed so far. Parquet/JSON-array are exact from open, so they're never blocked.
        let exportable =
            matches!(&app.view, View::Loaded(l) if matches!(l.table.row_count(), RowCount::Exact(_)));
        let export = ui.add_enabled_ui(exportable, |ui| {
            ui.menu_button("Export", |ui| export_menu(ui, to_export));
        });
        if loaded && !exportable {
            export
                .response
                .on_hover_text("Available once indexing finishes");
        }
        ui.separator();
        let settings_sc = ui.ctx().format_shortcut(&SETTINGS_SHORTCUT);
        if ui
            .add(egui::Button::new("Settings…").shortcut_text(settings_sc))
            .clicked()
        {
            app.panel_open = true;
            app.panel_tab = PanelTab::Settings;
            ui.close();
        }
        ui.separator();
        let close_sc = ui.ctx().format_shortcut(&CLOSE_SHORTCUT);
        if ui
            .add_enabled(
                loaded,
                egui::Button::new("Close File").shortcut_text(close_sc),
            )
            .clicked()
        {
            app.view = View::Empty;
            ui.close();
        }
        let quit_sc = ui.ctx().format_shortcut(&QUIT_SHORTCUT);
        if ui
            .add(egui::Button::new("Quit").shortcut_text(quit_sc))
            .clicked()
        {
            ui.ctx().send_viewport_cmd(egui::ViewportCommand::Close);
            ui.close();
        }
    });

    // The right-side panel holds Columns / Parsing / Settings tabs now; the bar's right-end icon
    // toggles it on the Columns tab (Parsing moved into the panel; it's a tab there when delimited).
    if matches!(app.view, View::Loaded(_)) {
        ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
            let on_columns = app.panel_open && app.panel_tab == PanelTab::Columns;
            if columns_toggle(ui, on_columns).clicked() {
                if on_columns {
                    app.panel_open = false;
                } else {
                    app.panel_open = true;
                    app.panel_tab = PanelTab::Columns;
                }
            }
        });
    }
}

/// The menu-bar toggle for the Columns panel: a painted "table columns" icon — two columns of rule
/// lines, mapped from a 24×24 viewBox (shapes, not a font-dependent glyph — see the `ui` module
/// invariant). Accent-coloured + backed when the panel is open.
fn columns_toggle(ui: &mut egui::Ui, open: bool) -> egui::Response {
    let (rect, response) = ui.allocate_exact_size(
        egui::vec2(30.0, ui.available_height()),
        egui::Sense::click(),
    );
    if response.hovered() {
        ui.ctx().set_cursor_icon(egui::CursorIcon::PointingHand);
    }

    let color = if open {
        ui.visuals().selection.bg_fill
    } else if response.hovered() {
        ui.visuals().text_color()
    } else {
        ui.visuals().weak_text_color()
    };
    let icon = egui::Rect::from_center_size(rect.center(), egui::vec2(18.0, 18.0));
    let painter = ui.painter();
    if open || response.hovered() {
        // Subtle button background so it reads as interactive / active.
        painter.rect_filled(
            rect.shrink2(egui::vec2(3.0, 4.0)),
            egui::CornerRadius::same(4),
            ui.visuals().widgets.hovered.bg_fill,
        );
    }
    // A left and right column of four rule lines each (viewBox 0..24 → `icon`).
    let stroke = egui::Stroke::new(1.5, color);
    let at =
        |x: f32, y: f32| icon.min + egui::vec2(x / 24.0 * icon.width(), y / 24.0 * icon.height());
    for y in [6.0, 10.0, 14.0, 18.0] {
        painter.line_segment([at(4.0, y), at(9.5, y)], stroke);
        painter.line_segment([at(14.5, y), at(20.0, y)], stroke);
    }

    response.on_hover_text(if open { "Hide columns" } else { "Show columns" })
}

/// A small uppercase, muted section heading inside the right panel.
fn panel_heading(ui: &mut egui::Ui, title: &str) {
    ui.label(
        egui::RichText::new(title)
            .text_style(theme::text_style(theme::MENU_SECTION))
            .strong()
            .color(ui.visuals().weak_text_color()),
    );
    ui.add_space(4.0);
}

/// The Columns panel tab: Select All/None, a scrollable per-column visibility list, and a pinned
/// "Reset columns & view" action.
pub(crate) fn columns_tab(ui: &mut egui::Ui, loaded: &mut LoadedTable) {
    egui::Panel::bottom("tz_columns_reset").show_inside(ui, |ui| {
        ui.add_space(6.0);
        if ui
            .add_sized(
                [ui.available_width(), 24.0],
                egui::Button::new("Reset columns & view"),
            )
            .clicked()
        {
            reset_view(loaded, ui.ctx());
        }
        ui.add_space(6.0);
    });
    egui::CentralPanel::default().show_inside(ui, |ui| {
        ui.add_space(6.0);
        ui.horizontal(|ui| {
            let w = (ui.available_width() - ui.spacing().item_spacing.x) / 2.0;
            if ui
                .add_sized([w, 22.0], egui::Button::new("Select All"))
                .clicked()
            {
                loaded.layout.visible.iter_mut().for_each(|v| *v = true);
            }
            if ui
                .add_sized([w, 22.0], egui::Button::new("Select None"))
                .clicked()
            {
                loaded.layout.visible.iter_mut().for_each(|v| *v = false);
            }
        });
        ui.add_space(6.0);
        ui.separator();
        ui.add_space(2.0);
        egui::ScrollArea::vertical()
            .auto_shrink([false, false])
            .show(ui, |ui| {
                for (i, shown) in loaded.layout.visible.iter_mut().enumerate() {
                    ui.checkbox(
                        shown,
                        column_name(loaded.table.schema(), ColumnId(i as u32), loaded.encoding),
                    );
                }
            });
    });
}

/// Reset column width/order/visibility/freeze, sort, and find/filter back to defaults.
fn reset_view(loaded: &mut LoadedTable, ctx: &egui::Context) {
    loaded.layout = GridLayout::new(loaded.table.schema().columns.len());
    loaded.view.sort = None;
    loaded.view.search.clear();
    loaded.view.regex = false;
    loaded.view.invert = false;
    loaded.view.filter_mode = false;
    loaded.view.selected = None;
    // Drop egui_table's persisted column widths so they return to their initial size.
    ctx.data_mut(|d| d.remove_by_type::<egui_table::TableState>());
}

/// One of the formats the Export submenu offers.
#[derive(Clone, Copy)]
pub(crate) enum ExportKind {
    Csv,
    Tsv,
    Ndjson,
    Parquet,
}

impl ExportKind {
    fn label(self) -> &'static str {
        match self {
            ExportKind::Csv => "CSV",
            ExportKind::Tsv => "TSV",
            ExportKind::Ndjson => "NDJSON",
            ExportKind::Parquet => "Parquet",
        }
    }

    /// File extension for the save dialog's default name.
    pub(crate) fn extension(self) -> &'static str {
        match self {
            ExportKind::Csv => "csv",
            ExportKind::Tsv => "tsv",
            ExportKind::Ndjson => "ndjson",
            ExportKind::Parquet => "parquet",
        }
    }
}

/// The Export submenu: one entry per format, each offering the current view or the whole source. The
/// chosen export is recorded in `to_export`; the app runs it off the UI thread (`app.rs`).
fn export_menu(ui: &mut egui::Ui, to_export: &mut Option<ExportRequest>) {
    for kind in [
        ExportKind::Csv,
        ExportKind::Tsv,
        ExportKind::Ndjson,
        ExportKind::Parquet,
    ] {
        ui.menu_button(kind.label(), |ui| {
            ui.set_min_width(180.0);
            for (label, scope) in [
                ("Current view…", ExportScope::CurrentView),
                ("Source (all data)…", ExportScope::Source),
            ] {
                if ui.button(label).clicked() {
                    *to_export = Some((scope, kind));
                    ui.close();
                }
            }
        });
    }
}

/// The Parsing panel tab (delimited text only): header toggle, delimiter (auto / presets / custom),
/// and display encoding. Changing the delimiter or header re-opens the file (column structure may
/// change); encoding is display-only.
pub(crate) fn parsing_tab(ui: &mut egui::Ui, loaded: &mut LoadedTable) {
    egui::ScrollArea::vertical()
        .auto_shrink([false, false])
        .show(ui, |ui| {
            ui.add_space(6.0);
            ui.checkbox(&mut loaded.dialect.has_header, "Header row");

            ui.add_space(12.0);
            panel_heading(ui, "DELIMITER");
            // Auto is the default; presets/custom are explicit overrides for when sniffing guessed wrong.
            let detected = loaded.detected_delimiter;
            if ui
                .selectable_label(
                    loaded.delimiter_auto,
                    format!("Auto ({})", delimiter_label(detected)),
                )
                .clicked()
            {
                loaded.dialect.delimiter = detected;
                loaded.delimiter_auto = true;
                loaded.delimiter_input = delimiter_display(detected);
            }
            for (label, byte) in [
                ("Comma", b','),
                ("Pipe", b'|'),
                ("Semicolon", b';'),
                ("Tab", b'\t'),
            ] {
                let selected = !loaded.delimiter_auto && loaded.dialect.delimiter == byte;
                if ui.selectable_label(selected, label).clicked() {
                    loaded.dialect.delimiter = byte;
                    loaded.delimiter_auto = false;
                    loaded.delimiter_input = delimiter_display(byte);
                }
            }
            ui.add_space(6.0);
            ui.horizontal(|ui| {
                ui.label("Custom:");
                ui.add(
                    egui::TextEdit::singleline(&mut loaded.delimiter_input)
                        .desired_width(54.0)
                        .hint_text(": or 0x01"),
                );
                if ui.button("Set").clicked()
                    && let Some(byte) = parse_delimiter(&loaded.delimiter_input)
                {
                    loaded.dialect.delimiter = byte;
                    loaded.delimiter_auto = false;
                }
            });

            ui.add_space(12.0);
            panel_heading(ui, "ENCODING");
            for choice in [encoding_rs::UTF_8, encoding_rs::WINDOWS_1252] {
                if ui
                    .selectable_label(std::ptr::eq(loaded.encoding, choice), choice.name())
                    .clicked()
                {
                    loaded.encoding = choice;
                }
            }
        });
}
