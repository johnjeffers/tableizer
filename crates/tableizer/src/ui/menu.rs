//! The menu bar (File / Parsing + the Columns-panel toggle), the right-side Columns panel, the
//! Export submenu, and writing exports to disk.

use eframe::egui;
use tableizer_core::{CancellationToken, ColumnId, ExportScope};

use crate::app::{CLOSE_SHORTCUT, OPEN_SHORTCUT, QUIT_SHORTCUT, SETTINGS_SHORTCUT, TableizerApp};
use crate::model::{
    Format, GridLayout, LoadedTable, View, column_name, delimiter_display, delimiter_label,
    parse_delimiter,
};
use crate::theme;
use std::path::PathBuf;

/// The menu bar: File / Parsing on the left, the Columns-panel toggle pinned to the right.
pub(crate) fn menu_bar(ui: &mut egui::Ui, app: &mut TableizerApp, to_open: &mut Option<PathBuf>) {
    ui.menu_button("File", |ui| {
        ui.set_min_width(150.0);
        let open_sc = ui.ctx().format_shortcut(&OPEN_SHORTCUT);
        if ui
            .add(egui::Button::new("Open…").shortcut_text(open_sc))
            .clicked()
        {
            if let Some(path) = rfd::FileDialog::new().pick_file() {
                *to_open = Some(path);
            }
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
        ui.add_enabled_ui(loaded, |ui| {
            ui.menu_button("Export", |ui| {
                if let View::Loaded(loaded) = &app.view {
                    export_menu(ui, loaded);
                }
            });
        });
        ui.separator();
        let settings_sc = ui.ctx().format_shortcut(&SETTINGS_SHORTCUT);
        if ui
            .add(egui::Button::new("Settings…").shortcut_text(settings_sc))
            .clicked()
        {
            app.settings_open = true;
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

    if let View::Loaded(loaded) = &mut app.view {
        // The Parsing menu (delimiter / header / encoding) is delimited-text only — NDJSON and
        // Parquet carry their own schema, so there is nothing to re-parse.
        if loaded.format == Format::Delimited {
            ui.menu_button("Parsing", |ui| parsing_menu(ui, loaded));
        }
    }

    // Columns live in a right-side slide-out panel now; its toggle sits at the right end of the bar.
    if matches!(app.view, View::Loaded(_)) {
        ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
            let open = app.columns_open;
            if columns_toggle(ui, open).clicked() {
                app.columns_open = !open;
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

/// The right-side Columns panel contents: a scrollable visibility list with a pinned reset action.
pub(crate) fn columns_panel(ui: &mut egui::Ui, loaded: &mut LoadedTable) {
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
        ui.add_space(8.0);
        ui.label(
            egui::RichText::new("COLUMNS")
                .text_style(theme::text_style(theme::MENU_SECTION))
                .strong()
                .color(ui.visuals().weak_text_color()),
        );
        ui.add_space(4.0);
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
enum ExportKind {
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

    fn extension(self) -> &'static str {
        match self {
            ExportKind::Csv => "csv",
            ExportKind::Tsv => "tsv",
            ExportKind::Ndjson => "ndjson",
            ExportKind::Parquet => "parquet",
        }
    }
}

/// The Export submenu: one entry per format, each offering the current view or the whole source.
fn export_menu(ui: &mut egui::Ui, loaded: &LoadedTable) {
    let mut request: Option<(ExportScope, ExportKind)> = None;
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
                    request = Some((scope, kind));
                    ui.close();
                }
            }
        });
    }
    if let Some((scope, kind)) = request {
        export_to_file(loaded, scope, kind);
    }
}

/// The Parsing menu: delimiter (presets + custom), header row, and display encoding. Changing the
/// delimiter or header re-opens the file (column structure may change); encoding is display-only.
fn parsing_menu(ui: &mut egui::Ui, loaded: &mut LoadedTable) {
    ui.set_min_width(180.0);

    ui.checkbox(&mut loaded.dialect.has_header, "Header row");

    // Use `CloseOnClickOutside` rather than the menu default (`CloseOnClick`): otherwise the click
    // that focuses the Custom text field counts as a menu click and dismisses the menu before you
    // can type. With this, clicks inside the submenu (presets, the text field) keep it open; a click
    // outside or Esc closes it.
    egui::containers::menu::SubMenuButton::new("Delimiter")
        .config(
            egui::containers::menu::MenuConfig::new()
                .close_behavior(egui::PopupCloseBehavior::CloseOnClickOutside)
                .style(super::wide_menu),
        )
        .ui(ui, |ui| {
            ui.set_min_width(196.0);
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
        });

    ui.menu_button("Encoding", |ui| {
        ui.set_min_width(160.0);
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

/// Export the table to a user-chosen file (native save dialog). Errors are reported to stderr.
fn export_to_file(loaded: &LoadedTable, scope: ExportScope, kind: ExportKind) {
    use tableizer_core::export;

    let table = loaded.table.as_ref();
    let schema = table.schema();
    let columns: Vec<ColumnId> = match scope {
        ExportScope::CurrentView => loaded.layout.displayed(),
        ExportScope::Source => (0..schema.columns.len() as u32).map(ColumnId).collect(),
    };
    // Column names: NDJSON keys / Parquet column names / the CSV header row.
    let names: Vec<Vec<u8>> = columns
        .iter()
        .map(|&c| column_name(schema, c, loaded.encoding).into_bytes())
        .collect();

    let Some(path) = rfd::FileDialog::new()
        .set_file_name(format!("export.{}", kind.extension()))
        .save_file()
    else {
        return; // user cancelled
    };

    let cancel = CancellationToken::new();
    let result = std::fs::File::create(&path)
        .map_err(|e| e.to_string())
        .and_then(|file| {
            let writer = std::io::BufWriter::new(file);
            match kind {
                ExportKind::Csv | ExportKind::Tsv => {
                    let delimiter = if matches!(kind, ExportKind::Tsv) {
                        b'\t'
                    } else {
                        b','
                    };
                    // Write a header row only if the source had one (else names are synthetic).
                    let header = loaded.dialect.has_header.then(|| names.clone());
                    export::export_csv(
                        table,
                        writer,
                        delimiter,
                        scope,
                        &columns,
                        header.as_deref(),
                        &cancel,
                    )
                }
                ExportKind::Ndjson => {
                    export::export_ndjson(table, writer, scope, &columns, &names, &cancel)
                }
                ExportKind::Parquet => {
                    export::export_parquet(table, writer, scope, &columns, &names, &cancel)
                }
            }
            .map_err(|e| e.to_string())
        });
    if let Err(error) = result {
        eprintln!("export failed: {error}");
    }
}
