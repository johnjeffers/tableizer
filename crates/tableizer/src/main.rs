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

/// The app's whole visual system in one place: a light/dark theme that can follow the OS, a
/// user-selectable accent color and layout density. [`build`] turns [`Settings`] into an egui
/// [`Style`] (spacing, rounding, widget colors) plus a [`Palette`] of the extra colors the custom
/// grid/toolbar painting needs. Retheme by editing this module.
mod theme {
    use eframe::egui::{
        Color32, CornerRadius, FontFamily, FontId, Margin, Stroke, Style, TextStyle, Theme, Vec2,
        style::Spacing,
    };
    use serde::{Deserialize, Serialize};
    use std::collections::BTreeMap;

    /// Light/dark selection.
    #[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
    pub enum Mode {
        /// Follow the OS light/dark setting.
        #[default]
        Auto,
        Light,
        Dark,
    }

    impl Mode {
        pub const ALL: [Mode; 3] = [Mode::Auto, Mode::Light, Mode::Dark];
        pub fn label(self) -> &'static str {
            match self {
                Mode::Auto => "Auto (OS)",
                Mode::Light => "Light",
                Mode::Dark => "Dark",
            }
        }
    }

    /// Accent color for selection, active controls, links and focus.
    #[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
    pub enum Accent {
        #[default]
        Blue,
        Teal,
        Violet,
        Amber,
    }

    impl Accent {
        pub const ALL: [Accent; 4] = [Accent::Blue, Accent::Teal, Accent::Violet, Accent::Amber];
        pub fn label(self) -> &'static str {
            match self {
                Accent::Blue => "Blue",
                Accent::Teal => "Teal",
                Accent::Violet => "Violet",
                Accent::Amber => "Amber",
            }
        }
        pub fn color(self) -> Color32 {
            match self {
                Accent::Blue => Color32::from_rgb(56, 124, 246),
                Accent::Teal => Color32::from_rgb(22, 160, 145),
                Accent::Violet => Color32::from_rgb(138, 104, 244),
                Accent::Amber => Color32::from_rgb(228, 150, 42),
            }
        }
    }

    /// Layout density.
    #[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
    pub enum Density {
        #[default]
        Comfortable,
        Compact,
    }

    impl Density {
        pub const ALL: [Density; 2] = [Density::Comfortable, Density::Compact];
        pub fn label(self) -> &'static str {
            match self {
                Density::Comfortable => "Comfortable",
                Density::Compact => "Compact",
            }
        }
    }

    /// Persisted, user-editable theme settings.
    #[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
    pub struct Settings {
        #[serde(default)]
        pub mode: Mode,
        #[serde(default)]
        pub accent: Accent,
        #[serde(default)]
        pub density: Density,
        /// Data-cell font family (a system font name); `None` = use the app font.
        #[serde(default)]
        pub table_font: Option<String>,
        /// Data-cell font size, in points.
        #[serde(default = "default_table_font_size")]
        pub table_font_size: f32,
    }

    fn default_table_font_size() -> f32 {
        13.5
    }

    impl Default for Settings {
        fn default() -> Self {
            Self {
                mode: Mode::default(),
                accent: Accent::default(),
                density: Density::default(),
                table_font: None,
                table_font_size: default_table_font_size(),
            }
        }
    }

    /// Extra colors + metrics the custom grid/toolbar painting needs (beyond the egui [`Style`]).
    #[derive(Clone)]
    pub struct Palette {
        pub accent: Color32,
        pub header_bg: Color32,
        pub header_text: Color32,
        pub row_selected: Color32,
        pub row_hover: Color32,
        pub search_match: Color32,
        pub stripe: Color32,
        pub warning: Color32,
        pub error: Color32,
        pub border: Color32,
        pub row_height: f32,
        pub header_height: f32,
        /// Font used for data cells (the user-chosen family + size).
        pub table_font: FontId,
    }

    /// Whether `settings` resolves to dark, given the OS preference.
    pub fn is_dark(settings: &Settings, system_dark: bool) -> bool {
        match settings.mode {
            Mode::Auto => system_dark,
            Mode::Light => false,
            Mode::Dark => true,
        }
    }

    fn with_alpha(color: Color32, alpha: u8) -> Color32 {
        Color32::from_rgba_unmultiplied(color.r(), color.g(), color.b(), alpha)
    }

    /// Build the egui [`Style`] + [`Palette`] for `settings` under the given OS theme.
    pub fn build(settings: &Settings, system_dark: bool) -> (Style, Palette) {
        let dark = is_dark(settings, system_dark);
        let accent = settings.accent.color();
        let comfortable = settings.density == Density::Comfortable;

        let mut visuals = if dark { Theme::Dark } else { Theme::Light }.default_visuals();
        // Accent wherever egui draws selection / focus / links.
        visuals.selection.bg_fill = with_alpha(accent, if dark { 110 } else { 90 });
        visuals.selection.stroke = Stroke::new(1.0, accent);
        visuals.hyperlink_color = accent;
        visuals.widgets.hovered.bg_stroke = Stroke::new(1.0, with_alpha(accent, 160));
        // Consistent, subtle rounding everywhere.
        let radius = CornerRadius::same(5);
        visuals.widgets.noninteractive.corner_radius = radius;
        visuals.widgets.inactive.corner_radius = radius;
        visuals.widgets.hovered.corner_radius = radius;
        visuals.widgets.active.corner_radius = radius;
        visuals.widgets.open.corner_radius = radius;
        visuals.window_corner_radius = CornerRadius::same(8);
        visuals.menu_corner_radius = CornerRadius::same(6);

        // Keep all metrics on the integer pixel grid — fractional spacing renders blurry text and
        // trips egui's "unaligned" debug overlay.
        let scale = if comfortable { 1.0 } else { 0.65 };
        let round = |v: f32| (v * scale).round();
        let spacing = Spacing {
            item_spacing: Vec2::new(round(8.0), round(6.0)),
            button_padding: Vec2::new(round(8.0), round(4.0)),
            menu_margin: Margin::same(round(6.0) as i8),
            window_margin: Margin::same(round(8.0) as i8),
            interact_size: Vec2::new(
                Spacing::default().interact_size.x,
                if comfortable { 24.0 } else { 20.0 },
            ),
            ..Spacing::default()
        };

        let text_styles = BTreeMap::from([
            (
                TextStyle::Small,
                FontId::new(11.0, FontFamily::Proportional),
            ),
            (TextStyle::Body, FontId::new(13.5, FontFamily::Proportional)),
            (
                TextStyle::Button,
                FontId::new(13.5, FontFamily::Proportional),
            ),
            (
                TextStyle::Heading,
                FontId::new(19.0, FontFamily::Proportional),
            ),
            (
                TextStyle::Monospace,
                FontId::new(12.5, FontFamily::Monospace),
            ),
        ]);
        let style = Style {
            visuals,
            spacing,
            text_styles,
            ..Style::default()
        };

        // Row height tracks the data-cell font size + density padding, so large fonts don't clip.
        let row_height = (settings.table_font_size + if comfortable { 12.0 } else { 7.0 }).round();

        let palette = Palette {
            accent,
            header_bg: if dark {
                Color32::from_gray(38)
            } else {
                Color32::from_gray(236)
            },
            header_text: if dark {
                Color32::from_gray(165)
            } else {
                Color32::from_gray(95)
            },
            row_selected: with_alpha(accent, if dark { 70 } else { 55 }),
            row_hover: with_alpha(accent, if dark { 28 } else { 20 }),
            search_match: Color32::from_rgba_unmultiplied(250, 205, 70, if dark { 50 } else { 95 }),
            stripe: if dark {
                Color32::from_white_alpha(8)
            } else {
                Color32::from_black_alpha(10)
            },
            warning: Color32::from_rgb(235, 165, 50),
            error: if dark {
                Color32::from_rgb(240, 110, 110)
            } else {
                Color32::from_rgb(200, 50, 50)
            },
            border: if dark {
                Color32::from_white_alpha(24)
            } else {
                Color32::from_black_alpha(28)
            },
            row_height,
            // Keep the header at least as tall as a row (so big fonts don't make rows overshoot it).
            header_height: row_height.max(if comfortable { 30.0 } else { 24.0 }),
            table_font: FontId::new(
                settings.table_font_size,
                FontFamily::Name(crate::fonts::TABLE_FONT.into()),
            ),
        };
        (style, palette)
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
            let mut app = TableizerApp::new(path);
            app.install_fonts(&cc.egui_ctx); // chrome + table fonts; re-installed on change in `ui`
            // The theme (`theme` module) is resolved and applied each frame in `App::ui`.
            Ok(Box::new(app))
        }),
    )
}

