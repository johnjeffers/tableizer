//! The app's data and per-file UI state: column layout, row selection, the find/sort/filter
//! controls, and the loaded-table aggregate, plus the small pure helpers (dialect sniffing, field
//! decoding, delimiter parsing) shared across the UI. The pure helpers are unit-tested below.

use std::io::Read;
use std::path::{Path, PathBuf};

use encoding_rs::Encoding;
use tableizer_core::{
    ColumnId, CsvTable, Direction, FilterSpec, Schema, SortKey, ViewSpec, ViewportSource,
    parse::Dialect,
};

/// Read a head sample and auto-detect the dialect (delimiter + header); fall back to the default.
pub(crate) fn sniff_file(path: &Path) -> Dialect {
    let mut head = vec![0u8; 64 * 1024];
    let read = std::fs::File::open(path)
        .and_then(|mut f| f.read(&mut head))
        .unwrap_or(0);
    head.truncate(read);
    Dialect::sniff(&head)
}

/// Decode raw field bytes to display text in `encoding`, dropping a leading BOM the decoder surfaces.
pub(crate) fn decode_field(bytes: &[u8], encoding: &'static Encoding) -> String {
    let (text, _, _) = encoding.decode(bytes);
    text.strip_prefix('\u{feff}').unwrap_or(&text).to_owned()
}

/// Case-insensitive substring match. `query_lower` must already be lowercased (an empty query never
/// matches, so an empty search box highlights nothing).
pub(crate) fn cell_matches(text: &str, query_lower: &str) -> bool {
    !query_lower.is_empty() && text.to_lowercase().contains(query_lower)
}

/// Open `path` behind the `ViewportSource` seam (the app is format-agnostic).
pub(crate) fn open_table(path: &Path, dialect: Dialect) -> Result<Box<dyn ViewportSource>, String> {
    CsvTable::open(path, dialect)
        .map(|t| Box::new(t) as Box<dyn ViewportSource>)
        .map_err(|e| e.to_string())
}

/// Render a delimiter byte for the custom field: a printable ASCII char as itself, anything else
/// (tab, control chars) as a `0x..` hex byte the user can read and re-enter.
pub(crate) fn delimiter_display(delimiter: u8) -> String {
    if delimiter.is_ascii_graphic() || delimiter == b' ' {
        (delimiter as char).to_string()
    } else {
        format!("0x{delimiter:02x}")
    }
}

