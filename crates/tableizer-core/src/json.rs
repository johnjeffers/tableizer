//! A [`ViewportSource`] over JSON — both **NDJSON / JSON Lines** (one value per line) and a single
//! top-level **JSON array** (`[ {…}, {…} ]`), the two common *tabular* JSON shapes (`docs/spec.md`
//! §3.1 / §4.5 — the nested format that proves the format-reader seam).
//!
//! The two shapes differ only in how records are delimited, so the engine is identical apart from a
//! [`Records`] boundary strategy:
//! - **Lines**: a raw `\n` is always a record boundary (newlines inside JSON strings are escaped).
//! - **Array**: records are the depth-1 elements of the array, located by a quote/escape/brace-depth
//!   aware scan ([`scan_value_end`]). This keeps arrays fully **out-of-core** — the same sparse
//!   offset index, streaming search, and external sort as NDJSON, with no whole-file parse.
//!
//! Columns are the union of top-level object keys in first-seen order (a `value` column when records
//! aren't objects). Cells render to UTF-8 text: strings unquoted, numbers/booleans as their JSON
//! text, `null` as empty, nested arrays/objects as compact JSON — there is no source byte for a
//! decoded value, so byte-fidelity is a faithful *rendering* the [`Cell`] then carries unchanged
//! through search / sort / export.

use std::collections::HashMap;
use std::path::Path;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::thread;

use serde_json::Value;

use crate::search::Matcher;
use crate::source::{Source, map_file};
use crate::{
    CancellationToken, Cell, Column, ColumnId, DataQuality, Error, InferredType, Progress, Result,
    RowCount, RowRange, Schema, SortKey, ViewSpec, ViewStatus, Viewport, ViewportRequest,
    ViewportSource,
};

/// Records between stored anchors. Lookup re-scans at most this many records from an anchor.
const ANCHOR_INTERVAL: u64 = 1024;

/// Records sampled from the head to discover columns + infer presentational types.
const SCHEMA_SAMPLE_ROWS: usize = 200;

/// Which tabular-JSON shape a source is: one value per line, or a single top-level array.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum JsonMode {
    /// NDJSON / JSON Lines — records are newline-separated.
    Lines,
    /// A single top-level JSON array — records are its depth-1 elements.
    Array,
}

/// Best-effort: does `head` (a prefix of the file is enough) look like NDJSON or a JSON array this
/// reader can read? Returns the detected [`JsonMode`], or `None` for non-JSON (→ treat as delimited).
/// Shared by the app's format routing and [`JsonTable`]'s own mode selection, so they never disagree.
pub fn sniff(head: &[u8]) -> Option<JsonMode> {
    let body = &head[bom_len(head)..];
    let first = body.iter().position(|b| !b.is_ascii_whitespace())?;
    match body[first] {
        b'[' => {
            // The first non-ws byte after '[' must begin a JSON value (or close an empty array) —
            // this rejects e.g. a CSV line `[id],name` whose '[' is not really a JSON array.
            match body[first + 1..].iter().find(|b| !b.is_ascii_whitespace()) {
                Some(&c) if is_value_start(c) || c == b']' => Some(JsonMode::Array),
                None => Some(JsonMode::Array), // '[' then only whitespace in the head — assume array
                _ => None,
            }
        }
        // NDJSON only if the first *line* is a complete JSON value (a pretty-printed object's first
        // line is just `{`, which won't parse — so it is correctly not treated as line-delimited).
        b'{' => {
            let line_end = body[first..]
                .iter()
                .position(|&b| b == b'\n')
                .map_or(body.len(), |p| first + p);
            serde_json::from_slice::<Value>(&body[first..line_end])
                .ok()
                .map(|_| JsonMode::Lines)
        }
        _ => None,
    }
}

/// Whether `c` can begin a JSON value (`{`, `[`, string, number, or `true`/`false`/`null`).
fn is_value_start(c: u8) -> bool {
    matches!(c, b'{' | b'[' | b'"' | b'-') || c.is_ascii_digit() || matches!(c, b't' | b'f' | b'n')
}

/// Length of a leading UTF-8 BOM (0 or 3).
fn bom_len(bytes: &[u8]) -> usize {
    if bytes.starts_with(&[0xEF, 0xBB, 0xBF]) {
        3
    } else {
        0
    }
}

/// The record-boundary strategy for a source: its [`JsonMode`] plus the byte offset where records
/// begin (after a BOM for Lines; just past the opening `[` for Array). Cheap to copy; the readers
/// thread it through every scan so Lines and Array share one engine.
#[derive(Clone, Copy)]
struct Records {
    mode: JsonMode,
    body_start: usize,
}