/// Font management: an OS-native chrome font (with bundled Inter as a cross-platform fallback) and a
/// user-selectable data-cell font, both resolved from the system font database via `fontdb`.
mod fonts {
    use eframe::egui::{FontData, FontDefinitions, FontFamily};
    use std::sync::Arc;

    /// egui custom-family name used for data cells.
    pub const TABLE_FONT: &str = "table";

    /// Whether ab_glyph (egui's font backend) can parse these bytes — guards the atlas build against
    /// unsupported fonts crashing the app.
    fn parseable(data: &[u8]) -> bool {
        ab_glyph::FontRef::try_from_slice(data).is_ok()
    }

    /// Candidate OS-native UI font families, best first.
    fn os_ui_families() -> &'static [&'static str] {
        #[cfg(target_os = "macos")]
        return &[
            "SF Pro Text",
            "SF Pro",
            ".AppleSystemUIFont",
            "Helvetica Neue",
        ];
        #[cfg(target_os = "windows")]
        return &["Segoe UI Variable Text", "Segoe UI"];
        #[cfg(not(any(target_os = "macos", target_os = "windows")))]
        return &[
            "Cantarell",
            "Ubuntu",
            "Noto Sans",
            "DejaVu Sans",
            "Liberation Sans",
        ];
    }

    /// Bytes of the first of `names` that resolves to a parseable font in `db`.
    fn load_family(db: &fontdb::Database, names: &[&str]) -> Option<Vec<u8>> {
        for &name in names {
            let Some(id) = db.query(&fontdb::Query {
                families: &[fontdb::Family::Name(name)],
                ..Default::default()
            }) else {
                continue;
            };
            if let Some(bytes) = db.with_face_data(id, |data, _| data.to_vec())
                && parseable(&bytes)
            {
                return Some(bytes);
            }
        }
        None
    }

    /// Whether a face is actually monospaced, measured by comparing the advance widths of a narrow
    /// and a wide glyph. `fontdb`'s `monospaced` flag only reads `post.isFixedPitch`, which some
    /// genuinely-monospaced fonts (e.g. Monaco) leave unset — so we measure to be sure.
    fn measured_monospaced(db: &fontdb::Database, id: fontdb::ID) -> bool {
        use ab_glyph::Font;
        db.with_face_data(id, |data, index| {
            let Ok(font) = ab_glyph::FontRef::try_from_slice_and_index(data, index) else {
                return false;
            };
            let (i, m) = (font.glyph_id('i'), font.glyph_id('M'));
            if i.0 == 0 || m.0 == 0 {
                return false; // not a Latin text font
            }
            let narrow = font.h_advance_unscaled(i);
            let wide = font.h_advance_unscaled(m);
            narrow > 0.0 && (narrow - wide).abs() < 1.0
        })
        .unwrap_or(false)
    }

    /// Sorted installed font families, each paired with a monospaced flag — for the picker's
    /// "Monospace" filter. When `measure` is false the flag is just `fontdb`'s declaration (fast,
    /// metadata only); when true it falls back to a measured advance-width check (catches fonts like
    /// Monaco that don't declare it, but parses each family's font, so it runs off the UI thread).
    pub fn installed_families(db: &fontdb::Database, measure: bool) -> Vec<(String, bool)> {
        use std::collections::BTreeMap;
        let mut families: BTreeMap<String, bool> = BTreeMap::new();
        for face in db.faces() {
            let Some((name, _)) = face.families.first() else {
                continue;
            };
            // Skip private system fonts: macOS hides families whose name starts with '.'.
            if name.starts_with('.') || families.contains_key(name) {
                continue;
            }
            let mono = face.monospaced || (measure && measured_monospaced(db, face.id));
            families.insert(name.clone(), mono);
        }
        families.into_iter().collect()
    }

    /// Build egui font definitions: OS-native proportional chrome font (+ bundled Inter + egui's
    /// defaults as fallbacks), and the chosen `table_family` for data cells (falling back to the
    /// proportional stack when unset or unloadable).
    pub fn definitions(db: &fontdb::Database, table_family: Option<&str>) -> FontDefinitions {
        let mut fonts = FontDefinitions::default();

        let mut proportional = Vec::new();
        if let Some(bytes) = load_family(db, os_ui_families()) {
            fonts
                .font_data
                .insert("os-ui".to_owned(), Arc::new(FontData::from_owned(bytes)));
            proportional.push("os-ui".to_owned());
        }
        fonts.font_data.insert(
            "Inter".to_owned(),
            Arc::new(FontData::from_static(include_bytes!(
                "../assets/InterVariable.ttf"
            ))),
        );
        proportional.push("Inter".to_owned());
        if let Some(defaults) = fonts.families.get(&FontFamily::Proportional) {
            proportional.extend(defaults.iter().cloned());
        }
        fonts
            .families
            .insert(FontFamily::Proportional, proportional.clone());

        let mut table = Vec::new();
        if let Some(name) = table_family
            && let Some(bytes) = load_family(db, &[name])
        {
            fonts
                .font_data
                .insert("table".to_owned(), Arc::new(FontData::from_owned(bytes)));
            table.push("table".to_owned());
        }
        table.extend(proportional);
        fonts
            .families
            .insert(FontFamily::Name(TABLE_FONT.into()), table);

        fonts
    }
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

