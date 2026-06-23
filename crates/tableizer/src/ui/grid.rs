//! The virtualised data grid: keyboard navigation, row selection, column sort/reorder, and the
//! `egui_table` delegate that pulls only the visible row window from the engine each frame.

use eframe::egui;
use encoding_rs::Encoding;
use tableizer_core::{
    CancellationToken, Cell, ColumnId, Direction, InferredType, RowCount, RowRange, SortKey,
    ViewportRequest, ViewportSource,
};

use crate::model::{LoadedTable, RowSpan, cell_matches, column_name, decode_field, reorder};
use crate::theme;

/// The virtualised grid, plus keyboard navigation and column-reorder application.
pub(crate) fn grid(ui: &mut egui::Ui, loaded: &mut LoadedTable, palette: &theme::Palette) {
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

    // Keyboard: move the selection (Shift extends it) + ⌘/Ctrl+C to copy it (unless typing).
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
            let current = view.selected.map(|s| s.lead);
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
                // Shift+move extends from the existing anchor; otherwise collapse to a single row.
                view.selected = Some(match (view.selected, i.modifiers.shift) {
                    (Some(s), true) => RowSpan {
                        anchor: s.anchor,
                        lead: next,
                    },
                    _ => RowSpan::single(next),
                });
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
        search: if view.case_sensitive {
            view.search.clone()
        } else {
            view.search.to_lowercase()
        },
        search_case_sensitive: view.case_sensitive,
        selected: view.selected,
        drag_active: view.selecting,
        hovered_row: view.hovered_row,
        cache_start: 0,
        cache: Vec::new(),
        pending_reorder: None,
        new_hovered: None,
        clicked_row: None,
        drag_started_row: None,
        drag_lead_row: None,
        copy_row: None,
        copy_selected: false,
        pending_sort: None,
    };

    let mut grid = egui_table::Table::new()
        .id_salt("tableizer-grid")
        .num_rows(total)
        .columns(table_columns)
        .num_sticky_cols(0)
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
    // Row selection: a drag selects a range (anchor at the start, lead following the pointer); a
    // plain click selects one row. The drag ends when the pointer is released.
    if let Some(start) = delegate.drag_started_row {
        view.selected = Some(RowSpan::single(start));
        view.selecting = true;
    } else if view.selecting
        && let Some(lead) = delegate.drag_lead_row
        && let Some(span) = view.selected.as_mut()
    {
        span.lead = lead;
    }
    if let Some(row) = delegate.clicked_row {
        view.selected = Some(RowSpan::single(row));
    }
    if view.selecting && !ui.input(|i| i.pointer.primary_down()) {
        view.selecting = false;
    }
    view.hovered_row = delegate.new_hovered;

    // Copy the targeted rows as TSV lines: "Copy Row" → one row; "Copy Selected" or ⌘/Ctrl+C → the
    // whole selection.
    let copy_span =
        delegate
            .copy_row
            .map(RowSpan::single)
            .or(if copy_request || delegate.copy_selected {
                view.selected
            } else {
                None
            });
    if let Some(span) = copy_span {
        let request = ViewportRequest {
            rows: RowRange {
                start: span.lo(),
                len: u32::try_from(span.len()).unwrap_or(u32::MAX),
            },
            columns: delegate.columns.clone(),
        };
        if let Ok(viewport) = delegate.table.fetch(&request, &CancellationToken::new()) {
            let text = viewport
                .rows
                .iter()
                .map(|cells| {
                    cells
                        .iter()
                        .map(|cell| decode_field(&cell.0, delegate.encoding))
                        .collect::<Vec<_>>()
                        .join("\t")
                })
                .collect::<Vec<_>>()
                .join("\n");
            if !text.is_empty() {
                ui.ctx().copy_text(text);
            }
        }
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
    /// Search query for the highlight: raw when `search_case_sensitive`, else lowercased (the cell
    /// text is lowercased to match). Cells containing it are highlighted (empty = no highlight).
    search: String,
    /// Whether the highlight match is case-sensitive.
    search_case_sensitive: bool,
    /// Selected display rows to highlight, if any.
    selected: Option<RowSpan>,
    /// Whether a row-selection drag is in progress (cells extend the selection to the pointer).
    drag_active: bool,
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
    /// Row where a selection drag began this frame (read back to anchor the selection).
    drag_started_row: Option<u64>,
    /// Row under the pointer during an active drag (read back to extend the selection).
    drag_lead_row: Option<u64>,
    /// Row to copy as TSV (from the "Copy Row" context item); handled after `show`.
    copy_row: Option<u64>,
    /// Set by the "Copy Selected" context item: copy the whole selection after `show`.
    copy_selected: bool,
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
                .strong()
                .text_style(theme::text_style(theme::COLUMN_HEADER)),
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

        // Whole-cell interaction: left-click selects the row; click-drag selects a range (the cell
        // claims the drag, so the table doesn't scroll); hover drives the highlight; right-click
        // opens the copy menu.
        let response = ui.interact(
            cell_rect,
            ui.id().with((cell.row_nr, cell.col_nr)),
            egui::Sense::click_and_drag(),
        );
        if response.clicked() {
            self.clicked_row = Some(cell.row_nr);
        }
        if response.drag_started() {
            self.drag_started_row = Some(cell.row_nr);
        }
        // While dragging, the cell under the pointer is the selection's lead (geometric test, since
        // egui ties `dragged()` to the cell where the drag began, not the one now under the cursor).
        if self.drag_active
            && ui
                .input(|i| i.pointer.interact_pos())
                .is_some_and(|p| cell_rect.contains(p))
        {
            self.drag_lead_row = Some(cell.row_nr);
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
        if self.selected.is_some_and(|s| s.contains(cell.row_nr)) {
            ui.painter().rect_filled(
                cell_rect,
                egui::CornerRadius::ZERO,
                self.palette.row_selected,
            );
        }
        if cell_matches(&text, &self.search, self.search_case_sensitive) {
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
            egui::Label::new(
                egui::RichText::new(text.as_str())
                    .font(font)
                    .color(self.palette.table_text),
            )
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

        // Right-click: copy this cell, the whole row, or the current selection (handled after `show`).
        response.context_menu(|ui| {
            if ui.button("Copy Cell").clicked() {
                ui.ctx().copy_text(text.clone());
                ui.close();
            }
            if ui.button("Copy Row").clicked() {
                self.copy_row = Some(cell.row_nr);
                ui.close();
            }
            if ui
                .add_enabled(self.selected.is_some(), egui::Button::new("Copy Selected"))
                .clicked()
            {
                self.copy_selected = true;
                ui.close();
            }
        });
    }
}