impl Records {
    /// Determine the boundary strategy for `bytes` (the reader is only built once routing decided the
    /// file is JSON, so a `None` sniff defaults to Lines rather than failing).
    fn detect(bytes: &[u8]) -> Records {
        let mode = sniff(bytes).unwrap_or(JsonMode::Lines);
        let from = bom_len(bytes);
        let body_start = match mode {
            JsonMode::Lines => from,
            // Just past the opening '[' (the first non-ws byte for an Array source).
            JsonMode::Array => bytes[from..]
                .iter()
                .position(|&b| b == b'[')
                .map_or(from, |p| from + p + 1),
        };
        Records { mode, body_start }
    }

    /// The next record at or after `pos`: `(start, end, next)` where `bytes[start..end]` is the
    /// record's JSON text and `next` is where scanning resumes. `None` at end of data.
    fn next(&self, bytes: &[u8], pos: usize) -> Option<(usize, usize, usize)> {
        match self.mode {
            JsonMode::Lines => next_line_record(bytes, pos),
            JsonMode::Array => next_array_element(bytes, pos),
        }
    }
}

/// A read-only table view over a JSON file (NDJSON or a top-level array). Progressive like
/// [`crate::CsvTable`]: [`open`] returns immediately (mmap + sample the head for columns) and builds
/// the record-offset index in the background; the row count grows honestly until it lands.
///
/// [`open`]: JsonTable::open
pub struct JsonTable {
    source: Source,
    records: Records,
    schema: Schema,
    columns: Arc<Vec<ColumnKey>>,
    index: Arc<Mutex<Option<RecordIndex>>>,
    frontier: Arc<AtomicU64>,
    cancel: CancellationToken,
    view: Arc<Mutex<Option<Vec<u64>>>>,
    view_building: Arc<AtomicBool>,
    view_cancel: Mutex<CancellationToken>,
}

/// One column of a JSON table: its display name plus the object key it projects. `key` is `None` for
/// the synthetic whole-value column used when records aren't objects.
#[derive(Clone, Debug)]
struct ColumnKey {
    name: Box<[u8]>,
    key: Option<String>,
    inferred: InferredType,
}

impl JsonTable {
    /// Build a table over in-memory bytes (small files, or tests). Indexed synchronously.
    pub fn from_bytes(bytes: impl Into<Arc<[u8]>>) -> Result<Self> {
        let source = Source::Bytes(bytes.into());
        let records = Records::detect(source.bytes());
        let (schema, columns) = derive_columns(source.bytes(), records);
        let index = RecordIndex::build(source.bytes(), records)?;
        Ok(Self {
            frontier: Arc::new(AtomicU64::new(index.row_count)),
            index: Arc::new(Mutex::new(Some(index))),
            source,
            records,
            schema,
            columns: Arc::new(columns),
            cancel: CancellationToken::new(),
            view: Arc::new(Mutex::new(None)),
            view_building: Arc::new(AtomicBool::new(false)),
            view_cancel: Mutex::new(CancellationToken::new()),
        })
    }

    /// Open a file by memory-mapping it. Returns immediately (mmap + head sample); the record index
    /// builds on a background thread so the first screen is never gated on a full scan.
    pub fn open(path: &Path) -> Result<Self> {
        let source = Source::Mmap(Arc::new(map_file(path)?));
        let records = Records::detect(source.bytes());
        let (schema, columns) = derive_columns(source.bytes(), records);
        let index = Arc::new(Mutex::new(None));
        let frontier = Arc::new(AtomicU64::new(0));
        let cancel = CancellationToken::new();
        spawn_index_build(
            source.clone(),
            records,
            Arc::clone(&index),
            Arc::clone(&frontier),
            cancel.clone(),
        );
        Ok(Self {
            source,
            records,
            schema,
            columns: Arc::new(columns),
            index,
            frontier,
            cancel,
            view: Arc::new(Mutex::new(None)),
            view_building: Arc::new(AtomicBool::new(false)),
            view_cancel: Mutex::new(CancellationToken::new()),
        })
    }

    /// The detected JSON shape (NDJSON lines vs a single array) — for the status-bar label.
    pub fn mode(&self) -> JsonMode {
        self.records.mode
    }
}

impl Drop for JsonTable {
    fn drop(&mut self) {
        self.cancel.cancel();
        self.view_cancel.lock().expect("view cancel lock").cancel();
    }
}

impl ViewportSource for JsonTable {
    fn schema(&self) -> &Schema {
        &self.schema
    }

    fn row_count(&self) -> RowCount {
        if let Some(len) = self
            .view
            .lock()
            .expect("view lock")
            .as_ref()
            .map(|rows| rows.len() as u64)
        {
            return RowCount::Exact(len);
        }
        match self.index.lock().expect("index lock").as_ref() {
            Some(index) => RowCount::Exact(index.row_count),
            None => RowCount::AtLeast(self.frontier.load(Ordering::Relaxed)),
        }
    }

    fn fetch(&self, request: &ViewportRequest, cancel: &CancellationToken) -> Result<Viewport> {
        let data_rows: Vec<u64> = {
            let view = self.view.lock().expect("view lock");
            match view.as_ref() {
                Some(rows) => {
                    let start = request.rows.start as usize;
                    (start..start.saturating_add(request.rows.len as usize))
                        .filter_map(|i| rows.get(i).copied())
                        .collect()
                }
                None => contiguous_rows(request.rows),
            }
        };
        self.fetch_data_rows(&data_rows, &request.columns, request.rows.start, cancel)
    }

