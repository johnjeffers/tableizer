//! The eframe application: window state, theme/font installation, file opening, and the per-frame
//! update loop that lays out the menu bar, toolbar, table, and status bar.

use std::path::PathBuf;

use eframe::egui;
use encoding_rs::Encoding;
use tableizer_core::parse::Dialect;

use crate::model::{
    Format, GridLayout, LoadedTable, SavedView, View, ViewControls, delimiter_display,
    detect_format, open_table, sniff_file,
};
use crate::persist::{prefs, recent, views};
use crate::ui::{
    columns_tab, empty_view, grid, menu_bar, parsing_tab, settings_tab, status_bar, toolbar,
};
use crate::{fonts, theme};

pub(crate) struct TableizerApp {
    pub(crate) view: View,
    pub(crate) recent: Vec<PathBuf>,
    theme: theme::Settings,
    /// `(settings, system_dark)` last pushed to egui — restyle only when this changes.
    applied_theme: Option<(theme::Settings, bool)>,
    /// System font database (for the chrome font + the table-font picker).
    fonts_db: std::sync::Arc<fontdb::Database>,
    /// Installed font families + a monospaced flag (cached for the picker).
    font_families: Vec<(String, bool)>,
    /// Receives the background-measured family list (full monospace flags); `None` once applied.
    font_rx: Option<std::sync::mpsc::Receiver<Vec<(String, bool)>>>,
    /// Table font last pushed to egui — rebuild the font atlas only when this changes.
    applied_table_font: Option<String>,
    /// Filter text in the table-font picker (inside the Settings tab).
    font_search: String,
    /// Whether the picker is filtered to monospaced fonts.
    font_mono_only: bool,
    /// Whether the right-side panel (Columns / Parsing / Settings tabs) is expanded.
    pub(crate) panel_open: bool,
    /// Which tab the right-side panel shows.
    pub(crate) panel_tab: PanelTab,
}

/// Which tab the right-side panel shows. Columns and Parsing need a loaded file (Parsing only for
/// delimited text); Settings is always available.
#[derive(Clone, Copy, Default, PartialEq, Eq)]
pub(crate) enum PanelTab {
    #[default]
    Columns,
    Parsing,
    Settings,
}

impl PanelTab {
    fn label(self) -> &'static str {
        match self {
            PanelTab::Columns => "Columns",
            PanelTab::Parsing => "Parsing",
            PanelTab::Settings => "Settings",
        }
    }
}

impl TableizerApp {
    pub(crate) fn new(path: Option<PathBuf>) -> Self {
        let mut db = fontdb::Database::new();
        db.load_system_fonts();
        // Fast family list now (metadata only); refine monospace flags off-thread (parsing fonts to
        // measure advances is slow, and would otherwise stall startup).
        let font_families = fonts::installed_families(&db, false);
        let fonts_db = std::sync::Arc::new(db);
        let (font_tx, font_rx) = std::sync::mpsc::channel();
        {
            let fonts_db = std::sync::Arc::clone(&fonts_db);
            std::thread::spawn(move || {
                let _ = font_tx.send(fonts::installed_families(&fonts_db, true));
            });
        }
        let mut app = Self {
            view: View::Empty,
            recent: recent::load(),
            theme: prefs::load(),
            applied_theme: None,
            fonts_db,
            font_families,
            font_rx: Some(font_rx),
            applied_table_font: None,
            font_search: String::new(),
            font_mono_only: false,
            panel_open: false,
            panel_tab: PanelTab::default(),
        };
        if let Some(path) = path {
            app.open_path(path);
        }
        app
    }

    /// Rebuild and install the font atlas (chrome + table fonts) for the current settings.
    pub(crate) fn install_fonts(&mut self, ctx: &egui::Context) {
        let definitions = fonts::definitions(&self.fonts_db, self.theme.table_font.as_deref());
        ctx.set_fonts(definitions);
        self.applied_table_font = self.theme.table_font.clone();
    }

