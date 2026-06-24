//! The app's data and per-file UI state: column layout, row selection, the find/sort/filter
//! controls, and the loaded-table aggregate, plus the small pure helpers (dialect sniffing, field
//! decoding, delimiter parsing) shared across the UI. The pure helpers are unit-tested below.

use std::io::Read;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use encoding_rs::Encoding;
use tableizer_core::search::Matcher;
use tableizer_core::{
    ColumnId, CsvTable, Direction, FilterSpec, JsonTable, ParquetTable, Schema, SortKey, ViewSpec,
    ViewportSource, parse::Dialect,
};

/// A table behind the [`ViewportSource`] seam, shared (`Arc`) so a background export thread can hold
/// its own handle while the UI keeps rendering from the same engine. `Send + Sync` because the engine
/// readers already are (their state is `Arc`/`Mutex`/atomics).
pub(crate) type SharedTable = Arc<dyn ViewportSource + Send + Sync>;

/// The file format behind a [`ViewportSource`]. Detected on open; selects which engine reader to
/// build and which UI affordances apply (only the delimited format exposes the Parsing tab).
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(crate) enum Format {
    /// CSV / TSV / arbitrary-separator text (the dialect-driven path).
    Delimited,
    /// JSON — NDJSON / JSON Lines, or a single top-level JSON array.
    Json,
    /// Apache Parquet.
    Parquet,
}

/// Detect a file's format from its **content**, not just its extension — extensions lie (a `.json`
/// holding NDJSON, or a mis-named Parquet). Order: Parquet's `PAR1` magic, then a JSON shape sniff
/// (NDJSON or a top-level array), then the extension as a fallback for content that didn't classify
/// (e.g. an empty file), else delimited text.
pub(crate) fn detect_format(path: &Path) -> Format {
    let head = read_head(path, 64 * 1024);
    if head.starts_with(b"PAR1") {
        return Format::Parquet;
    }
    if tableizer_core::json::sniff(&head).is_some() {
        return Format::Json;
    }
    match path
        .extension()
        .and_then(|e| e.to_str())
        .map(str::to_ascii_lowercase)
        .as_deref()
    {
        Some("parquet" | "pqt") => Format::Parquet,
        Some("ndjson" | "jsonl") => Format::Json,
        _ => Format::Delimited,
    }
}

/// Read up to `max` leading bytes of `path` (best-effort; a read failure yields an empty buffer).
fn read_head(path: &Path, max: usize) -> Vec<u8> {
    let mut head = vec![0u8; max];
    let read = std::fs::File::open(path)
        .and_then(|mut f| f.read(&mut head))
        .unwrap_or(0);
    head.truncate(read);
    head
}

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

/// Compile the in-place highlight matcher from the find controls, or `None` when there's nothing to
/// highlight (empty query, or an invalid regex while the user is still typing it). It mirrors the
/// filter exactly — same regex/literal and case rules — except it never inverts: highlight marks the
/// cells that *match*, regardless of "Invert search". Returns a [`Matcher`] tested per cell on the
/// raw bytes (the same bytes the filter matches), so highlight and filter never disagree.
pub(crate) fn highlight_matcher(view: &ViewControls) -> Option<Matcher> {
    if view.search.is_empty() {
        return None;
    }
    Matcher::compile(&FilterSpec {
        query: view.search.clone(),
        regex: view.regex,
        invert: false,
        case_sensitive: view.case_sensitive,
    })
    .ok()
}