/// A friendly name for a delimiter byte (for the "Auto · detected …" label).
pub(crate) fn delimiter_label(delimiter: u8) -> String {
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
pub(crate) fn parse_delimiter(input: &str) -> Option<u8> {
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

pub(crate) fn column_name(schema: &Schema, id: ColumnId, encoding: &'static Encoding) -> String {
    schema
        .columns
        .get(id.0 as usize)
        .map(|c| decode_field(&c.name, encoding))
        .unwrap_or_else(|| format!("col{}", id.0))
}

/// Display order + visibility of columns (app state; persists across frames).
pub(crate) struct GridLayout {
    /// All source columns in display order.
    pub(crate) order: Vec<ColumnId>,
    /// Visibility per source-column index.
    pub(crate) visible: Vec<bool>,
}

impl GridLayout {
    pub(crate) fn new(column_count: usize) -> Self {
        Self {
            order: (0..column_count as u32).map(ColumnId).collect(),
            visible: vec![true; column_count],
        }
    }

    /// Visible columns, in display order — what the grid actually renders.
    pub(crate) fn displayed(&self) -> Vec<ColumnId> {
        self.order
            .iter()
            .copied()
            .filter(|c| self.visible[c.0 as usize])
            .collect()
    }
}

/// Move `dragged` next to `target` in display order — before it, or after it when `after` is set.
/// Pure so the reorder logic is verified independently of the drag-and-drop UI.
pub(crate) fn reorder(order: &mut Vec<ColumnId>, dragged: ColumnId, target: ColumnId, after: bool) {
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
/// An inclusive range of selected display rows. `anchor` is where the selection began (a click, or
/// the start of a click-drag); `lead` is the active end (the drag position / keyboard cursor). A
/// single-row selection has `anchor == lead`.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(crate) struct RowSpan {
    pub(crate) anchor: u64,
    pub(crate) lead: u64,
}

impl RowSpan {
    pub(crate) fn single(row: u64) -> Self {
        Self {
            anchor: row,
            lead: row,
        }
    }
    pub(crate) fn lo(self) -> u64 {
        self.anchor.min(self.lead)
    }
    pub(crate) fn hi(self) -> u64 {
        self.anchor.max(self.lead)
    }
    pub(crate) fn contains(self, row: u64) -> bool {
        self.lo() <= row && row <= self.hi()
    }
    pub(crate) fn len(self) -> u64 {
        self.hi() - self.lo() + 1
    }
}

#[derive(Default)]
pub(crate) struct ViewControls {
    /// The find/filter query (also used for in-place highlight).
    pub(crate) search: String,
    /// Interpret the query as a regex.
    pub(crate) regex: bool,
    /// Show only NON-matching rows.
    pub(crate) invert: bool,
    /// Hide non-matching rows (filter) rather than only highlighting them.
    pub(crate) filter_mode: bool,
    /// Active sort, if any.
    pub(crate) sort: Option<SortKey>,
    /// The `ViewSpec` last applied to the engine (to detect changes).
    pub(crate) applied: ViewSpec,
    /// Last error from applying the view (e.g. invalid regex).
    pub(crate) error: Option<String>,
    /// Selected display rows (click, click-drag range, or arrow/page/home/end); ⌘/Ctrl+C copies them.
    pub(crate) selected: Option<RowSpan>,
    /// True while a click-drag row selection is in progress (cells extend it to the pointer).
    pub(crate) selecting: bool,
    /// Display row under the mouse (transient; drives the hover highlight).
    pub(crate) hovered_row: Option<u64>,
}

impl ViewControls {
    /// The view the engine should currently have, derived from the controls.
    pub(crate) fn desired(&self) -> ViewSpec {
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

/// A persisted per-file view: column order/visibility + sort + filter. Saved to the config dir and
/// reapplied when the same file is reopened.
#[derive(Clone, Default, PartialEq, serde::Serialize, serde::Deserialize)]
pub(crate) struct SavedView {
    order: Vec<u32>,
    pub(crate) visible: Vec<bool>,
    /// (column index, ascending?)
    sort: Option<(u32, bool)>,
    /// (query, regex, invert) — present only when a hide-non-matching filter is active.
    filter: Option<(String, bool, bool)>,
    /// Explicit delimiter override; `None` = auto-detect (the default).
    #[serde(default)]
    pub(crate) delimiter: Option<u8>,
}

impl SavedView {
    /// Snapshot the current layout + controls. `delimiter` is the explicit override (or `None` = auto).
    pub(crate) fn snapshot(
        layout: &GridLayout,
        view: &ViewControls,
        delimiter: Option<u8>,
    ) -> Self {
        Self {
            order: layout.order.iter().map(|c| c.0).collect(),
            visible: layout.visible.clone(),
            sort: view
                .sort
                .map(|s| (s.column.0, s.direction == Direction::Ascending)),
            filter: (view.filter_mode && !view.search.is_empty())
                .then(|| (view.search.clone(), view.regex, view.invert)),
            delimiter,
        }
    }

    /// Reapply onto a freshly-opened layout + controls (length-checked against the column count).
    pub(crate) fn apply(&self, layout: &mut GridLayout, view: &mut ViewControls) {
        if self.order.len() == layout.order.len() {
            layout.order = self.order.iter().map(|&c| ColumnId(c)).collect();
        }
        if self.visible.len() == layout.visible.len() {
            layout.visible = self.visible.clone();
        }
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
pub(crate) struct LoadedTable {
    pub(crate) path: PathBuf,
    pub(crate) table: Box<dyn ViewportSource>,
    pub(crate) layout: GridLayout,
    pub(crate) dialect: Dialect,
    pub(crate) encoding: &'static Encoding,
    pub(crate) view: ViewControls,
    pub(crate) saved: SavedView,
    /// Text in the Parsing menu's custom-delimiter field (a char like `:` or a hex byte like `0x01`).
    pub(crate) delimiter_input: String,
    /// The delimiter `Dialect::sniff` detected on open — what "Auto" resolves to.
    pub(crate) detected_delimiter: u8,
    /// Whether the delimiter is auto-detected (the default) vs an explicit user override.
    pub(crate) delimiter_auto: bool,
}

/// What the window is currently showing.
pub(crate) enum View {
    Empty,
    Loaded(Box<LoadedTable>),
    Failed { path: PathBuf, error: String },
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
    fn row_span_covers_the_range_regardless_of_drag_direction() {
        let down = RowSpan { anchor: 2, lead: 5 };
        assert_eq!((down.lo(), down.hi(), down.len()), (2, 5, 4));
        assert!(down.contains(2) && down.contains(4) && down.contains(5));
        assert!(!down.contains(1) && !down.contains(6));

        // Dragging upward (lead < anchor) selects the same rows.
        let up = RowSpan { anchor: 5, lead: 2 };
        assert_eq!((up.lo(), up.hi(), up.len()), (2, 5, 4));
        assert!(up.contains(3));

        let single = RowSpan::single(7);
        assert_eq!(single.len(), 1);
        assert!(single.contains(7) && !single.contains(8));
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