    fn fetch_source(
        &self,
        request: &ViewportRequest,
        cancel: &CancellationToken,
    ) -> Result<Viewport> {
        let data_rows = contiguous_rows(request.rows);
        self.fetch_data_rows(&data_rows, &request.columns, request.rows.start, cancel)
    }

    fn data_quality(&self) -> DataQuality {
        DataQuality {
            ragged_rows: 0,
            complete: self.index.lock().expect("index lock").is_some(),
        }
    }

    fn set_view(&self, spec: &ViewSpec) -> Result<()> {
        let matcher = match &spec.filter {
            Some(filter) => Some(Matcher::compile(filter)?),
            None => None,
        };
        let sort = spec.sort;

        let cancel = {
            let mut guard = self.view_cancel.lock().expect("view cancel lock");
            guard.cancel();
            *guard = CancellationToken::new();
            guard.clone()
        };

        if matcher.is_none() && sort.is_none() {
            *self.view.lock().expect("view lock") = None;
            self.view_building.store(false, Ordering::Relaxed);
            return Ok(());
        }

        self.view_building.store(true, Ordering::Relaxed);
        let source = self.source.clone();
        let records = self.records;
        let columns = Arc::clone(&self.columns);
        let view = Arc::clone(&self.view);
        let building = Arc::clone(&self.view_building);
        thread::spawn(move || {
            let built = build_view(source.bytes(), records, &columns, matcher, sort, &cancel);
            if let Ok(rows) = built
                && !cancel.is_cancelled()
            {
                *view.lock().expect("view lock") = Some(rows);
            }
            building.store(false, Ordering::Relaxed);
        });
        Ok(())
    }

    fn clear_view(&self) {
        self.view_cancel.lock().expect("view cancel lock").cancel();
        *self.view.lock().expect("view lock") = None;
        self.view_building.store(false, Ordering::Relaxed);
    }

    fn view_status(&self) -> ViewStatus {
        ViewStatus {
            building: self.view_building.load(Ordering::Relaxed),
        }
    }
}

impl JsonTable {
    /// Materialise the given data-rows into a viewport, addressing each via the record index. Falls
    /// back to head-streaming if the index isn't ready yet (parity with [`crate::CsvTable`]).
    fn fetch_data_rows(
        &self,
        data_rows: &[u64],
        columns: &[ColumnId],
        stream_start: u64,
        cancel: &CancellationToken,
    ) -> Result<Viewport> {
        let bytes = self.source.bytes();
        let guard = self.index.lock().expect("index lock");
        let Some(index) = guard.as_ref() else {
            drop(guard);
            let len = u32::try_from(data_rows.len()).unwrap_or(u32::MAX);
            return Ok(self.fetch_streaming(
                RowRange {
                    start: stream_start,
                    len,
                },
                columns,
                cancel,
            ));
        };
        let total = index.row_count;
        let mut rows = Vec::new();
        for &data_row in data_rows {
            if cancel.is_cancelled() {
                return Err(Error::Cancelled);
            }
            if data_row >= total {
                continue;
            }
            let offset = index
                .offset_of_row(bytes, self.records, data_row)
                .expect("a row below the total has an indexed offset");
            let value = record_value(bytes, self.records, offset);
            rows.push(project(value.as_ref(), columns, &self.columns));
        }
        Ok(Viewport { rows })
    }

    /// Serve `rows` by scanning forward from the head — used before the index is ready.
    fn fetch_streaming(
        &self,
        rows: RowRange,
        columns: &[ColumnId],
        cancel: &CancellationToken,
    ) -> Viewport {
        let bytes = self.source.bytes();
        let mut pos = self.records.body_start;
        let mut skipped = 0u64;
        while skipped < rows.start {
            match self.records.next(bytes, pos) {
                Some((_, _, next)) => pos = next,
                None => return Viewport::default(),
            }
            skipped += 1;
        }
        let mut out = Vec::new();
        for _ in 0..rows.len {
            if cancel.is_cancelled() {
                break;
            }
            let Some((start, end, next)) = self.records.next(bytes, pos) else {
                break;
            };
            pos = next;
            let value = serde_json::from_slice::<Value>(&bytes[start..end]).ok();
            out.push(project(value.as_ref(), columns, &self.columns));
        }
        Viewport { rows: out }
    }
}

/// The contiguous data-rows covered by a display `range` (the identity view).
fn contiguous_rows(range: RowRange) -> Vec<u64> {
    (range.start..range.start.saturating_add(u64::from(range.len))).collect()
}

