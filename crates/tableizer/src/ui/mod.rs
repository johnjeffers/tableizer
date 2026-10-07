//! The egui rendering layer: top-level window panels (toolbar, status bar, empty state) and — via
//! submodules — the menu bar, the right-side tabbed panel (Columns / Parsing / Settings), and the
//! data grid.
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
pub(crate) use menu::{ExportKind, ExportRequest, columns_tab, hide_search, menu_bar, parsing_tab};
pub(crate) use settings::settings_tab;

use std::path::{Path, PathBuf};

use eframe::egui;
use tableizer_core::RowCount;

use crate::model::{LoadedTable, ViewControls, format_label};
use crate::theme;

/// egui's standard menu look (`menu_style`) with roomier horizontal item padding, so the highlight
/// behind a hovered/selected item — and the menu-bar buttons — isn't cramped against the text.
/// Applied to the menu bar and every menu/submenu popup; vertical padding is left as `menu_style`
/// sets it.
pub(crate) fn wide_menu(style: &mut egui::Style) {
    egui::containers::menu::menu_style(style);
    style.spacing.button_padding.x = 6.0;
}

/// The toolbar (search bar): the find/filter controls. `focus_find` requests focus on the Find field
/// (⌘/Ctrl+F). Returns `true` when it asks to be hidden: its close button, or Esc in the Find field.
pub(crate) fn toolbar(ui: &mut egui::Ui, view: &mut ViewControls, focus_find: bool) -> bool {
    // The ✕ at the right end, laid out first; the controls fill (and wrap within) the rest.
    let row = egui::Layout::right_to_left(egui::Align::Center);
    ui.with_layout(row, |ui| {
        let close = close_button(ui, "Hide search (clears it)").clicked();
        let controls = egui::Layout::left_to_right(egui::Align::Center).with_main_wrap(true);
        let escaped = ui.with_layout(controls, |ui| find_controls(ui, view, focus_find));
        close || escaped.inner
    })
    .inner
}

/// The search bar's find/filter controls, left to right. Returns `true` when Esc was pressed in the
/// Find field.
fn find_controls(ui: &mut egui::Ui, view: &mut ViewControls, focus_find: bool) -> bool {
    ui.label("Find:");
    let find = ui.add(
        egui::TextEdit::singleline(&mut view.search)
            .hint_text("substring or regex")
            .desired_width(180.0),
    );
    if focus_find {
        find.request_focus();
    }
    // Prev/Next jump the selection between matches across the whole file (a background scan, so a
    // far-off match never freezes the UI). Enabled whenever there's a query; degenerate but
    // harmless under "Show matches only" (every visible row matches there).
    let has_query = !view.search.is_empty();
    if ui
        .add_enabled(has_query, egui::Button::new("<"))
        .on_hover_text("Previous match (above the selection)")
        .clicked()
    {
        view.find_request = Some(false);
    }
    if ui
        .add_enabled(has_query, egui::Button::new(">"))
        .on_hover_text("Next match (below the selection)")
        .clicked()
    {
        view.find_request = Some(true);
    }
    ui.checkbox(&mut view.filter_mode, "Show matches only");
    ui.checkbox(&mut view.regex, "Use regex");
    ui.checkbox(&mut view.case_sensitive, "Match case");
    ui.checkbox(&mut view.invert, "Invert search");
    // Esc makes egui drop the field's focus as the frame starts, so it shows up here as focus lost
    // this frame with Esc pressed (the same way egui reports Enter).
    find.lost_focus() && ui.input(|i| i.key_pressed(egui::Key::Escape))
}

/// A small ✕ close button drawn as two strokes (shapes, not a glyph — font-independent, per the module
/// invariant), with `tooltip` on hover. Returns its click response.
pub(crate) fn close_button(ui: &mut egui::Ui, tooltip: &str) -> egui::Response {
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
    response.on_hover_text(tooltip)
}

