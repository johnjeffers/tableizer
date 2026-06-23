//! The Settings window: appearance (theme / accent / density), the table-font picker with live
//! preview, and the index-cache size/clear control. Everything applies live and persists on change.

use eframe::egui;

use crate::fonts;
use crate::theme;

use super::menu_choice;

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

/// A section heading inside the Settings window.
fn settings_section(ui: &mut egui::Ui, title: &str) {
    ui.add_space(2.0);
    ui.label(
        egui::RichText::new(title.to_uppercase())
            .strong()
            .text_style(theme::text_style(theme::SETTINGS_HEADING)),
    );
    ui.add_space(6.0);
}

/// A horizontal segmented control (pill row) for a small set of mutually-exclusive choices.
fn segmented<T: Copy + PartialEq>(ui: &mut egui::Ui, current: &mut T, options: &[(T, &str)]) {
    ui.horizontal(|ui| {
        ui.spacing_mut().item_spacing.x = 4.0;
        for (value, label) in options {
            if ui.selectable_label(*current == *value, *label).clicked() {
                *current = *value;
            }
        }
    });
}

/// A row of clickable accent color swatches; the selected one is ringed.
fn accent_swatches(ui: &mut egui::Ui, current: &mut theme::Accent) {
    ui.horizontal(|ui| {
        ui.spacing_mut().item_spacing.x = 10.0;
        let ring = ui.visuals().weak_text_color();
        for accent in theme::Accent::ALL {
            let (rect, response) =
                ui.allocate_exact_size(egui::vec2(20.0, 20.0), egui::Sense::click());
            let center = rect.center();
            ui.painter().circle_filled(center, 7.0, accent.color());
            if *current == accent {
                ui.painter()
                    .circle_stroke(center, 9.5, egui::Stroke::new(2.0, accent.color()));
            } else if response.hovered() {
                ui.painter()
                    .circle_stroke(center, 9.5, egui::Stroke::new(1.5, ring));
            }
            if response.on_hover_text(accent.label()).clicked() {
                *current = accent;
            }
        }
    });
}

/// The Settings window (non-modal, singleton): appearance (theme/accent/density) and the table font
/// (size + live preview + searchable family list). Everything applies live and persists on change.
/// Toggled from the menu bar and ⌘/Ctrl+, ; closed by its ✕ or Esc.
pub(crate) fn settings_window(
    ctx: &egui::Context,
    open: &mut bool,
    settings: &mut theme::Settings,
    families: &[(String, bool)],
    font_search: &mut String,
    mono_only: &mut bool,
) {
    let mut window_open = *open;
    egui::Window::new("Settings")
        .open(&mut window_open)
        .resizable(false)
        .collapsible(false)
        .default_width(360.0)
        .show(ctx, |ui| {
            settings_section(ui, "Appearance");
            egui::Grid::new("settings_appearance")
                .num_columns(2)
                .spacing([24.0, 12.0])
                .min_col_width(64.0)
                .show(ui, |ui| {
                    ui.label("Theme");
                    segmented(
                        ui,
                        &mut settings.mode,
                        &theme::Mode::ALL.map(|m| (m, m.label())),
                    );
                    ui.end_row();

                    ui.label("Accent");
                    accent_swatches(ui, &mut settings.accent);
                    ui.end_row();

                    ui.label("Density");
                    segmented(
                        ui,
                        &mut settings.density,
                        &theme::Density::ALL.map(|d| (d, d.label())),
                    );
                    ui.end_row();
                });

            ui.add_space(12.0);
            settings_section(ui, "Table font");
            ui.horizontal(|ui| {
                ui.label("Size");
                ui.spacing_mut().item_spacing.x = 4.0;
                let h = ui.spacing().interact_size.y;
                if ui
                    .button(egui::RichText::new("−").text_style(theme::text_style(theme::STEPPER)))
                    .clicked()
                {
                    settings.table_font_size = (settings.table_font_size - 0.5).max(8.0);
                }
                ui.add_sized(
                    egui::vec2(48.0, h),
                    egui::Label::new(format!("{:.1} pt", settings.table_font_size)),
                );
                if ui
                    .button(egui::RichText::new("+").text_style(theme::text_style(theme::STEPPER)))
                    .clicked()
                {
                    settings.table_font_size = (settings.table_font_size + 0.5).min(32.0);
                }
                ui.add_space(12.0);
                let weak = ui.visuals().weak_text_color();
                let current = settings
                    .table_font
                    .clone()
                    .unwrap_or_else(|| "App font".to_owned());
                ui.label(egui::RichText::new(current).color(weak));
            });
            ui.add_space(6.0);

            // Live preview, rendered in the chosen table font + size.
            let preview_font = egui::FontId::new(
                settings.table_font_size,
                egui::FontFamily::Name(fonts::TABLE_FONT.into()),
            );
            egui::Frame::group(ui.style()).show(ui, |ui| {
                ui.set_width(ui.available_width());
                ui.label(
                    egui::RichText::new("The quick brown fox  ·  0123456789").font(preview_font),
                );
            });
            ui.add_space(8.0);

            ui.horizontal(|ui| {
                // Reserve room for the checkbox on the right; the search field fills the rest.
                let search_w = (ui.available_width() - 108.0).max(80.0);
                ui.add(
                    egui::TextEdit::singleline(font_search)
                        .hint_text("Search fonts…")
                        .desired_width(search_w),
                );
                ui.checkbox(mono_only, "Monospace");
            });
            ui.add_space(4.0);
            egui::Frame::group(ui.style()).show(ui, |ui| {
                egui::ScrollArea::vertical()
                    .max_height(170.0)
                    .auto_shrink([false, false])
                    .show(ui, |ui| {
                        let row_w = ui.available_width();
                        if menu_choice(
                            ui,
                            row_w,
                            settings.table_font.is_none(),
                            None,
                            "App font (default)",
                        ) {
                            settings.table_font = None;
                        }
                        let query = font_search.to_lowercase();
                        for (family, is_mono) in families {
                            if *mono_only && !*is_mono {
                                continue;
                            }
                            if !query.is_empty() && !family.to_lowercase().contains(&query) {
                                continue;
                            }
                            let selected = settings.table_font.as_deref() == Some(family.as_str());
                            if menu_choice(ui, row_w, selected, None, family) {
                                settings.table_font = Some(family.clone());
                            }
                        }
                    });
            });

            ui.add_space(12.0);
            settings_section(ui, "Index cache");
            ui.label(
                egui::RichText::new(format!(
                    "Size on disk: {}",
                    human_bytes(tableizer_core::cache::total_size())
                ))
                .color(ui.visuals().weak_text_color()),
            );
            ui.add_space(4.0);
            if ui.button("Clear cache").clicked() {
                tableizer_core::cache::clear();
            }
        });
    *open = window_open;
}