/// Build the display order for a view: scan every record, keep those passing `matcher`, and (if
/// `sort` is set) order them by the rendered sort-key field. Mirrors [`crate::CsvTable`]'s in-memory
/// view build (the spill-to-disk refinement, §4.3, applies equally here).
fn build_view(
    bytes: &[u8],
    records: Records,
    columns: &[ColumnKey],
    matcher: Option<Matcher>,
    sort: Option<SortKey>,
    cancel: &CancellationToken,
) -> Result<Vec<u64>> {
    let mut rows: Vec<(Vec<u8>, u64)> = Vec::new();
    let mut pos = records.body_start;
    let mut data_row = 0u64;
    while let Some((start, end, next)) = records.next(bytes, pos) {
        pos = next;
        let row = data_row;
        data_row += 1;
        if row.is_multiple_of(4096) && cancel.is_cancelled() {
            return Err(Error::Cancelled);
        }
        let value = serde_json::from_slice::<Value>(&bytes[start..end]).ok();
        if let Some(matcher) = &matcher {
            let fields = render_columns(value.as_ref(), columns);
            if !matcher.matches_any(fields.iter().map(Vec::as_slice)) {
                continue;
            }
        }
        let key = match &sort {
            Some(sort) => render_column(value.as_ref(), columns, sort.column),
            None => Vec::new(),
        };
        rows.push((key, row));
    }
    if let Some(sort) = &sort {
        rows.sort_by(|a, b| crate::sort::compare_keys(&a.0, &b.0, sort.direction));
    }
    Ok(rows.into_iter().map(|(_, data_row)| data_row).collect())
}

/// Spawn the background record-index build, publishing the growing row count via `frontier`.
fn spawn_index_build(
    source: Source,
    records: Records,
    slot: Arc<Mutex<Option<RecordIndex>>>,
    frontier: Arc<AtomicU64>,
    cancel: CancellationToken,
) {
    thread::spawn(move || {
        let progress_frontier = Arc::clone(&frontier);
        let built = RecordIndex::build_with(source.bytes(), records, &cancel, |p| {
            progress_frontier.store(p.rows_indexed, Ordering::Relaxed);
        });
        if let Ok(index) = built {
            frontier.store(index.row_count, Ordering::Relaxed);
            *slot.lock().expect("index lock") = Some(index);
        }
    });
}

/// Parse the record beginning at `offset` (a record start from the index) into a JSON value.
fn record_value(bytes: &[u8], records: Records, offset: usize) -> Option<Value> {
    let (start, end, _next) = records.next(bytes, offset)?;
    serde_json::from_slice(&bytes[start..end]).ok()
}

/// Render every column of `value` to bytes (for the search matcher — match the displayed text).
fn render_columns(value: Option<&Value>, columns: &[ColumnKey]) -> Vec<Vec<u8>> {
    (0..columns.len())
        .map(|i| render_column(value, columns, ColumnId(i as u32)))
        .collect()
}

/// Render one column of `value` to bytes (empty if absent / not an object / parse failed).
fn render_column(value: Option<&Value>, columns: &[ColumnKey], column: ColumnId) -> Vec<u8> {
    let Some(col) = columns.get(column.0 as usize) else {
        return Vec::new();
    };
    match &col.key {
        Some(key) => match value {
            Some(Value::Object(map)) => map.get(key).map(render_value).unwrap_or_default(),
            _ => Vec::new(),
        },
        None => value.map(render_value).unwrap_or_default(),
    }
}

/// Project a parsed record onto the requested columns, in order, as rendered text cells.
fn project(value: Option<&Value>, columns: &[ColumnId], cols: &[ColumnKey]) -> Vec<Cell> {
    columns
        .iter()
        .map(|&c| Cell(render_column(value, cols, c).into_boxed_slice()))
        .collect()
}

/// Render a single JSON value to display bytes: strings unquoted, numbers/bools as their JSON text,
/// `null` as empty (so the grid shows its null placeholder), arrays/objects as compact JSON.
fn render_value(value: &Value) -> Vec<u8> {
    match value {
        Value::Null => Vec::new(),
        Value::Bool(true) => b"true".to_vec(),
        Value::Bool(false) => b"false".to_vec(),
        Value::Number(n) => n.to_string().into_bytes(),
        Value::String(s) => s.clone().into_bytes(),
        other => serde_json::to_vec(other).unwrap_or_default(),
    }
}

/// Accumulates the presentational type of one column across the sampled records (mirrors the CSV
/// type inference: a type holds only if every non-null sampled value fits; nulls never disqualify).
#[derive(Clone, Copy)]
struct TypeAcc {
    any: bool,
    all_int: bool,
    all_float: bool,
    all_bool: bool,
}

impl TypeAcc {
    fn new() -> Self {
        Self {
            any: false,
            all_int: true,
            all_float: true,
            all_bool: true,
        }
    }

    fn observe(&mut self, value: &Value) {
        match value {
            Value::Null => {} // null = absent; doesn't disqualify any type
            Value::Number(n) => {
                self.any = true;
                self.all_bool = false;
                self.all_int &= n.is_i64() || n.is_u64();
            }
            Value::Bool(_) => {
                self.any = true;
                self.all_int = false;
                self.all_float = false;
            }
            _ => {
                self.any = true;
                self.all_int = false;
                self.all_float = false;
                self.all_bool = false;
            }
        }
    }

