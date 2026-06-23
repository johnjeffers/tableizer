//! The menu bar (File / Parsing / Columns), the Export submenu, and writing exports to disk.

use eframe::egui;
use encoding_rs::Encoding;
use tableizer_core::{CancellationToken, ColumnId, ExportScope, ViewportSource, parse::Dialect};

use crate::app::{QUIT_SHORTCUT, SETTINGS_SHORTCUT, TableizerApp};
use crate::model::{
    GridLayout, LoadedTable, View, column_name, delimiter_display, delimiter_label, parse_delimiter,
};
use crate::theme;
use std::path::PathBuf;

/// The menu bar (File / Parsing / Columns).
pub(crate) fn menu_bar(ui: &mut egui::Ui, app: &mut TableizerApp, to_open: &mut Option<PathBuf>) {
    ui.menu_button("File", |ui| {
        ui.set_min_width(150.0);
        if ui.button("Open…").clicked() {
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
        if ui
            .add_enabled(loaded, egui::Button::new("Close File"))
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
        ui.menu_button("Parsing", |ui| parsing_menu(ui, loaded));
        ui.menu_button("Columns", |ui| {
            ui.set_min_width(190.0);
            menu_section(ui, "VISIBLE");
            for (i, shown) in loaded.layout.visible.iter_mut().enumerate() {
                ui.checkbox(
                    shown,
                    column_name(loaded.table.schema(), ColumnId(i as u32), loaded.encoding),
                );
            }
            menu_section(ui, "RESET");
            if ui.button("Reset columns & view").clicked() {
                // Column width/order/visibility/freeze, sort, and find/filter back to defaults.
                loaded.layout = GridLayout::new(loaded.table.schema().columns.len());
                loaded.view.sort = None;
                loaded.view.search.clear();
                loaded.view.regex = false;
                loaded.view.invert = false;
                loaded.view.filter_mode = false;
                loaded.view.selected = None;
                // Drop egui_table's persisted column widths so they return to their initial size.
                ui.ctx()
                    .data_mut(|d| d.remove_by_type::<egui_table::TableState>());
                ui.close();
            }
        });
    }
}

/// The Export submenu: current view or source, as CSV or TSV, to a chosen file.
fn export_menu(ui: &mut egui::Ui, loaded: &LoadedTable) {
    ui.set_min_width(186.0);
    let mut request: Option<(ExportScope, u8, &str)> = None;
    menu_section(ui, "CURRENT VIEW");
    if ui.button("as CSV…").clicked() {
        request = Some((ExportScope::CurrentView, b',', "csv"));
        ui.close();
    }
    if ui.button("as TSV…").clicked() {
        request = Some((ExportScope::CurrentView, b'\t', "tsv"));
        ui.close();
    }
    menu_section(ui, "SOURCE (ALL DATA)");
    if ui.button("as CSV…").clicked() {
        request = Some((ExportScope::Source, b',', "csv"));
        ui.close();
    }
    if ui.button("as TSV…").clicked() {
        request = Some((ExportScope::Source, b'\t', "tsv"));
        ui.close();
    }
    if let Some((scope, delimiter, extension)) = request {
        export_to_file(
            loaded.table.as_ref(),
            &loaded.dialect,
            loaded.encoding,
            &loaded.layout,
            scope,
            delimiter,
            extension,
        );
    }
}

/// A small, uppercase, muted section header inside a dropdown menu.
fn menu_section(ui: &mut egui::Ui, title: &str) {
    let color = ui.visuals().weak_text_color();
    ui.add_space(6.0);
    ui.label(
        egui::RichText::new(title)
            .text_style(theme::text_style(theme::MENU_SECTION))
            .strong()
            .color(color),
    );
    ui.add_space(1.0);
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
                .close_behavior(egui::PopupCloseBehavior::CloseOnClickOutside),
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