/// Render a delimiter byte for the custom field: a printable ASCII char as itself, anything else
/// (tab, control chars) as a `0x..` hex byte the user can read and re-enter.
fn delimiter_display(delimiter: u8) -> String {
    if delimiter.is_ascii_graphic() || delimiter == b' ' {
        (delimiter as char).to_string()
    } else {
        format!("0x{delimiter:02x}")
    }
}

/// A friendly name for a delimiter byte (for the "Auto · detected …" label).
fn delimiter_label(delimiter: u8) -> String {
    match delimiter {
        b',' => "comma".to_string(),
        b'\t' => "tab".to_string(),
        b';' => "semicolon".to_string(),
        b'|' => "pipe".to_string(),
        _ => delimiter_display(delimiter),
    }
}

/// Parse the custom-delimiter field: a single ASCII character, or a hex byte as `0xNN` / `\xNN`.
/// Returns `None` for anything that isn't a single byte (so the field can be edited mid-entry).
fn parse_delimiter(input: &str) -> Option<u8> {
    if let Some(hex) = input
        .strip_prefix("0x")
        .or_else(|| input.strip_prefix("\\x"))
        && hex.len() == 2
    {
        return u8::from_str_radix(hex, 16).ok();
    }
    let bytes = input.as_bytes();
    if bytes.len() == 1 && bytes[0].is_ascii() {
        return Some(bytes[0]);
    }
    None
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

/// Move `dragged` next to `target` in display order — before it, or after it when `after` is set.
/// Pure so the reorder logic is verified independently of the drag-and-drop UI.
fn reorder(order: &mut Vec<ColumnId>, dragged: ColumnId, target: ColumnId, after: bool) {
    if dragged == target {
        return;
    }
    let Some(from) = order.iter().position(|&c| c == dragged) else {
        return;
    };
    let col = order.remove(from);
    let insert_at = order
        .iter()
        .position(|&c| c == target)
        .map_or(order.len(), |i| i + usize::from(after))
        .min(order.len());
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
    /// Selected display row (click or arrow/page/home/end keys); ⌘/Ctrl+C copies it.
    selected_row: Option<u64>,
    /// Display row under the mouse (transient; drives the hover highlight).
    hovered_row: Option<u64>,
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
    /// Explicit delimiter override; `None` = auto-detect (the default).
    #[serde(default)]
    delimiter: Option<u8>,
}

impl SavedView {
    /// Snapshot the current layout + controls. `delimiter` is the explicit override (or `None` = auto).
    fn snapshot(layout: &GridLayout, view: &ViewControls, delimiter: Option<u8>) -> Self {
        Self {
            order: layout.order.iter().map(|c| c.0).collect(),
            visible: layout.visible.clone(),
            frozen: layout.frozen,
            sort: view
                .sort
                .map(|s| (s.column.0, s.direction == Direction::Ascending)),
            filter: (view.filter_mode && !view.search.is_empty())
                .then(|| (view.search.clone(), view.regex, view.invert)),
            delimiter,
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
    /// Text in the Parsing menu's custom-delimiter field (a char like `:` or a hex byte like `0x01`).
    delimiter_input: String,
    /// The delimiter `Dialect::sniff` detected on open — what "Auto" resolves to.
    detected_delimiter: u8,
    /// Whether the delimiter is auto-detected (the default) vs an explicit user override.
    delimiter_auto: bool,
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

/// Theme-settings persistence in the OS config dir.
mod prefs {
    use crate::theme::Settings;
    use std::path::PathBuf;

    fn file() -> Option<PathBuf> {
        let base = directories::BaseDirs::new()?;
        Some(base.config_dir().join("tableizer").join("theme.json"))
    }

    pub fn load() -> Settings {
        file()
            .and_then(|f| std::fs::read(f).ok())
            .and_then(|data| serde_json::from_slice(&data).ok())
            .unwrap_or_default()
    }

    pub fn save(settings: &Settings) {
        let Some(path) = file() else {
            return;
        };
        if let Some(dir) = path.parent() {
            let _ = std::fs::create_dir_all(dir);
        }
        if let Ok(data) = serde_json::to_vec_pretty(settings) {
            let _ = std::fs::write(path, data);
        }
    }
}

struct TableizerApp {
    view: View,
    recent: Vec<PathBuf>,
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
    /// Filter text in the table-font picker (inside the Settings window).
    font_search: String,
    /// Whether the picker is filtered to monospaced fonts.
    font_mono_only: bool,
    /// Whether the Settings window is open.
    settings_open: bool,
}

impl TableizerApp {
    fn new(path: Option<PathBuf>) -> Self {
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
            settings_open: false,
        };
        if let Some(path) = path {
            app.open_path(path);
        }
        app
    }

    /// Rebuild and install the font atlas (chrome + table fonts) for the current settings.
    fn install_fonts(&mut self, ctx: &egui::Context) {
        let definitions = fonts::definitions(&self.fonts_db, self.theme.table_font.as_deref());
        ctx.set_fonts(definitions);
        self.applied_table_font = self.theme.table_font.clone();
    }

    fn open_path(&mut self, path: PathBuf) {
        let mut dialect = sniff_file(&path);
        let detected_delimiter = dialect.delimiter;
        // A persisted delimiter override must be applied *before* opening (it changes the column
        // structure); the rest of the saved view (layout/sort/filter) is applied after.
        let saved = views::load(&path).unwrap_or_default();
        let delimiter_auto = match saved.delimiter {
            Some(byte) => {
                dialect.delimiter = byte;
                false
            }
            None => true,
        };
        // UTF-16 is transcoded to UTF-8 by the engine; single-byte encodings default to UTF-8 here and
        // can be switched to Windows-1252 via the Parsing menu.
        let encoding: &'static Encoding = encoding_rs::UTF_8;
        self.view = match open_table(&path, dialect) {
            Ok(table) => {
                let mut layout = GridLayout::new(table.schema().columns.len());
                let mut view = ViewControls::default();
                saved.apply(&mut layout, &mut view);
                recent::add(&mut self.recent, &path);
                View::Loaded(Box::new(LoadedTable {
                    delimiter_input: delimiter_display(dialect.delimiter),
                    detected_delimiter,
                    delimiter_auto,
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
            ctx.set_global_style(style);
            self.applied_theme = Some((self.theme.clone(), system_dark));
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

        egui::Panel::top("menu_bar").show_inside(ui, |ui| {
            egui::MenuBar::new().ui(ui, |ui| menu_bar(ui, self, &mut to_open));
        });

        if matches!(self.view, View::Loaded(_)) {
            egui::Panel::top("toolbar").show_inside(ui, |ui| {
                if let View::Loaded(loaded) = &mut self.view {
                    toolbar(ui, loaded, focus_find);
                }
            });
        }

        // React to the toolbar's edits: a dialect change re-opens the file; otherwise apply the
        // filter/sort view and persist the per-file saved view.
        if let View::Loaded(loaded) = &mut self.view {
            if Some(loaded.dialect) != dialect_before {
                if let Ok(reopened) = open_table(&loaded.path, loaded.dialect) {
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

        if matches!(self.view, View::Loaded(_)) {
            egui::Panel::bottom("status_bar").show_inside(ui, |ui| {
                if let View::Loaded(loaded) = &self.view {
                    status_bar(ui, loaded, &palette);
                }
            });
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

        // Settings window: toggled by the menu-bar item and ⌘/Ctrl+, ; closed by its ✕ or Esc.
        if ctx.input_mut(|i| i.consume_key(egui::Modifiers::COMMAND, egui::Key::Comma)) {
            self.settings_open = !self.settings_open;
        }
        if self.settings_open
            && ctx.input(|i| i.key_pressed(egui::Key::Escape))
            && ctx.memory(|m| m.focused().is_none())
        {
            self.settings_open = false;
        }
        if self.settings_open {
            settings_window(
                &ctx,
                &mut self.settings_open,
                &mut self.theme,
                &self.font_families,
                &mut self.font_search,
                &mut self.font_mono_only,
            );
        }

        if self.theme != theme_before {
            prefs::save(&self.theme);
        }
        if let Some(path) = to_open {
            self.open_path(path);
        }
    }

    fn clear_color(&self, visuals: &egui::Visuals) -> [f32; 4] {
        // Window edges match the panel background (set via the theme `Style`).
        visuals.panel_fill.to_normalized_gamma_f32()
    }
}

/// The menu bar (File / View / Export / Cache / Settings).
fn menu_bar(ui: &mut egui::Ui, app: &mut TableizerApp, to_open: &mut Option<PathBuf>) {
    ui.menu_button("File", |ui| {
        ui.set_min_width(150.0);
        if ui.button("Open…").clicked() {
            if let Some(path) = rfd::FileDialog::new().pick_file() {
                *to_open = Some(path);
            }
            ui.close();
        }
        ui.menu_button("Recent", |ui| {
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
        });
        ui.separator();
        let loaded = matches!(app.view, View::Loaded(_));
        if ui.add_enabled(loaded, egui::Button::new("Close")).clicked() {
            app.view = View::Empty;
            ui.close();
        }
    });

    if let View::Loaded(loaded) = &mut app.view {
        ui.menu_button("Parsing", |ui| parsing_menu(ui, loaded));
        ui.menu_button("View", |ui| {
            ui.set_min_width(190.0);
            menu_section(ui, "COLUMNS");
            for (i, shown) in loaded.layout.visible.iter_mut().enumerate() {
                ui.checkbox(
                    shown,
                    column_name(loaded.table.schema(), ColumnId(i as u32), loaded.encoding),
                );
            }
            menu_section(ui, "FREEZE");
            ui.horizontal(|ui| {
                ui.label("Leftmost columns:");
                ui.add(
                    egui::DragValue::new(&mut loaded.layout.frozen)
                        .range(0..=loaded.table.schema().columns.len()),
                );
            });
        });
        ui.menu_button("Export", |ui| export_menu(ui, loaded));
    }

    ui.menu_button("Cache", |ui| {
        ui.set_min_width(180.0);
        menu_section(ui, "INDEX CACHE");
        ui.label(format!(
            "Size on disk: {}",
            human_bytes(tableizer_core::cache::total_size())
        ));
        ui.add_space(2.0);
        if ui.button("Clear cache").clicked() {
            tableizer_core::cache::clear();
            ui.close();
        }
    });

    // Settings opens a window (not a dropdown), so it's a plain menu-bar button.
    if ui.add(egui::Button::new("Settings").frame(false)).clicked() {
        app.settings_open = !app.settings_open;
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
    ui.label(egui::RichText::new(title).size(10.5).strong().color(color));
    ui.add_space(1.0);
}

/// A full-width menu choice row with hover + selected states (selected = accent fill + strong text),
/// optionally led by a colored dot. Returns true when clicked.
fn menu_choice(
    ui: &mut egui::Ui,
    width: f32,
    selected: bool,
    dot: Option<egui::Color32>,
    text: &str,
) -> bool {
    let sel_bg = ui.visuals().selection.bg_fill;
    let hover_bg = ui.visuals().widgets.hovered.weak_bg_fill;
    let text_color = if selected {
        ui.visuals().strong_text_color()
    } else {
        ui.visuals().text_color()
    };
    // In a menu popup, callers must pass a fixed width — `available_width()` there is the whole
    // window and would balloon the menu; in a window, `available_width()` is correct.
    let (rect, response) = ui.allocate_exact_size(egui::vec2(width, 24.0), egui::Sense::click());
    if selected {
        ui.painter()
            .rect_filled(rect, egui::CornerRadius::same(5), sel_bg);
    } else if response.hovered() {
        ui.painter()
            .rect_filled(rect, egui::CornerRadius::same(5), hover_bg);
    }
    let text_x = if let Some(color) = dot {
        ui.painter()
            .circle_filled(rect.left_center() + egui::vec2(13.0, 0.0), 5.0, color);
        26.0
    } else {
        10.0
    };
    ui.painter().text(
        rect.left_center() + egui::vec2(text_x, 0.0),
        egui::Align2::LEFT_CENTER,
        text,
        egui::FontId::proportional(13.5),
        text_color,
    );
    response.clicked()
}

/// A section heading inside the Settings window.
fn settings_section(ui: &mut egui::Ui, title: &str) {
    ui.add_space(2.0);
    ui.label(egui::RichText::new(title).strong().size(14.0));
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
fn settings_window(
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
                if ui.button(egui::RichText::new("−").size(15.0)).clicked() {
                    settings.table_font_size = (settings.table_font_size - 0.5).max(8.0);
                }
                ui.add_sized(
                    egui::vec2(48.0, h),
                    egui::Label::new(format!("{:.1} pt", settings.table_font_size)),
                );
                if ui.button(egui::RichText::new("+").size(15.0)).clicked() {
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
        });
    *open = window_open;
}

/// The toolbar: the find/filter controls. `focus_find` requests focus on the Find field (⌘/Ctrl+F).
fn toolbar(ui: &mut egui::Ui, loaded: &mut LoadedTable, focus_find: bool) {
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
        ui.checkbox(&mut view.filter_mode, "Only show matches");
        ui.checkbox(&mut view.regex, "Regex");
        ui.checkbox(&mut view.invert, "Invert");
    });
}

/// The bottom status bar: path, row count, indexing/view-build progress, data-quality, errors.
fn status_bar(ui: &mut egui::Ui, loaded: &LoadedTable, palette: &theme::Palette) {
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
        if let Some(row) = loaded.view.selected_row {
            ui.separator();
            let weak = ui.visuals().weak_text_color();
            ui.label(
                egui::RichText::new(format!("row {} selected", fmt_count(row + 1))).color(weak),
            );
        }
    });
}

/// The empty (no file) view.
fn empty_view(ui: &mut egui::Ui, recent: &[PathBuf], to_open: &mut Option<PathBuf>) {
    ui.add_space(40.0);
    ui.vertical_centered(|ui| {
        ui.heading("Tableizer");
        ui.label("Open a file via a CLI argument, or pick a recent one below.");
        ui.add_space(12.0);
        for path in recent {
            if ui.button(path.display().to_string()).clicked() {
                *to_open = Some(path.clone());
            }
        }
    });
}

/// The virtualised grid, plus keyboard navigation and column-reorder application.
fn grid(ui: &mut egui::Ui, loaded: &mut LoadedTable, palette: &theme::Palette) {
    let LoadedTable {
        table,
        layout,
        encoding,
        view,
        ..
    } = loaded;
    let encoding: &'static Encoding = encoding;
    let total = match table.row_count() {
        RowCount::Exact(n) | RowCount::AtLeast(n) => n,
    };

    let displayed = layout.displayed();
    if displayed.is_empty() {
        ui.add_space(20.0);
        ui.vertical_centered(|ui| ui.label("All columns hidden — enable some in the View menu."));
        return;
    }
    let headers: Vec<String> = displayed
        .iter()
        .map(|&c| column_name(table.schema(), c, encoding))
        .collect();
    let table_columns: Vec<egui_table::Column> = (0..displayed.len())
        .map(|_| {
            egui_table::Column::new(180.0)
                .range(64.0..=900.0)
                .resizable(true)
        })
        .collect();
    let frozen = layout.frozen.min(displayed.len());

    // Keyboard: move the selected row + ⌘/Ctrl+C to copy it (unless typing in a text field).
    let mut scroll_to: Option<u64> = None;
    let mut copy_request = false;
    let typing = ui.ctx().memory(|m| m.focused().is_some());
    if !typing && total > 0 {
        let last = total - 1;
        const PAGE: u64 = 20;
        ui.input(|i| {
            if i.modifiers.command && i.key_pressed(egui::Key::C) {
                copy_request = true;
            }
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
        palette: palette.clone(),
        sort: view.sort,
        search: view.search.to_lowercase(),
        selected_row: view.selected_row,
        hovered_row: view.hovered_row,
        cache_start: 0,
        cache: Vec::new(),
        pending_reorder: None,
        new_hovered: None,
        clicked_row: None,
        copy_row: None,
        pending_sort: None,
    };

    let mut grid = egui_table::Table::new()
        .id_salt("tableizer-grid")
        .num_rows(total)
        .columns(table_columns)
        .num_sticky_cols(frozen)
        .headers(vec![egui_table::HeaderRow::new(palette.header_height)]);
    if let Some(row) = scroll_to {
        grid = grid.scroll_to_row(row, Some(egui::Align::Center));
    }
    grid.show(ui, &mut delegate);

    if let Some((dragged, target, after)) = delegate.pending_reorder {
        reorder(&mut layout.order, dragged, target, after);
    }
    // Clicking a column header cycles its sort: none → ascending → descending → none.
    if let Some(col) = delegate.pending_sort {
        view.sort = match view.sort {
            Some(s) if s.column == col && s.direction == Direction::Ascending => Some(SortKey {
                column: col,
                direction: Direction::Descending,
            }),
            Some(s) if s.column == col && s.direction == Direction::Descending => None,
            _ => Some(SortKey {
                column: col,
                direction: Direction::Ascending,
            }),
        };
    }
    if let Some(row) = delegate.clicked_row {
        view.selected_row = Some(row);
    }
    view.hovered_row = delegate.new_hovered;

    // Copy the targeted row (context "Copy row", or ⌘/Ctrl+C on the selection) as a TSV line.
    let copy_target = delegate.copy_row.or(if copy_request {
        view.selected_row
    } else {
        None
    });
    if let Some(row) = copy_target {
        let request = ViewportRequest {
            rows: RowRange { start: row, len: 1 },
            columns: delegate.columns.clone(),
        };
        if let Ok(viewport) = delegate.table.fetch(&request, &CancellationToken::new())
            && let Some(cells) = viewport.rows.first()
        {
            let line = cells
                .iter()
                .map(|cell| decode_field(&cell.0, delegate.encoding))
                .collect::<Vec<_>>()
                .join("\t");
            ui.ctx().copy_text(line);
        }
    }
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

/// The Parsing menu: delimiter (presets + custom), header row, and display encoding. Changing the
/// delimiter or header re-opens the file (column structure may change); encoding is display-only.
fn parsing_menu(ui: &mut egui::Ui, loaded: &mut LoadedTable) {
    ui.set_min_width(196.0);

    menu_section(ui, "DELIMITER");
    // Auto is the default; presets/custom are explicit overrides for when sniffing guessed wrong.
    let detected = loaded.detected_delimiter;
    if menu_choice(
        ui,
        186.0,
        loaded.delimiter_auto,
        None,
        &format!("Auto · detected {}", delimiter_label(detected)),
    ) {
        loaded.dialect.delimiter = detected;
        loaded.delimiter_auto = true;
        loaded.delimiter_input = delimiter_display(detected);
    }
    for (label, byte) in [
        ("Comma", b','),
        ("Tab", b'\t'),
        ("Semicolon", b';'),
        ("Pipe", b'|'),
    ] {
        if menu_choice(
            ui,
            186.0,
            !loaded.delimiter_auto && loaded.dialect.delimiter == byte,
            None,
            label,
        ) {
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

    menu_section(ui, "HEADER");
    ui.checkbox(&mut loaded.dialect.has_header, "First row is a header");

    menu_section(ui, "ENCODING");
    for choice in [encoding_rs::UTF_8, encoding_rs::WINDOWS_1252] {
        if menu_choice(
            ui,
            186.0,
            std::ptr::eq(loaded.encoding, choice),
            None,
            choice.name(),
        ) {
            loaded.encoding = choice;
        }
    }
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
    /// Resolved theme colors + metrics for painting.
    palette: theme::Palette,
    /// Active sort (for the header indicator), if any.
    sort: Option<SortKey>,
    /// Lowercased search query; cells containing it are highlighted (empty = no highlight).
    search: String,
    /// Selected display row to highlight, if any.
    selected_row: Option<u64>,
    /// Row under the mouse last frame (painted as hovered).
    hovered_row: Option<u64>,
    cache_start: u64,
    cache: Vec<Vec<Cell>>,
    /// Set by `header_cell_ui` when a column header is dropped onto another; applied after `show`.
    /// `(dragged, target, after)` — drop the dragged column before/after the target on release.
    pending_reorder: Option<(ColumnId, ColumnId, bool)>,
    /// Row whose cell was hovered this frame (read back after `show` to update the hover state).
    new_hovered: Option<u64>,
    /// Row whose cell was left-clicked this frame (read back to update the selection).
    clicked_row: Option<u64>,
    /// Row to copy as TSV (from the "Copy row" context item); handled after `show`.
    copy_row: Option<u64>,
    /// Column whose header was clicked this frame (read back to cycle the sort).
    pending_sort: Option<ColumnId>,
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

    fn default_row_height(&self) -> f32 {
        self.palette.row_height
    }

    fn header_cell_ui(&mut self, ui: &mut egui::Ui, cell: &egui_table::HeaderCellInfo) {
        let idx = cell.col_range.start;
        let (Some(&col_id), Some(name)) = (self.columns.get(idx), self.headers.get(idx)) else {
            return;
        };
        // Distinct header bar with a hairline beneath it.
        let rect = ui.max_rect();
        ui.painter()
            .rect_filled(rect, egui::CornerRadius::ZERO, self.palette.header_bg);
        ui.painter().hline(
            rect.x_range(),
            rect.bottom() - 0.5,
            egui::Stroke::new(1.0, self.palette.border),
        );
        let handle_id = egui::Id::new(("tz-col-handle", col_id.0));
        let grip_color = self.palette.header_text;
        let cell_h = rect.height();

        // A small ⋮-style grip on the left is the ONLY draggable area; the rest of the header is free
        // for other interactions. The grip is *painted* (three dots) — a glyph would be font-dependent
        // (and a text label would steal the drag as a text selection). Then the muted column name.
        ui.add_space(3.0);
        let handle = ui
            .dnd_drag_source(handle_id, col_id, |ui| {
                let (grip, _) =
                    ui.allocate_exact_size(egui::vec2(12.0, cell_h), egui::Sense::hover());
                let c = grip.center();
                for dy in [-4.0, 0.0, 4.0] {
                    ui.painter()
                        .circle_filled(egui::pos2(c.x, c.y + dy), 1.4, grip_color);
                }
            })
            .response;
        if handle.hovered() {
            ui.ctx().set_cursor_icon(egui::CursorIcon::Grab);
        }
        ui.add_space(2.0);
        let name_label = egui::Label::new(
            egui::RichText::new(name.to_uppercase())
                .color(self.palette.header_text)
                .size(11.0),
        )
        .selectable(false);
        // Auto-size (double-click separator) must also fit the header name → measure it in full.
        ui.add(if ui.is_sizing_pass() {
            name_label.wrap_mode(egui::TextWrapMode::Extend)
        } else {
            name_label.truncate()
        });

        // Sort indicator: a small accent triangle on the sorted column (painted, not a glyph).
        if let Some(sort) = self.sort
            && sort.column == col_id
        {
            let cx = rect.right() - 9.0;
            let cy = rect.center().y;
            let points = if sort.direction == Direction::Ascending {
                vec![
                    egui::pos2(cx - 4.0, cy + 2.5),
                    egui::pos2(cx + 4.0, cy + 2.5),
                    egui::pos2(cx, cy - 3.0),
                ]
            } else {
                vec![
                    egui::pos2(cx - 4.0, cy - 2.5),
                    egui::pos2(cx + 4.0, cy - 2.5),
                    egui::pos2(cx, cy + 3.0),
                ]
            };
            ui.painter().add(egui::Shape::convex_polygon(
                points,
                self.palette.accent,
                egui::Stroke::NONE,
            ));
        }

        // Dim the column currently being dragged.
        if ui.ctx().is_being_dragged(handle_id) {
            ui.painter()
                .rect_filled(rect, egui::CornerRadius::ZERO, self.palette.row_hover);
        }

        // The whole cell is a drop target. The dragged column lands on the *far* side of this column
        // from where it came: if it's currently to our left (dragging right) it drops after us, else
        // before — so the insertion jumps a whole column the moment the cursor crosses a border.
        let drop = ui.interact(
            rect,
            egui::Id::new(("tz-col-drop", col_id.0)),
            egui::Sense::hover(),
        );
        if drop
            .dnd_hover_payload::<ColumnId>()
            .is_some_and(|dragged| *dragged != col_id)
        {
            // Highlight the whole header cell — the dragged column will take this column's slot.
            ui.painter()
                .rect_filled(rect, egui::CornerRadius::ZERO, self.palette.row_selected);
            ui.painter().rect_stroke(
                rect,
                egui::CornerRadius::ZERO,
                egui::Stroke::new(2.0, self.palette.accent),
                egui::StrokeKind::Inside,
            );
        }
        if let Some(dragged) = drop.dnd_release_payload::<ColumnId>()
            && *dragged != col_id
        {
            let after = self
                .columns
                .iter()
                .position(|&c| c == *dragged)
                .is_some_and(|src| src < idx);
            self.pending_reorder = Some((*dragged, col_id, after));
        }

        // Click the header body (right of the grip) to cycle this column's sort.
        let sort_rect =
            egui::Rect::from_min_max(egui::pos2(handle.rect.right() + 4.0, rect.top()), rect.max);
        let sort_click = ui.interact(
            sort_rect,
            egui::Id::new(("tz-col-sort", col_id.0)),
            egui::Sense::click(),
        );
        if sort_click.hovered() {
            ui.ctx().set_cursor_icon(egui::CursorIcon::PointingHand);
        }
        if sort_click.clicked() {
            self.pending_sort = Some(col_id);
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

        let cell_rect = ui.max_rect();

        // Whole-cell interaction: left-click selects the row, hover drives the row highlight,
        // right-click opens the copy menu.
        let response = ui.interact(
            cell_rect,
            ui.id().with((cell.row_nr, cell.col_nr)),
            egui::Sense::click(),
        );
        if response.clicked() {
            self.clicked_row = Some(cell.row_nr);
        }
        if response.hovered() {
            self.new_hovered = Some(cell.row_nr);
        }

        // Backgrounds, least- to most-specific: stripe → hover → selection → search match.
        if cell.row_nr % 2 == 1 {
            ui.painter()
                .rect_filled(cell_rect, egui::CornerRadius::ZERO, self.palette.stripe);
        }
        if Some(cell.row_nr) == self.hovered_row {
            ui.painter()
                .rect_filled(cell_rect, egui::CornerRadius::ZERO, self.palette.row_hover);
        }
        if Some(cell.row_nr) == self.selected_row {
            ui.painter().rect_filled(
                cell_rect,
                egui::CornerRadius::ZERO,
                self.palette.row_selected,
            );
        }
        if cell_matches(&text, &self.search) {
            ui.painter().rect_filled(
                cell_rect,
                egui::CornerRadius::ZERO,
                self.palette.search_match,
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
        let font = self.palette.table_font.clone();
        let label = if text.is_empty() {
            egui::Label::new(egui::RichText::new("—").weak().font(font))
        } else {
            egui::Label::new(egui::RichText::new(text.as_str()).font(font))
        }
        .selectable(false);
        // During the auto-size (sizing) pass, measure the full text rather than truncating, so a
        // double-clicked separator fits the widest value.
        let label = if ui.is_sizing_pass() {
            label.wrap_mode(egui::TextWrapMode::Extend)
        } else {
            label.truncate()
        };
        // 10px of padding on both sides so text never touches the column edge (and so auto-size
        // leaves the same margin); numeric columns align right.
        let layout = if numeric {
            egui::Layout::right_to_left(egui::Align::Center)
        } else {
            egui::Layout::left_to_right(egui::Align::Center)
        };
        ui.with_layout(layout, |ui| {
            ui.add_space(10.0);
            ui.add(label);
            ui.add_space(10.0);
        });

        // Right-click: copy this cell, or the whole row as TSV (handled after `show`).
        response.context_menu(|ui| {
            if ui.button("Copy cell").clicked() {
                ui.ctx().copy_text(text.clone());
                ui.close();
            }
            if ui.button("Copy row").clicked() {
                self.copy_row = Some(cell.row_nr);
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
        reorder(&mut order, ColumnId(3), ColumnId(1), false);
        assert_eq!(
            order,
            vec![ColumnId(0), ColumnId(3), ColumnId(1), ColumnId(2)]
        );
    }

    #[test]
    fn reorder_after_the_target_inserts_to_its_right() {
        // Drag column 0 to *after* column 2 (right half of the target).
        let mut order = vec![ColumnId(0), ColumnId(1), ColumnId(2), ColumnId(3)];
        reorder(&mut order, ColumnId(0), ColumnId(2), true);
        assert_eq!(
            order,
            vec![ColumnId(1), ColumnId(2), ColumnId(0), ColumnId(3)]
        );
    }

    #[test]
    fn reorder_dragging_onto_itself_is_a_noop() {
        let mut order = vec![ColumnId(0), ColumnId(1)];
        reorder(&mut order, ColumnId(1), ColumnId(1), false);
        assert_eq!(order, vec![ColumnId(0), ColumnId(1)]);
    }

    #[test]
    fn displayed_columns_respect_visibility_and_order() {
        let mut layout = GridLayout::new(3);
        layout.visible[1] = false;
        reorder(&mut layout.order, ColumnId(2), ColumnId(0), false);
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

    #[test]
    fn parses_single_char_and_hex_delimiters() {
        assert_eq!(parse_delimiter(","), Some(b','));
        assert_eq!(parse_delimiter(":"), Some(b':'));
        assert_eq!(parse_delimiter("0x01"), Some(1)); // Hive/Unix Ctrl-A
        assert_eq!(parse_delimiter("\\x09"), Some(b'\t'));
        assert_eq!(parse_delimiter(""), None); // mid-entry: not yet a delimiter
        assert_eq!(parse_delimiter("ab"), None); // not a single byte
        assert_eq!(parse_delimiter("é"), None); // multi-byte char isn't a single delimiter
    }

    #[test]
    fn delimiter_display_round_trips_through_parse() {
        for byte in [b',', b'\t', b'|', 1u8, 0x1f] {
            assert_eq!(parse_delimiter(&delimiter_display(byte)), Some(byte));
        }
    }
}