    fn resolve(self) -> InferredType {
        if !self.any {
            InferredType::Text
        } else if self.all_int {
            InferredType::Integer
        } else if self.all_float {
            InferredType::Float
        } else if self.all_bool {
            InferredType::Boolean
        } else {
            InferredType::Text
        }
    }
}

/// Discover columns by sampling the first [`SCHEMA_SAMPLE_ROWS`] records: the union of top-level
/// object keys in first-seen order, each with an inferred presentational type. Falls back to one
/// synthetic `value` column when no sampled record is an object.
fn derive_columns(bytes: &[u8], records: Records) -> (Schema, Vec<ColumnKey>) {
    let mut keys: Vec<String> = Vec::new();
    let mut index_of: HashMap<String, usize> = HashMap::new();
    let mut types: Vec<TypeAcc> = Vec::new();
    let mut saw_object = false;

    let mut pos = records.body_start;
    let mut sampled = 0usize;
    while sampled < SCHEMA_SAMPLE_ROWS {
        let Some((start, end, next)) = records.next(bytes, pos) else {
            break;
        };
        pos = next;
        sampled += 1;
        if let Ok(Value::Object(map)) = serde_json::from_slice::<Value>(&bytes[start..end]) {
            saw_object = true;
            for (k, v) in &map {
                let idx = *index_of.entry(k.clone()).or_insert_with(|| {
                    keys.push(k.clone());
                    types.push(TypeAcc::new());
                    keys.len() - 1
                });
                types[idx].observe(v);
            }
        }
    }

    let columns: Vec<ColumnKey> = if saw_object {
        keys.into_iter()
            .zip(types)
            .map(|(k, acc)| ColumnKey {
                name: k.clone().into_bytes().into_boxed_slice(),
                key: Some(k),
                inferred: acc.resolve(),
            })
            .collect()
    } else {
        vec![ColumnKey {
            name: b"value".to_vec().into_boxed_slice(),
            key: None,
            inferred: InferredType::Text,
        }]
    };

    let schema = Schema {
        columns: columns
            .iter()
            .enumerate()
            .map(|(i, c)| Column {
                id: ColumnId(i as u32),
                name: c.name.clone(),
                inferred: c.inferred,
            })
            .collect(),
    };
    (schema, columns)
}

/// Find the next non-blank line at or after `pos` (NDJSON record boundary). Returns `(start, end,
/// next)` where `bytes[start..end]` is the line sans terminator/`\r`. Blank lines are skipped.
fn next_line_record(bytes: &[u8], mut pos: usize) -> Option<(usize, usize, usize)> {
    let len = bytes.len();
    while pos < len {
        let (line_end, next) = match memchr::memchr(b'\n', &bytes[pos..]) {
            Some(i) => (pos + i, pos + i + 1),
            None => (len, len),
        };
        let mut end = line_end;
        if end > pos && bytes[end - 1] == b'\r' {
            end -= 1;
        }
        if bytes[pos..end].iter().all(u8::is_ascii_whitespace) {
            pos = next;
            continue;
        }
        return Some((pos, end, next));
    }
    None
}

/// Find the next depth-1 array element at or after `pos`. `pos` sits at an element boundary (just
/// past the opening `[` or a previous element's end); leading whitespace and the separating `,` are
/// skipped, `]` ends the array. Returns `(start, end, next)` with `bytes[start..end]` the element's
/// JSON text. Never treats an element-opening `{`/`[` as a separator — only `,`/whitespace are.
fn next_array_element(bytes: &[u8], mut pos: usize) -> Option<(usize, usize, usize)> {
    let len = bytes.len();
    while pos < len {
        match bytes[pos] {
            b']' => return None,
            c if c.is_ascii_whitespace() || c == b',' => pos += 1,
            _ => {
                let end = scan_value_end(bytes, pos);
                return Some((pos, end, end));
            }
        }
    }
    None
}

/// Byte index just past the JSON value beginning at `start` (its first non-ws char). Quote/escape/
/// brace-depth aware, so delimiters inside strings or nested containers are not mistaken for the
/// value's end. On malformed/truncated input it stops at EOF rather than panicking.
fn scan_value_end(bytes: &[u8], start: usize) -> usize {
    let len = bytes.len();
    match bytes.get(start) {
        Some(b'"') => scan_string_end(bytes, start),
        Some(b'{') | Some(b'[') => {
            let mut depth = 0usize;
            let mut in_string = false;
            let mut escaped = false;
            let mut i = start;
            while i < len {
                let c = bytes[i];
                if in_string {
                    if escaped {
                        escaped = false;
                    } else if c == b'\\' {
                        escaped = true;
                    } else if c == b'"' {
                        in_string = false;
                    }
                } else {
                    match c {
                        b'"' => in_string = true,
                        b'{' | b'[' => depth += 1,
                        b'}' | b']' => {
                            depth = depth.saturating_sub(1);
                            if depth == 0 {
                                return i + 1;
                            }
                        }
                        _ => {}
                    }
                }
                i += 1;
            }
            len
        }
        // Primitive (number / true / false / null): up to the next structural delimiter or whitespace.
        _ => {
            let mut i = start;
            while i < len
                && !matches!(bytes[i], b',' | b']' | b'}')
                && !bytes[i].is_ascii_whitespace()
            {
                i += 1;
            }
            i
        }
    }
}