/// The bottom status bar: path · format · cols/rows · indexing/view-build progress · data-quality ·
/// errors · selection.
pub(crate) fn status_bar(ui: &mut egui::Ui, loaded: &LoadedTable, palette: &theme::Palette) {
    let (total, indexing) = match loaded.table.row_count() {
        RowCount::Exact(n) => (n, false),
        RowCount::AtLeast(n) => (n, true),
    };
    let cols = loaded.table.schema().columns.len() as u64;
    ui.horizontal(|ui| {
        ui.label(&loaded.origin);
        ui.separator();
        ui.label(format_label(loaded.format, &loaded.dialect));
        ui.separator();
        // "n cols, n rows" — the row count is a growing lower bound (≥) while the index builds.
        if indexing {
            ui.label(format!(
                "{} cols, ≥ {} rows",
                fmt_count(cols),
                fmt_count(total)
            ));
            ui.spinner();
            ui.ctx().request_repaint();
        } else {
            ui.label(format!(
                "{} cols, {} rows",
                fmt_count(cols),
                fmt_count(total)
            ));
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

/// The rows of a start-screen list (the browse tree, the recents), styled like the data grid's: for
/// items the whole row, full width, taking the click and the `hover` highlight, and — given a `stripe`
/// color (the tree; not the recents) — alternating backgrounds, rows counted in display order across
/// every level.
pub(crate) struct ListRows {
    stripe: Option<egui::Color32>,
    hover: egui::Color32,
    /// The next row's position in display order.
    row: usize,
}

impl ListRows {
    pub(crate) fn new(stripe: Option<egui::Color32>, hover: egui::Color32) -> Self {
        Self {
            stripe,
            hover,
            row: 0,
        }
    }

    /// Lay out a status row ("Listing…", an error) with `add_contents` (left to right): striped like
    /// any row, but not interactive.
    pub(crate) fn status(&mut self, ui: &mut egui::Ui, add_contents: impl FnOnce(&mut egui::Ui)) {
        // Reserve the background's place before the row's contents are painted; its size is only
        // known once they're laid out.
        let background = ui.painter().add(egui::Shape::Noop);
        let row = ui.horizontal(add_contents).response.rect;
        if let Some(stripe) = self.stripe_now() {
            ui.painter().set(
                background,
                egui::Shape::rect_filled(band(ui, row), egui::CornerRadius::ZERO, stripe),
            );
        }
        self.row += 1;
    }

    /// Lay out a file or folder row with `add_contents` (left to right; non-interactive contents —
    /// the row itself takes the click), returning the whole row's response. Like a table row: the full
    /// width responds, and hovering it paints `hover` over its stripe, behind its contents.
    pub(crate) fn item(
        &mut self,
        ui: &mut egui::Ui,
        id: egui::Id,
        add_contents: impl FnOnce(&mut egui::Ui),
    ) -> egui::Response {
        // Backgrounds, least- to most-specific (as in the grid): stripe → hover. Their places are
        // reserved before the contents are painted; their size is only known once laid out.
        let stripe = ui.painter().add(egui::Shape::Noop);
        let hover = ui.painter().add(egui::Shape::Noop);
        let row = ui.horizontal(add_contents).response.rect;
        let band = band(ui, row);
        let response = ui.interact(band, id, egui::Sense::click());
        let fill = |color| egui::Shape::rect_filled(band, egui::CornerRadius::ZERO, color);
        if let Some(color) = self.stripe_now() {
            ui.painter().set(stripe, fill(color));
        }
        if response.hovered() {
            ui.painter().set(hover, fill(self.hover));
        }
        self.row += 1;
        response
    }

    /// The current row's stripe color, if it is striped: every other row, when striping at all.
    fn stripe_now(&self) -> Option<egui::Color32> {
        self.stripe.filter(|_| self.row % 2 == 1)
    }
}

/// A list row's background band: the full width, and half the gap above and below, so neighbouring
/// bands meet evenly.
fn band(ui: &egui::Ui, row: egui::Rect) -> egui::Rect {
    let half_gap = ui.spacing().item_spacing.y / 2.0;
    egui::Rect::from_x_y_ranges(ui.max_rect().x_range(), row.y_range())
        .expand2(egui::vec2(0.0, half_gap))
}

/// The start-screen **controls column** (left side of the landing): the recent-files list. Files are
/// opened from the browser column to its right (see `show_landing`) — there is no OS file picker or
/// URL dialog. Rows highlight on hover like the table's (from `palette`), unstriped. Sets `to_open` when a recent entry is
/// clicked, and `clear_recent` when the list's
/// Clear button is (the File menu's Clear Recents does the same).
pub(crate) fn empty_view(
    ui: &mut egui::Ui,
    recent: &[PathBuf],
    palette: &theme::Palette,
    to_open: &mut Option<PathBuf>,
    clear_recent: &mut bool,
) {
    ui.add_space(10.0);
    ui.label("Browse for a file on the right, or pick a recent.");
    if !recent.is_empty() {
        ui.add_space(20.0);
        ui.horizontal(|ui| {
            ui.label(egui::RichText::new("RECENT").weak());
            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                if ui
                    .small_button("Clear")
                    .on_hover_text("Clear the recent files list")
                    .clicked()
                {
                    *clear_recent = true;
                }
            });
        });
        ui.add_space(6.0);
        // Rows highlighted like the table's (and the browser's): the whole row hovering and taking the
        // click — but unstriped, as a short list. Names are middle-elided so a long key keeps its start + extension; the full
        // path/URL shows on hover. Scrolls within the column.
        egui::ScrollArea::vertical()
            .auto_shrink([false, false])
            .show(ui, |ui| {
                let mut rows = ListRows::new(None, palette.row_hover);
                for path in recent {
                    let name = elide_middle(&recent_name(path), 36);
                    let row = rows.item(ui, ui.id().with(path), |ui| {
                        ui.add_space(4.0);
                        ui.add(egui::Label::new(name).selectable(false));
                    });
                    if row.on_hover_text(path.display().to_string()).clicked() {
                        *to_open = Some(path.clone());
                    }
                }
            });
    }
}

/// The display name for a recent entry: its file/object name (the last path segment), or the whole
/// path/URL if it has none.
fn recent_name(path: &Path) -> String {
    path.file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_else(|| path.display().to_string())
}

/// Middle-elide `s` to at most `max` characters, keeping the start and end (so a file extension stays
/// visible): a long `…fusionauth-alb-acl_20260524T2355Z_43f57849.log.gz` becomes `…43f57849.log.gz`.
fn elide_middle(s: &str, max: usize) -> String {
    let chars: Vec<char> = s.chars().collect();
    if chars.len() <= max {
        return s.to_string();
    }
    let keep = max.saturating_sub(1); // room for the ellipsis
    let tail = keep / 2;
    let head = keep - tail;
    let start: String = chars[..head].iter().collect();
    let end: String = chars[chars.len() - tail..].iter().collect();
    format!("{start}…{end}")
}

/// Format a byte count in binary units (KiB/MiB/GiB), for the download progress dialog.
pub(crate) fn fmt_bytes(n: u64) -> String {
    const UNITS: [&str; 5] = ["B", "KiB", "MiB", "GiB", "TiB"];
    let mut value = n as f64;
    let mut unit = 0;
    while value >= 1024.0 && unit < UNITS.len() - 1 {
        value /= 1024.0;
        unit += 1;
    }
    if unit == 0 {
        format!("{n} B")
    } else {
        format!("{value:.1} {}", UNITS[unit])
    }
}

/// Format a row count with thousands separators.
pub(crate) fn fmt_count(n: u64) -> String {
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

#[cfg(test)]
mod tests {
    use super::*;

    const STRIPE: egui::Color32 = egui::Color32::from_rgb(1, 2, 3);
    const HOVER: egui::Color32 = egui::Color32::from_rgb(4, 5, 6);

    /// Renders [`empty_view`] headlessly, frame by frame, recording what it asks for.
    struct RecentsHarness {
        ctx: egui::Context,
        recent: Vec<PathBuf>,
        palette: theme::Palette,
        to_open: Option<PathBuf>,
        clear_recent: bool,
        /// The view's full width.
        width: f32,
    }

    impl RecentsHarness {
        fn new(recent: &[&str]) -> Self {
            let (_, mut palette) = theme::build(&theme::Settings::default(), false);
            palette.stripe = STRIPE;
            palette.row_hover = HOVER;
            let mut harness = Self {
                ctx: egui::Context::default(),
                recent: recent.iter().map(PathBuf::from).collect(),
                palette,
                to_open: None,
                clear_recent: false,
                width: 0.0,
            };
            harness.frame(Vec::new()); // lay out once, so input finds the rows
            harness
        }

        /// One frame with `events`: the shapes painted, in paint order.
        fn frame(&mut self, events: Vec<egui::Event>) -> Vec<egui::Shape> {
            let input = egui::RawInput {
                events,
                ..egui::RawInput::default()
            };
            let Self {
                ctx,
                recent,
                palette,
                to_open,
                clear_recent,
                width,
            } = self;
            let mut output = ctx.run_ui(input, |ui| {
                *width = ui.max_rect().width();
                empty_view(ui, recent, palette, to_open, clear_recent);
            });
            output.textures_delta.clear(); // no renderer to upload the font atlas to
            output.shapes.into_iter().map(|c| c.shape).collect()
        }

        /// Where `text` is painted, if it is.
        fn text_rect(&mut self, text: &str) -> Option<egui::Rect> {
            self.frame(Vec::new()).iter().find_map(|shape| match shape {
                egui::Shape::Text(t) if t.galley.text() == text => {
                    Some(t.galley.rect.translate(t.pos.to_vec2()))
                }
                _ => None,
            })
        }

        /// Move the pointer to `pos`; the next frame's shapes.
        fn hover(&mut self, pos: egui::Pos2) -> Vec<egui::Shape> {
            self.frame(vec![egui::Event::PointerMoved(pos)]);
            self.frame(Vec::new())
        }

        /// Click at `pos` (pointer moved there, pressed, then released over a few frames).
        fn click(&mut self, pos: egui::Pos2) {
            let button = |pressed| egui::Event::PointerButton {
                pos,
                button: egui::PointerButton::Primary,
                pressed,
                modifiers: egui::Modifiers::NONE,
            };
            self.frame(vec![egui::Event::PointerMoved(pos)]);
            self.frame(vec![button(true)]);
            self.frame(vec![button(false)]);
        }
    }

    /// The rects filled with `color` in `shapes`, in paint order.
    fn filled(shapes: &[egui::Shape], color: egui::Color32) -> Vec<egui::Rect> {
        shapes
            .iter()
            .filter_map(|shape| match shape {
                egui::Shape::Rect(r) if r.fill == color => Some(r.rect),
                _ => None,
            })
            .collect()
    }

    #[test]
    fn empty_view_clear_button_asks_to_clear_the_recents() {
        let mut h = RecentsHarness::new(&["/data/a.csv", "/data/b.csv"]);
        let clear = h.text_rect("Clear").unwrap();
        h.click(clear.center());
        assert_eq!((h.to_open, h.clear_recent), (None, true));
    }

    #[test]
    fn empty_view_opens_a_clicked_recent() {
        let mut h = RecentsHarness::new(&["/data/a.csv", "/data/b.csv"]);
        let name = h.text_rect("b.csv").unwrap();
        h.click(name.center());
        assert_eq!(h.to_open, Some(PathBuf::from("/data/b.csv")));
        assert!(!h.clear_recent);
    }

    #[test]
    fn empty_view_opens_a_recent_clicked_anywhere_on_its_row() {
        let mut h = RecentsHarness::new(&["/data/a.csv", "/data/b.csv"]);
        let name = h.text_rect("b.csv").unwrap();
        h.click(egui::pos2(h.width - 10.0, name.center().y));
        assert_eq!(h.to_open, Some(PathBuf::from("/data/b.csv")));
    }

    #[test]
    fn empty_view_hover_highlights_the_whole_row_like_a_table() {
        let mut h = RecentsHarness::new(&["/data/a.csv", "/data/b.csv"]);
        let name = h.text_rect("a.csv").unwrap();
        let shapes = h.hover(name.center());
        let bands = filled(&shapes, HOVER);
        assert_eq!(bands.len(), 1, "one hovered row");
        assert!(bands[0].contains(name.center()));
        assert!(
            (bands[0].width() - h.width).abs() < 1.0,
            "full width: {} of {}",
            bands[0].width(),
            h.width
        );
        // The table's highlight is the only one under the pointer: no per-label hover box.
        let others: Vec<_> = shapes
            .iter()
            .filter_map(|s| match s {
                egui::Shape::Rect(r)
                    if ![STRIPE, HOVER].contains(&r.fill)
                        && r.fill.a() > 0
                        && r.rect.contains(name.center()) =>
                {
                    Some(r.rect)
                }
                _ => None,
            })
            .collect();
        assert_eq!(others, [], "no other highlight");
    }

    #[test]
    fn empty_view_does_not_stripe_the_recents() {
        let mut h = RecentsHarness::new(&["/data/a.csv", "/data/b.csv", "/data/c.csv"]);
        assert_eq!(filled(&h.frame(Vec::new()), STRIPE), []);
    }

    #[test]
    fn empty_view_has_no_clear_button_without_recents() {
        let mut h = RecentsHarness::new(&[]);
        assert_eq!(h.text_rect("Clear"), None);
    }

    /// Render [`toolbar`] headlessly in a 900-pt-wide window, clicking at the point `at` picks from
    /// the first frame's shapes; whether the toolbar asked to close.
    fn click_toolbar(at: impl Fn(&[egui::Shape]) -> egui::Pos2) -> bool {
        let ctx = egui::Context::default();
        let mut view = ViewControls::default();
        let mut closed = false;
        let mut frame = |events: Vec<egui::Event>| {
            let input = egui::RawInput {
                events,
                screen_rect: Some(egui::Rect::from_min_size(
                    egui::Pos2::ZERO,
                    egui::vec2(900.0, 600.0),
                )),
                ..egui::RawInput::default()
            };
            let mut output = ctx.run_ui(input, |ui| {
                closed |= toolbar(ui, &mut view, false);
            });
            output.textures_delta.clear(); // no renderer to upload the font atlas to
            output
                .shapes
                .into_iter()
                .map(|c| c.shape)
                .collect::<Vec<_>>()
        };
        let pos = at(&frame(Vec::new()));
        let button = |pressed| egui::Event::PointerButton {
            pos,
            button: egui::PointerButton::Primary,
            pressed,
            modifiers: egui::Modifiers::NONE,
        };
        frame(vec![egui::Event::PointerMoved(pos)]);
        frame(vec![button(true)]);
        frame(vec![button(false)]);
        closed
    }

    /// The centre of the toolbar's ✕: the rightmost painted line segment.
    fn toolbar_close(shapes: &[egui::Shape]) -> egui::Pos2 {
        shapes
            .iter()
            .filter_map(|s| match s {
                egui::Shape::LineSegment { points, .. } => Some(egui::pos2(
                    (points[0].x + points[1].x) / 2.0,
                    (points[0].y + points[1].y) / 2.0,
                )),
                _ => None,
            })
            .max_by(|a, b| a.x.total_cmp(&b.x))
            .expect("the toolbar's ✕")
    }

    #[test]
    fn toolbar_stays_one_row_tall_in_its_panel() {
        // The ✕ and the controls share one row at a normal window width.
        let ctx = egui::Context::default();
        let mut view = ViewControls::default();
        let mut height = 0.0;
        let input = egui::RawInput {
            screen_rect: Some(egui::Rect::from_min_size(
                egui::Pos2::ZERO,
                egui::vec2(900.0, 600.0),
            )),
            ..egui::RawInput::default()
        };
        for _ in 0..3 {
            let mut output = ctx.run_ui(input.clone(), |ui| {
                let panel = egui::Panel::top("toolbar").show(ui, |ui| {
                    toolbar(ui, &mut view, false);
                });
                height = panel.response.rect.height();
            });
            output.textures_delta.clear();
        }
        assert!(height < 40.0, "the search bar is {height} pt tall");
    }

    /// Render [`toolbar`] headlessly over a few frames — the first with `focus_find` — then press
    /// `key`; whether the toolbar asked to close.
    fn press_in_toolbar(focus_find: bool, key: egui::Key) -> bool {
        let ctx = egui::Context::default();
        let mut view = ViewControls::default();
        let mut closed = false;
        let mut frame = |events: Vec<egui::Event>, focus: bool| {
            let input = egui::RawInput {
                events,
                ..egui::RawInput::default()
            };
            let mut output = ctx.run_ui(input, |ui| {
                closed |= toolbar(ui, &mut view, focus);
            });
            output.textures_delta.clear(); // no renderer to upload the font atlas to
        };
        frame(Vec::new(), focus_find);
        frame(Vec::new(), false); // the focus request lands
        frame(
            vec![egui::Event::Key {
                key,
                physical_key: None,
                pressed: true,
                repeat: false,
                modifiers: egui::Modifiers::NONE,
            }],
            false,
        );
        closed
    }

    #[test]
    fn toolbar_esc_in_the_find_field_asks_to_hide_the_search_bar() {
        assert!(press_in_toolbar(true, egui::Key::Escape));
    }

    #[test]
    fn toolbar_esc_elsewhere_does_not_close_it() {
        assert!(!press_in_toolbar(false, egui::Key::Escape));
    }

    #[test]
    fn toolbar_other_keys_in_the_find_field_do_not_close_it() {
        assert!(!press_in_toolbar(true, egui::Key::Enter));
    }

    #[test]
    fn toolbar_close_button_asks_to_hide_the_search_bar() {
        assert!(click_toolbar(toolbar_close));
    }

    #[test]
    fn toolbar_close_button_sits_at_the_right_end() {
        let at = std::cell::Cell::new(egui::Pos2::ZERO);
        click_toolbar(|shapes| {
            at.set(toolbar_close(shapes));
            egui::Pos2::ZERO
        });
        assert!(
            at.get().x > 850.0,
            "the ✕ is at the bar's right end: {:?}",
            at.get()
        );
    }

    #[test]
    fn toolbar_does_not_close_on_other_clicks() {
        let label = |shapes: &[egui::Shape]| {
            shapes
                .iter()
                .find_map(|s| match s {
                    egui::Shape::Text(t) if t.galley.text() == "Find:" => {
                        Some(t.galley.rect.translate(t.pos.to_vec2()).center())
                    }
                    _ => None,
                })
                .unwrap()
        };
        assert!(!click_toolbar(label));
    }

    #[test]
    fn elide_middle_keeps_start_and_end() {
        // Short strings pass through unchanged.
        assert_eq!(elide_middle("short.csv", 58), "short.csv");
        // Long names keep both ends (so the extension survives) around a single ellipsis.
        let long =
            "121700706967_waflogs_ap-northeast-1_fusionauth-alb-acl_20260524T2355Z_43f57849.log.gz";
        let elided = elide_middle(long, 40);
        assert_eq!(elided.chars().count(), 40);
        assert!(elided.starts_with("121700706967"));
        assert!(elided.ends_with("43f57849.log.gz"));
        assert!(elided.contains('…'));
    }
}
