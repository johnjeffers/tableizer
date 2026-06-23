//! The egui rendering layer: top-level window panels (toolbar, status bar, empty state) and — via
//! submodules — the menu bar, Settings window, and data grid.
//!
//! Invariant: menu and list item text is rendered with native egui widgets (`Button`,
//! `SelectableLabel`, `Label`, …), never hand-painted with `Painter::text`. Text painted directly
//! into a menu popup renders at a different size than the surrounding native widgets (a real bug we
//! hit once), so the data grid — which has no native-widget equivalent — is the only place allowed
//! to paint text by hand.

mod grid;
mod menu;
mod settings;

pub(crate) use grid::grid;
pub(crate) use menu::menu_bar;
pub(crate) use settings::settings_window;

use std::path::PathBuf;

use eframe::egui;
use tableizer_core::RowCount;

use crate::model::LoadedTable;
use crate::theme;

/// egui's standard menu look (`menu_style`) with roomier horizontal item padding, so the highlight
/// behind a hovered/selected item — and the menu-bar buttons — isn't cramped against the text.
/// Applied to the menu bar and every menu/submenu popup; vertical padding is left as `menu_style`
/// sets it.
pub(crate) fn wide_menu(style: &mut egui::Style) {
    egui::containers::menu::menu_style(style);
    style.spacing.button_padding.x = 6.0;
}

/// The toolbar: the find/filter controls. `focus_find` requests focus on the Find field (⌘/Ctrl+F).
pub(crate) fn toolbar(ui: &mut egui::Ui, loaded: &mut LoadedTable, focus_find: bool) {
    let LoadedTable { view, .. } = loaded;
    ui.horizontal_wrapped(|ui| {
        ui.label("Find:");
        let find = ui.add(
            egui::TextEdit::singleline(&mut view.search)
                .hint_text("substring or regex")
                .desired_width(180.0),
        );
        if focus_find {
            find.request_focus();
        }
        ui.checkbox(&mut view.filter_mode, "Show matches only");
        ui.checkbox(&mut view.regex, "Use regex");
        ui.checkbox(&mut view.case_sensitive, "Match case");
        ui.checkbox(&mut view.invert, "Invert search");
    });
}

/// The bottom status bar: path, row count, indexing/view-build progress, data-quality, errors.
pub(crate) fn status_bar(ui: &mut egui::Ui, loaded: &LoadedTable, palette: &theme::Palette) {
    let (total, indexing) = match loaded.table.row_count() {
        RowCount::Exact(n) => (n, false),
        RowCount::AtLeast(n) => (n, true),
    };
    ui.horizontal(|ui| {
        ui.label(loaded.path.display().to_string());
        ui.separator();
        if indexing {
            ui.label(format!("indexing… ≥ {} rows", fmt_count(total)));
            ui.spinner();
            ui.ctx().request_repaint();
        } else {
            ui.label(format!("{} rows", fmt_count(total)));
        }
        let quality = loaded.table.data_quality();
        if quality.complete && quality.ragged_rows > 0 {
            ui.separator();
            ui.colored_label(
                palette.warning,
                format!("⚠ {} ragged rows", fmt_count(quality.ragged_rows)),
            );
        }
        if loaded.table.view_status().building {
            ui.separator();
            ui.spinner();
            ui.label("applying view…");
            ui.ctx().request_repaint();
        }
        if let Some(error) = &loaded.view.error {
            ui.separator();
            ui.colored_label(palette.error, format!("filter error: {error}"));
        }
        if let Some(span) = loaded.view.selected {
            ui.separator();
            let weak = ui.visuals().weak_text_color();
            let label = if span.len() == 1 {
                format!("row {} selected", fmt_count(span.lo() + 1))
            } else {
                format!(
                    "rows {}–{} selected ({})",
                    fmt_count(span.lo() + 1),
                    fmt_count(span.hi() + 1),
                    fmt_count(span.len())
                )
            };
            ui.label(egui::RichText::new(label).color(weak));
        }
    });
}

/// The empty (no file) view: an Open button (and recent files, if any). Mirrors File ▸ Open…, so a
/// file can always be chosen from within the app — no CLI argument required.
pub(crate) fn empty_view(ui: &mut egui::Ui, recent: &[PathBuf], to_open: &mut Option<PathBuf>) {
    ui.add_space(40.0);
    ui.vertical_centered(|ui| {
        ui.label("Open a delimited file to get started.");
        ui.add_space(12.0);
        if ui.button("Open File…").clicked()
            && let Some(path) = rfd::FileDialog::new().pick_file()
        {
            *to_open = Some(path);
        }
        if !recent.is_empty() {
            ui.add_space(16.0);
            ui.label(egui::RichText::new("RECENT").weak());
            ui.add_space(4.0);
            for path in recent {
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
                }
            }
        }
    });
}

/// Format a row count with thousands separators.
fn fmt_count(n: u64) -> String {
    let digits = n.to_string();
    let bytes = digits.as_bytes();
    let mut out = String::with_capacity(digits.len() + digits.len() / 3);
    for (i, b) in bytes.iter().enumerate() {
        if i > 0 && (bytes.len() - i).is_multiple_of(3) {
            out.push(',');
        }
        out.push(*b as char);
    }
    out
}