/// Byte index just past the JSON string beginning at `start` (where `bytes[start] == b'"'`).
fn scan_string_end(bytes: &[u8], start: usize) -> usize {
    let len = bytes.len();
    let mut i = start + 1;
    let mut escaped = false;
    while i < len {
        let c = bytes[i];
        if escaped {
            escaped = false;
        } else if c == b'\\' {
            escaped = true;
        } else if c == b'"' {
            return i + 1;
        }
        i += 1;
    }
    len
}

/// A sparse record → byte-offset index over either NDJSON lines or JSON-array elements (the
/// [`Records`] strategy decides which), analogous to [`crate::index::OffsetIndex`].
struct RecordIndex {
    anchors: Vec<u64>,
    row_count: u64,
}

impl RecordIndex {
    fn build(bytes: &[u8], records: Records) -> Result<Self> {
        Self::build_with(bytes, records, &CancellationToken::new(), |_| {})
    }

    fn build_with(
        bytes: &[u8],
        records: Records,
        cancel: &CancellationToken,
        mut progress: impl FnMut(Progress),
    ) -> Result<Self> {
        let mut anchors = Vec::new();
        let mut row_count: u64 = 0;
        let mut pos = records.body_start;
        while let Some((start, _end, next)) = records.next(bytes, pos) {
            if row_count.is_multiple_of(ANCHOR_INTERVAL) {
                anchors.push(start as u64);
            }
            row_count += 1;
            pos = next;
            if row_count.is_multiple_of(4096) {
                if cancel.is_cancelled() {
                    return Err(Error::Cancelled);
                }
                progress(Progress {
                    bytes_processed: next as u64,
                    rows_indexed: row_count,
                });
            }
        }
        if cancel.is_cancelled() {
            return Err(Error::Cancelled);
        }
        progress(Progress {
            bytes_processed: bytes.len() as u64,
            rows_indexed: row_count,
        });
        Ok(Self { anchors, row_count })
    }