    fn open_path(&mut self, path: PathBuf) {
        let format = detect_format(&path);
        // The saved view (column layout/sort/filter) applies to every format; only the delimiter
        // override within it is delimited-specific.
        let saved = views::load(&path).unwrap_or_default();
        // Delimiter sniffing + override only make sense for delimited text; the other formats carry
        // their own schema, so they use a default dialect (header on → exports include column names).
        let (dialect, detected_delimiter, delimiter_auto) = match format {
            Format::Delimited => {
                let mut dialect = sniff_file(&path);
                let detected_delimiter = dialect.delimiter;
                // A persisted delimiter override must be applied *before* opening (it changes the
                // column structure); the rest of the saved view is applied after.
                let delimiter_auto = match saved.delimiter {
                    Some(byte) => {
                        dialect.delimiter = byte;
                        false
                    }
                    None => true,
                };
                (dialect, detected_delimiter, delimiter_auto)
            }
            Format::Json | Format::Parquet => (Dialect::default(), b',', true),
        };
        // UTF-16 is transcoded to UTF-8 by the engine; single-byte encodings default to UTF-8 here and
        // can be switched to Windows-1252 via the Parsing tab.
        let encoding: &'static Encoding = encoding_rs::UTF_8;
        self.view = match open_table(&path, format, dialect) {
            Ok(table) => {
                let mut layout = GridLayout::new(table.schema().columns.len());
                let mut view = ViewControls::default();
                saved.apply(&mut layout, &mut view);
                recent::add(&mut self.recent, &path);
                View::Loaded(Box::new(LoadedTable {
                    delimiter_input: delimiter_display(dialect.delimiter),
                    detected_delimiter,
                    delimiter_auto,
                    format,
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

    /// The panel tabs available right now. Columns + (delimited-only) Parsing need a loaded file;
    /// Settings is always present.
    fn available_tabs(&self) -> Vec<PanelTab> {
        let mut tabs = Vec::with_capacity(3);
        if let View::Loaded(loaded) = &self.view {
            tabs.push(PanelTab::Columns);
            if loaded.format == Format::Delimited {
                tabs.push(PanelTab::Parsing);
            }
        }
        tabs.push(PanelTab::Settings);
        tabs
    }

    /// Keep `panel_tab` on a currently-available tab (the file may have closed or changed format).
    fn fix_panel_tab(&mut self) {
        if !self.available_tabs().contains(&self.panel_tab) {
            self.panel_tab = if matches!(self.view, View::Loaded(_)) {
                PanelTab::Columns
            } else {
                PanelTab::Settings
            };
        }
    }

    /// Render the right panel's tab strip (with a close button) and the active tab's contents.
    fn side_panel_contents(&mut self, ui: &mut egui::Ui) {
        let tabs = self.available_tabs();
        ui.add_space(4.0);
        ui.horizontal(|ui| {
            for tab in &tabs {
                if ui
                    .selectable_label(self.panel_tab == *tab, tab.label())
                    .clicked()
                {
                    self.panel_tab = *tab;
                }
            }
            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                if close_button(ui).clicked() {
                    self.panel_open = false;
                }
            });
        });
        ui.separator();
        match self.panel_tab {
            PanelTab::Columns => {
                if let View::Loaded(loaded) = &mut self.view {
                    columns_tab(ui, loaded);
                }
            }
            PanelTab::Parsing => {
                if let View::Loaded(loaded) = &mut self.view {
                    parsing_tab(ui, loaded);
                }
            }
            PanelTab::Settings => settings_tab(
                ui,
                &mut self.theme,
                &self.font_families,
                &mut self.font_search,
                &mut self.font_mono_only,
            ),
        }
    }
}

impl eframe::App for TableizerApp {
    fn ui(&mut self, ui: &mut egui::Ui, _frame: &mut eframe::Frame) {
        let ctx = ui.ctx().clone();

        // Pick up the background-measured font-family list (full monospace flags) when it's ready.
        let measured = self.font_rx.as_ref().and_then(|rx| rx.try_recv().ok());
        if let Some(families) = measured {
            self.font_families = families;
            self.font_rx = None;
        }

        // Resolve the theme (following the OS for `Auto`) and restyle only when it changes.
        let system_dark = ctx.system_theme().is_none_or(|t| t == egui::Theme::Dark);
        let (style, palette) = theme::build(&self.theme, system_dark);
        if self.applied_theme.as_ref() != Some(&(self.theme.clone(), system_dark)) {
            ctx.set_global_style(style.clone());
            self.applied_theme = Some((self.theme.clone(), system_dark));
            // `set_global_style` only reaches uis created afterwards (and ctx-level popups), but this
            // frame's root `ui` already exists with the old style. Apply directly so the named text
            // styles resolve this frame too — otherwise the first frame panics on lookup.
            ui.set_style(style);
            // Match the OS window chrome (title bar) to the resolved theme, so it flips along with
            // the app colors instead of staying on the OS default.
            ctx.send_viewport_cmd(egui::ViewportCommand::SetTheme(
                if theme::is_dark(&self.theme, system_dark) {
                    egui::SystemTheme::Dark
                } else {
                    egui::SystemTheme::Light
                },
            ));
        }
        // Rebuild the font atlas only when the chosen table font changes.
        if self.applied_table_font != self.theme.table_font {
            self.install_fonts(&ctx);
        }

        let mut to_open: Option<PathBuf> = None;
        let theme_before = self.theme.clone();
        let dialect_before = match &self.view {
            View::Loaded(loaded) => Some(loaded.dialect),
            _ => None,
        };
        // ⌘/Ctrl+F focuses the Find field.
        let focus_find = ctx.input_mut(|i| i.consume_key(egui::Modifiers::COMMAND, egui::Key::F));
        // ⌘Q / Ctrl+Q quits the app.
        if ctx.input_mut(|i| i.consume_shortcut(&QUIT_SHORTCUT)) {
            ctx.send_viewport_cmd(egui::ViewportCommand::Close);
        }
        // ⌘O / Ctrl+O opens a file (same as File ▸ Open…).
        if ctx.input_mut(|i| i.consume_shortcut(&OPEN_SHORTCUT))
            && let Some(path) = rfd::FileDialog::new().pick_file()
        {
            to_open = Some(path);
        }
        // ⌘W / Ctrl+W closes the current file (no-op when none is open).
        if ctx.input_mut(|i| i.consume_shortcut(&CLOSE_SHORTCUT))
            && matches!(self.view, View::Loaded(_))
        {
            self.view = View::Empty;
        }
        // ⌘, / Ctrl+, opens the right panel on the Settings tab (toggles it closed if already there).
        if ctx.input_mut(|i| i.consume_shortcut(&SETTINGS_SHORTCUT)) {
            if self.panel_open && self.panel_tab == PanelTab::Settings {
                self.panel_open = false;
            } else {
                self.panel_open = true;
                self.panel_tab = PanelTab::Settings;
            }
        }
        // Esc closes the panel when nothing else holds keyboard focus (e.g. not mid-edit in a field).
        if self.panel_open
            && ctx.input(|i| i.key_pressed(egui::Key::Escape))
            && ctx.memory(|m| m.focused().is_none())
        {
            self.panel_open = false;
        }

        egui::Panel::top("menu_bar").show_inside(ui, |ui| {
            // `wide_menu` gives the bar buttons *and* every dropdown popup roomier horizontal item
            // padding than egui's default `menu_style` (which hugs the text at 2px). `.config(..)`
            // carries it into the submenus, which inherit the bar's menu config.
            egui::MenuBar::new()
                .style(crate::ui::wide_menu)
                .config(egui::containers::menu::MenuConfig::new().style(crate::ui::wide_menu))
                .ui(ui, |ui| menu_bar(ui, self, &mut to_open));
        });

        if matches!(self.view, View::Loaded(_)) {
            egui::Panel::top("toolbar").show_inside(ui, |ui| {
                if let View::Loaded(loaded) = &mut self.view {
                    toolbar(ui, loaded, focus_find);
                }
            });
        }

        if matches!(self.view, View::Loaded(_)) {
            egui::Panel::bottom("status_bar").show_inside(ui, |ui| {
                if let View::Loaded(loaded) = &self.view {
                    status_bar(ui, loaded, &palette);
                }
            });
        }

        // Right-side tabbed panel (Columns / Parsing / Settings): resizable, slides in/out when
        // toggled. Shown before the central panel so the grid takes the remaining width; the default
        // width suits the Settings tab (its font picker is the widest content).
        self.fix_panel_tab();
        let panel_open = self.panel_open;
        egui::Panel::right("side_panel")
            .resizable(true)
            .default_size(280.0)
            .min_size(280.0)
            .max_size(500.0)
            .show_animated_inside(ui, panel_open, |ui| self.side_panel_contents(ui));

        // React to edits from the toolbar (filter/sort) and the side panel (Parsing → dialect,
        // Columns → layout): a dialect change re-opens the file; otherwise apply the view and persist
        // the per-file saved view. This must run *after* the panel renders — the Parsing tab lives
        // there, so an earlier check would miss the change (and next frame's snapshot would hide it).
        if let View::Loaded(loaded) = &mut self.view {
            if Some(loaded.dialect) != dialect_before {
                if let Ok(reopened) = open_table(&loaded.path, loaded.format, loaded.dialect) {
                    loaded.table = reopened;
                    loaded.layout = GridLayout::new(loaded.table.schema().columns.len());
                }
            } else {
                let desired = loaded.view.desired();
                if desired != loaded.view.applied {
                    loaded.view.applied = desired.clone();
                    loaded.view.error = match loaded.table.set_view(&desired) {
                        Ok(()) => None,
                        Err(error) => Some(error.to_string()),
                    };
                }
                let delimiter = (!loaded.delimiter_auto).then_some(loaded.dialect.delimiter);
                let current = SavedView::snapshot(&loaded.layout, &loaded.view, delimiter);
                if current != loaded.saved {
                    views::save(&loaded.path, &current);
                    loaded.saved = current;
                }
            }
        }

        // No inner margin: the table fills the central area edge-to-edge (the empty/failed views
        // center their own content, so they're unaffected).
        let central_frame = egui::Frame::central_panel(ui.style()).inner_margin(egui::Margin::ZERO);
        egui::CentralPanel::default()
            .frame(central_frame)
            .show_inside(ui, |ui| match &mut self.view {
                View::Empty => empty_view(ui, &self.recent, &mut to_open),
                View::Failed { path, error } => {
                    ui.add_space(40.0);
                    ui.vertical_centered(|ui| {
                        ui.heading("Could not open file");
                        ui.label(format!("{}: {error}", path.display()));
                    });
                }
                View::Loaded(loaded) => grid(ui, loaded, &palette),
            });

        if self.theme != theme_before {
            prefs::save(&self.theme);
        }
        // Files handed to us by macOS "Open With" / double-click arrive via an Apple Event, not argv
        // (see macos_open.rs); open whatever has been queued since the last frame.
        #[cfg(target_os = "macos")]
        for path in crate::macos_open::take_pending() {
            self.open_path(path);
        }
        if let Some(path) = to_open {
            self.open_path(path);
            // Drop egui_table's stored column widths so the new file's columns auto-fit to their
            // content on open (egui_table re-fits when it has no state for the table). Column
            // order/visibility live in our own `GridLayout`, so they're unaffected.
            ctx.data_mut(|d| d.remove_by_type::<egui_table::TableState>());
        }
    }

    fn clear_color(&self, visuals: &egui::Visuals) -> [f32; 4] {
        // Window edges match the panel background (set via the theme `Style`).
        visuals.panel_fill.to_normalized_gamma_f32()
    }
}

/// A small ✕ close button drawn as two strokes (shapes, not a glyph — font-independent, per the `ui`
/// module's hand-painted-text invariant). Returns its click response.
fn close_button(ui: &mut egui::Ui) -> egui::Response {
    let (rect, response) = ui.allocate_exact_size(egui::vec2(20.0, 20.0), egui::Sense::click());
    if response.hovered() {
        ui.ctx().set_cursor_icon(egui::CursorIcon::PointingHand);
    }
    let color = if response.hovered() {
        ui.visuals().text_color()
    } else {
        ui.visuals().weak_text_color()
    };
    let c = rect.center();
    let r = 4.0;
    let stroke = egui::Stroke::new(1.5, color);
    ui.painter()
        .line_segment([c + egui::vec2(-r, -r), c + egui::vec2(r, r)], stroke);
    ui.painter()
        .line_segment([c + egui::vec2(-r, r), c + egui::vec2(r, -r)], stroke);
    response.on_hover_text("Close panel")
}

/// Quit — the standard per-OS shortcut (⌘Q on macOS, Ctrl+Q elsewhere).
pub(crate) const QUIT_SHORTCUT: egui::KeyboardShortcut =
    egui::KeyboardShortcut::new(egui::Modifiers::COMMAND, egui::Key::Q);
/// Open the right panel on the Settings tab (⌘, / Ctrl+,).
pub(crate) const SETTINGS_SHORTCUT: egui::KeyboardShortcut =
    egui::KeyboardShortcut::new(egui::Modifiers::COMMAND, egui::Key::Comma);
/// Open a file (⌘O / Ctrl+O).
pub(crate) const OPEN_SHORTCUT: egui::KeyboardShortcut =
    egui::KeyboardShortcut::new(egui::Modifiers::COMMAND, egui::Key::O);
/// Close the current file (⌘W / Ctrl+W).
pub(crate) const CLOSE_SHORTCUT: egui::KeyboardShortcut =
    egui::KeyboardShortcut::new(egui::Modifiers::COMMAND, egui::Key::W);