/// Open `path` behind the `ViewportSource` seam (the rest of the app is format-agnostic). The
/// `dialect` is consulted only for [`Format::Delimited`]; the other readers carry their own schema.
pub(crate) fn open_table(
    path: &Path,
    format: Format,
    dialect: Dialect,
) -> Result<SharedTable, String> {
    match format {
        Format::Delimited => CsvTable::open(path, dialect)
            .map(|t| Arc::new(t) as SharedTable)
            .map_err(|e| e.to_string()),
        Format::Json => JsonTable::open(path)
            .map(|t| Arc::new(t) as SharedTable)
            .map_err(|e| e.to_string()),
        Format::Parquet => ParquetTable::open(path)
            .map(|t| Arc::new(t) as SharedTable)
            .map_err(|e| e.to_string()),
    }
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

/// A human-readable name for the loaded file's format (for the status bar). A delimited file is
/// named by its delimiter — comma → CSV, tab → TSV, anything else → "Delimited · <delimiter>".
pub(crate) fn format_label(format: Format, dialect: &Dialect) -> String {
    match format {
        Format::Parquet => "Parquet".to_string(),
        Format::Json => "JSON".to_string(),
        Format::Delimited => match dialect.delimiter {
            b',' => "CSV".to_string(),
            b'\t' => "TSV".to_string(),
            other => format!("Delimited · {}", delimiter_label(other)),
        },
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
    /// Match the query case-sensitively (default: case-insensitive).
    pub(crate) case_sensitive: bool,
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
                case_sensitive: self.case_sensitive,
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
    /// Whether find matches case-sensitively. Defaulted so views saved before this field load.
    #[serde(default)]
    case_sensitive: bool,
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
            case_sensitive: view.case_sensitive,
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
        view.case_sensitive = self.case_sensitive;
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
    pub(crate) table: SharedTable,
    pub(crate) layout: GridLayout,
    /// The detected file format. Gates the delimited-only UI (the Parsing tab).
    pub(crate) format: Format,
    pub(crate) dialect: Dialect,
    pub(crate) encoding: &'static Encoding,
    pub(crate) view: ViewControls,
    pub(crate) saved: SavedView,
    /// Text in the Parsing tab's custom-delimiter field (a char like `:` or a hex byte like `0x01`).
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

    /// Write `content` to a temp file with the given extension and detect its format.
    fn detect_with(extension: &str, content: &[u8]) -> Format {
        let suffix = format!(".{extension}");
        let mut file = tempfile::Builder::new().suffix(&suffix).tempfile().unwrap();
        std::io::Write::write_all(&mut file, content).unwrap();
        detect_format(file.path())
    }

    #[test]
    fn detect_format_falls_back_to_extension_for_empty_content() {
        // With no content to sniff, the extension decides (these files are empty on disk).
        assert_eq!(detect_with("parquet", b""), Format::Parquet);
        assert_eq!(detect_with("PQT", b""), Format::Parquet);
        assert_eq!(detect_with("ndjson", b""), Format::Json);
        assert_eq!(detect_with("JSONL", b""), Format::Json);
        assert_eq!(detect_with("csv", b""), Format::Delimited);
        assert_eq!(detect_with("unknown_ext_zz", b""), Format::Delimited);
    }

    #[test]
    fn detect_format_routes_json_by_content_regardless_of_extension() {
        // The reported bug: NDJSON content in a `.json` file must be JSON, not CSV.
        assert_eq!(detect_with("json", b"{\"a\":1}\n{\"a\":2}\n"), Format::Json);
        // A top-level array in a `.json` file is JSON too.
        assert_eq!(detect_with("json", b"[{\"a\":1},{\"a\":2}]"), Format::Json);
        // Content wins over a misleading extension (NDJSON named `.csv`).
        assert_eq!(detect_with("csv", b"{\"a\":1}\n{\"a\":2}\n"), Format::Json);
        // Real delimited content stays delimited even with a `.json` extension.
        assert_eq!(
            detect_with("json", b"name,age\nbob,30\n"),
            Format::Delimited
        );
        // A `[`-leading CSV is not mistaken for a JSON array.
        assert_eq!(
            detect_with("csv", b"[id],name\n[1],bob\n"),
            Format::Delimited
        );
    }

    #[test]
    fn format_label_names_each_format() {
        let comma = Dialect::default();
        assert_eq!(format_label(Format::Parquet, &comma), "Parquet");
        assert_eq!(format_label(Format::Json, &comma), "JSON");
        // Delimited is named by its delimiter.
        assert_eq!(format_label(Format::Delimited, &comma), "CSV");
        let tab = Dialect {
            delimiter: b'\t',
            ..Dialect::default()
        };
        assert_eq!(format_label(Format::Delimited, &tab), "TSV");
        let semi = Dialect {
            delimiter: b';',
            ..Dialect::default()
        };
        assert_eq!(
            format_label(Format::Delimited, &semi),
            "Delimited · semicolon"
        );
    }

    #[test]
    fn detect_format_falls_back_to_parquet_magic_bytes() {
        // A Parquet file mis-named without a known extension is still recognised by its `PAR1` magic.
        let mut file = tempfile::NamedTempFile::with_suffix(".bin").unwrap();
        std::io::Write::write_all(&mut file, b"PAR1\x00\x00datatrailer").unwrap();
        assert_eq!(detect_format(file.path()), Format::Parquet);

        // A non-Parquet file with an unknown extension reads as delimited.
        let mut other = tempfile::NamedTempFile::with_suffix(".bin").unwrap();
        std::io::Write::write_all(&mut other, b"a,b,c\n1,2,3\n").unwrap();
        assert_eq!(detect_format(other.path()), Format::Delimited);
    }

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

    /// A `ViewControls` with just the find fields set, for the highlight-matcher tests.
    fn find(search: &str, regex: bool, case_sensitive: bool) -> ViewControls {
        ViewControls {
            search: search.to_owned(),
            regex,
            case_sensitive,
            ..ViewControls::default()
        }
    }

    fn highlights(view: &ViewControls, cell: &str) -> bool {
        highlight_matcher(view).is_some_and(|m| m.matches_any([cell.as_bytes()]))
    }

    #[test]
    fn highlight_matcher_handles_case_and_empty_query() {
        assert!(highlights(&find("world", false, false), "Hello World")); // case-insensitive default
        assert!(!highlights(&find("xyz", false, false), "Hello"));
        assert!(highlight_matcher(&find("", false, false)).is_none()); // empty query → no highlight
        // Case-sensitive: compared as-is.
        assert!(highlights(&find("World", false, true), "Hello World"));
        assert!(!highlights(&find("world", false, true), "Hello World"));
    }

    #[test]
    fn highlight_matcher_respects_regex_mode() {
        // In regex mode the highlight uses the pattern, not a literal substring of the pattern text.
        let view = find(r"^\d{3}$", true, false);
        assert!(highlights(&view, "123"));
        assert!(!highlights(&view, "12"));
        // An invalid regex (mid-typing) compiles to nothing rather than erroring.
        assert!(highlight_matcher(&find("(", true, false)).is_none());
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