    /// Byte offset where record `row` begins, or `None` if out of range. Seeks to the nearest
    /// preceding anchor and re-scans forward at most `ANCHOR_INTERVAL` records.
    fn offset_of_row(&self, bytes: &[u8], records: Records, row: u64) -> Option<usize> {
        if row >= self.row_count {
            return None;
        }
        let mut pos = self.anchors[(row / ANCHOR_INTERVAL) as usize] as usize;
        let skip = row % ANCHOR_INTERVAL;
        let mut i = 0u64;
        loop {
            let (start, _end, next) = records.next(bytes, pos)?;
            if i == skip {
                return Some(start);
            }
            i += 1;
            pos = next;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{Direction, FilterSpec};
    use std::time::Duration;

    fn request(start: u64, len: u32, columns: &[u32]) -> ViewportRequest {
        ViewportRequest {
            rows: RowRange { start, len },
            columns: columns.iter().copied().map(ColumnId).collect(),
        }
    }

    fn cells(viewport: &Viewport) -> Vec<Vec<String>> {
        viewport
            .rows
            .iter()
            .map(|row| {
                row.iter()
                    .map(|c| String::from_utf8_lossy(&c.0).into_owned())
                    .collect()
            })
            .collect()
    }

    fn col_names(table: &JsonTable) -> Vec<String> {
        table
            .schema()
            .columns
            .iter()
            .map(|c| String::from_utf8_lossy(&c.name).into_owned())
            .collect()
    }

    fn await_indexed(table: &JsonTable) {
        for _ in 0..2000 {
            if matches!(table.row_count(), RowCount::Exact(_)) {
                return;
            }
            thread::sleep(Duration::from_millis(1));
        }
        panic!("index did not complete in time");
    }

    fn await_view(table: &JsonTable) {
        for _ in 0..2000 {
            if !table.view_status().building {
                return;
            }
            thread::sleep(Duration::from_millis(1));
        }
        panic!("view did not build in time");
    }

    // ---- NDJSON (lines mode) ----

    #[test]
    fn ndjson_columns_are_union_of_keys_in_first_seen_order() {
        let data = b"{\"name\":\"bob\",\"age\":30}\n{\"name\":\"ann\",\"city\":\"rome\"}\n";
        let table = JsonTable::from_bytes(data.to_vec()).unwrap();
        assert_eq!(table.mode(), JsonMode::Lines);
        assert_eq!(col_names(&table), vec!["name", "age", "city"]);
        assert!(matches!(table.row_count(), RowCount::Exact(2)));
    }

    #[test]
    fn ndjson_projects_fields_with_missing_rendered_empty() {
        let data = b"{\"name\":\"bob\",\"age\":30}\n{\"name\":\"ann\",\"city\":\"rome\"}\n";
        let table = JsonTable::from_bytes(data.to_vec()).unwrap();
        let viewport = table
            .fetch(&request(0, 2, &[0, 1, 2]), &CancellationToken::new())
            .unwrap();
        assert_eq!(
            cells(&viewport),
            vec![
                vec!["bob".to_string(), "30".to_string(), String::new()],
                vec!["ann".to_string(), String::new(), "rome".to_string()],
            ]
        );
    }

    #[test]
    fn renders_null_as_empty_and_nested_as_compact_json() {
        let data = b"{\"a\":null,\"b\":[1,2],\"c\":{\"x\":1}}\n";
        let table = JsonTable::from_bytes(data.to_vec()).unwrap();
        let viewport = table
            .fetch(&request(0, 1, &[0, 1, 2]), &CancellationToken::new())
            .unwrap();
        assert_eq!(
            cells(&viewport),
            vec![vec![
                String::new(),
                "[1,2]".to_string(),
                "{\"x\":1}".to_string()
            ]]
        );
    }

    #[test]
    fn ndjson_skips_blank_lines() {
        let data = b"{\"a\":1}\n\n   \n{\"a\":2}\n";
        let table = JsonTable::from_bytes(data.to_vec()).unwrap();
        assert!(matches!(table.row_count(), RowCount::Exact(2)));
        let viewport = table
            .fetch(&request(0, 2, &[0]), &CancellationToken::new())
            .unwrap();
        assert_eq!(
            cells(&viewport),
            vec![vec!["1".to_string()], vec!["2".to_string()]]
        );
    }

    #[test]
    fn ndjson_resolves_offsets_across_many_anchors() {
        let mut data = Vec::new();
        for i in 0..3000u32 {
            data.extend_from_slice(format!("{{\"i\":{i}}}\n").as_bytes());
        }
        let table = JsonTable::from_bytes(data).unwrap();
        assert!(matches!(table.row_count(), RowCount::Exact(3000)));
        for row in [0u64, 1023, 1024, 1025, 2999] {
            let viewport = table
                .fetch(&request(row, 1, &[0]), &CancellationToken::new())
                .unwrap();
            assert_eq!(cells(&viewport)[0][0], row.to_string());
        }
    }

    // ---- JSON array (array mode) ----

    #[test]
    fn array_of_objects_reads_like_a_table() {
        let data = b"[{\"name\":\"bob\",\"age\":30},{\"name\":\"ann\",\"age\":25}]";
        let table = JsonTable::from_bytes(data.to_vec()).unwrap();
        assert_eq!(table.mode(), JsonMode::Array);
        assert_eq!(col_names(&table), vec!["name", "age"]);
        assert!(matches!(table.row_count(), RowCount::Exact(2)));
        let viewport = table
            .fetch(&request(0, 2, &[0, 1]), &CancellationToken::new())
            .unwrap();
        assert_eq!(
            cells(&viewport),
            vec![
                vec!["bob".to_string(), "30".to_string()],
                vec!["ann".to_string(), "25".to_string()],
            ]
        );
    }

    #[test]
    fn pretty_printed_array_with_whitespace_and_newlines() {
        let data = b"[\n  { \"a\": 1 },\n  { \"a\": 2 },\n  { \"a\": 3 }\n]\n";
        let table = JsonTable::from_bytes(data.to_vec()).unwrap();
        assert_eq!(table.mode(), JsonMode::Array);
        assert!(matches!(table.row_count(), RowCount::Exact(3)));
        let viewport = table
            .fetch(&request(0, 3, &[0]), &CancellationToken::new())
            .unwrap();
        assert_eq!(
            cells(&viewport),
            vec![
                vec!["1".to_string()],
                vec!["2".to_string()],
                vec!["3".to_string()],
            ]
        );
    }

    #[test]
    fn array_elements_with_strings_containing_delimiters() {
        // Strings hold ']' ',' '{' and an escaped quote — boundaries must not be fooled.
        let data = br#"[{"s":"a,b]c{"},{"s":"x\"y"}]"#;
        let table = JsonTable::from_bytes(data.to_vec()).unwrap();
        assert!(matches!(table.row_count(), RowCount::Exact(2)));
        let viewport = table
            .fetch(&request(0, 2, &[0]), &CancellationToken::new())
            .unwrap();
        assert_eq!(
            cells(&viewport),
            vec![vec!["a,b]c{".to_string()], vec!["x\"y".to_string()],]
        );
    }

    #[test]
    fn array_of_nested_values() {
        let data = br#"[{"a":[1,2],"b":{"x":1}},{"a":[3],"b":{"y":2}}]"#;
        let table = JsonTable::from_bytes(data.to_vec()).unwrap();
        let viewport = table
            .fetch(&request(0, 2, &[0, 1]), &CancellationToken::new())
            .unwrap();
        assert_eq!(
            cells(&viewport),
            vec![
                vec!["[1,2]".to_string(), "{\"x\":1}".to_string()],
                vec!["[3]".to_string(), "{\"y\":2}".to_string()],
            ]
        );
    }

    #[test]
    fn array_of_scalars_uses_value_column() {
        let data = b"[1, 2, 3]";
        let table = JsonTable::from_bytes(data.to_vec()).unwrap();
        assert_eq!(col_names(&table), vec!["value"]);
        let viewport = table
            .fetch(&request(0, 3, &[0]), &CancellationToken::new())
            .unwrap();
        assert_eq!(
            cells(&viewport),
            vec![
                vec!["1".to_string()],
                vec!["2".to_string()],
                vec!["3".to_string()],
            ]
        );
    }

    #[test]
    fn global_filter_hides_non_matching_rows() {
        let data = b"{\"name\":\"apple\"}\n{\"name\":\"banana\"}\n{\"name\":\"avocado\"}\n";
        let table = JsonTable::from_bytes(data.to_vec()).unwrap();
        table
            .set_view(&ViewSpec {
                filter: Some(FilterSpec {
                    query: "av".into(),
                    regex: false,
                    invert: false,
                    case_sensitive: false,
                }),
                sort: None,
            })
            .unwrap();
        await_view(&table);
        assert!(matches!(table.row_count(), RowCount::Exact(1)));
        let viewport = table
            .fetch(&request(0, 10, &[0]), &CancellationToken::new())
            .unwrap();
        assert_eq!(cells(&viewport), vec![vec!["avocado".to_string()]]);
    }

    #[test]
    fn array_filter_and_sort() {
        let data = b"[{\"n\":3},{\"n\":1},{\"n\":2}]";
        let table = JsonTable::from_bytes(data.to_vec()).unwrap();
        table
            .set_view(&ViewSpec {
                filter: None,
                sort: Some(SortKey {
                    column: ColumnId(0),
                    direction: Direction::Ascending,
                }),
            })
            .unwrap();
        await_view(&table);
        let viewport = table
            .fetch(&request(0, 3, &[0]), &CancellationToken::new())
            .unwrap();
        assert_eq!(
            cells(&viewport),
            vec![
                vec!["1".to_string()],
                vec!["2".to_string()],
                vec!["3".to_string()],
            ]
        );
    }

    #[test]
    fn array_resolves_offsets_across_many_anchors() {
        let mut data = Vec::from(&b"["[..]);
        for i in 0..3000u32 {
            if i > 0 {
                data.push(b',');
            }
            data.extend_from_slice(format!("{{\"i\":{i}}}").as_bytes());
        }
        data.push(b']');
        let table = JsonTable::from_bytes(data).unwrap();
        assert!(matches!(table.row_count(), RowCount::Exact(3000)));
        for row in [0u64, 1023, 1024, 1025, 2999] {
            let viewport = table
                .fetch(&request(row, 1, &[0]), &CancellationToken::new())
                .unwrap();
            assert_eq!(cells(&viewport)[0][0], row.to_string());
        }
    }

    #[test]
    fn opens_an_array_file_via_mmap() {
        use std::io::Write;
        let mut file = tempfile::NamedTempFile::new().unwrap();
        file.write_all(b"[{\"a\":1,\"b\":2},{\"a\":3,\"b\":4}]")
            .unwrap();
        let table = JsonTable::open(file.path()).unwrap();
        await_indexed(&table);
        assert_eq!(table.mode(), JsonMode::Array);
        let viewport = table
            .fetch(&request(0, 2, &[0, 1]), &CancellationToken::new())
            .unwrap();
        assert_eq!(
            cells(&viewport),
            vec![
                vec!["1".to_string(), "2".to_string()],
                vec!["3".to_string(), "4".to_string()],
            ]
        );
    }

    // ---- sniff ----

    #[test]
    fn sniff_classifies_shapes() {
        assert_eq!(sniff(b"{\"a\":1}\n{\"a\":2}\n"), Some(JsonMode::Lines));
        assert_eq!(sniff(b"  [ {\"a\":1} ]"), Some(JsonMode::Array));
        assert_eq!(sniff(b"[1,2,3]"), Some(JsonMode::Array));
        assert_eq!(sniff(b"[]"), Some(JsonMode::Array));
        // BOM is tolerated.
        assert_eq!(sniff(b"\xEF\xBB\xBF[{\"a\":1}]"), Some(JsonMode::Array));
        // Not JSON: CSV, or a '[' that doesn't begin a JSON value.
        assert_eq!(sniff(b"name,age\nbob,30\n"), None);
        assert_eq!(sniff(b"[id],name\n[1],bob\n"), None);
        // A pretty-printed single object: first line is just '{', not a complete value.
        assert_eq!(sniff(b"{\n  \"a\": 1\n}\n"), None);
    }
}
