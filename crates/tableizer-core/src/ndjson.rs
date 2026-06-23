//! A [`ViewportSource`] over an NDJSON / JSON Lines file (`docs/spec.md` §3.1 / §4.5 — the nested
//! format that proves the format-reader seam).
//!
//! Each line is one JSON value (almost always an object); a raw `\n` is therefore always a record
//! boundary (newlines inside JSON strings are escaped), so indexing is a plain newline scan — no
//! quote-state to track. Columns are the **union of top-level object keys**, in first-seen order.
//! When no line is an object (e.g. one JSON scalar/array per line) the table falls back to a single
//! `value` column holding the whole rendered line.
//!
//! Cells are rendered to UTF-8 **text** at this boundary: strings unquoted, numbers/booleans as their
//! JSON text, `null` as empty, nested arrays/objects as compact JSON. There is no source-byte for a
//! decoded value, so byte-fidelity is necessarily a faithful *rendering* here, not a passthrough
//! (the [`Cell`] still carries those exact rendered bytes through search/sort/export unchanged).

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

/// One column of an NDJSON table: its display name plus the object key it projects. `key` is `None`
/// for the synthetic whole-value column used when the file is not line-of-objects.
#[derive(Clone, Debug)]
struct ColumnKey {
    name: Box<[u8]>,
    /// Top-level object key to project, or `None` to render the whole record value.
    key: Option<String>,
    inferred: InferredType,
}

/// A read-only table view over an NDJSON file. Progressive like [`crate::CsvTable`]: [`open`] returns
/// immediately (mmap + sample the head for columns) and builds the line-offset index in the
/// background; the row count grows honestly until it lands.
///
/// [`open`]: NdjsonTable::open
pub struct NdjsonTable {
    source: Source,
    schema: Schema,
    columns: Arc<Vec<ColumnKey>>,
    index: Arc<Mutex<Option<LineIndex>>>,
    frontier: Arc<AtomicU64>,
    cancel: CancellationToken,
    view: Arc<Mutex<Option<Vec<u64>>>>,
    view_building: Arc<AtomicBool>,
    view_cancel: Mutex<CancellationToken>,
}

impl NdjsonTable {
    /// Build a table over in-memory bytes (small files, or tests). Indexed synchronously.
    pub fn from_bytes(bytes: impl Into<Arc<[u8]>>) -> Result<Self> {
        let source = Source::Bytes(bytes.into());
        let (schema, columns) = derive_columns(source.bytes());
        let index = LineIndex::build(source.bytes())?;
        Ok(Self {
            frontier: Arc::new(AtomicU64::new(index.row_count)),
            index: Arc::new(Mutex::new(Some(index))),
            source,
            schema,
            columns: Arc::new(columns),
            cancel: CancellationToken::new(),
            view: Arc::new(Mutex::new(None)),
            view_building: Arc::new(AtomicBool::new(false)),
            view_cancel: Mutex::new(CancellationToken::new()),
        })
    }

    /// Open a file by memory-mapping it. Returns immediately (mmap + head sample); the line index
    /// builds on a background thread so the first screen is never gated on a full scan.
    pub fn open(path: &Path) -> Result<Self> {
        let source = Source::Mmap(Arc::new(map_file(path)?));
        let (schema, columns) = derive_columns(source.bytes());
        let index = Arc::new(Mutex::new(None));
        let frontier = Arc::new(AtomicU64::new(0));
        let cancel = CancellationToken::new();
        spawn_index_build(
            source.clone(),
            Arc::clone(&index),
            Arc::clone(&frontier),
            cancel.clone(),
        );
        Ok(Self {
            source,
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
}

impl Drop for NdjsonTable {
    fn drop(&mut self) {
        self.cancel.cancel();
        self.view_cancel.lock().expect("view cancel lock").cancel();
    }
}

impl ViewportSource for NdjsonTable {
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
        let columns = Arc::clone(&self.columns);
        let view = Arc::clone(&self.view);
        let building = Arc::clone(&self.view_building);
        thread::spawn(move || {
            let built = build_view(source.bytes(), &columns, matcher, sort, &cancel);
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

impl NdjsonTable {
    /// Materialise the given data-rows into a viewport, addressing each via the line index. Falls
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
                .offset_of_row(bytes, data_row)
                .expect("a row below the total has an indexed offset");
            let value = record_value(bytes, offset);
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
        let mut pos = 0usize;
        let mut skipped = 0u64;
        while skipped < rows.start {
            match next_record(bytes, pos) {
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
            let Some((start, end, next)) = next_record(bytes, pos) else {
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
    columns: &[ColumnKey],
    matcher: Option<Matcher>,
    sort: Option<SortKey>,
    cancel: &CancellationToken,
) -> Result<Vec<u64>> {
    let mut rows: Vec<(Vec<u8>, u64)> = Vec::new();
    let mut pos = 0usize;
    let mut data_row = 0u64;
    while let Some((start, end, next)) = next_record(bytes, pos) {
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

/// Spawn the background line-index build, publishing the growing row count via `frontier`.
fn spawn_index_build(
    source: Source,
    slot: Arc<Mutex<Option<LineIndex>>>,
    frontier: Arc<AtomicU64>,
    cancel: CancellationToken,
) {
    thread::spawn(move || {
        let progress_frontier = Arc::clone(&frontier);
        let built = LineIndex::build_with(source.bytes(), &cancel, |p| {
            progress_frontier.store(p.rows_indexed, Ordering::Relaxed);
        });
        if let Ok(index) = built {
            frontier.store(index.row_count, Ordering::Relaxed);
            *slot.lock().expect("index lock") = Some(index);
        }
    });
}

/// Parse the record beginning at `offset` (a record start from the index) into a JSON value.
fn record_value(bytes: &[u8], offset: usize) -> Option<Value> {
    let (start, end, _next) = next_record(bytes, offset)?;
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
fn derive_columns(bytes: &[u8]) -> (Schema, Vec<ColumnKey>) {
    let mut keys: Vec<String> = Vec::new();
    let mut index_of: HashMap<String, usize> = HashMap::new();
    let mut types: Vec<TypeAcc> = Vec::new();
    let mut saw_object = false;

    let mut pos = 0usize;
    let mut sampled = 0usize;
    while sampled < SCHEMA_SAMPLE_ROWS {
        let Some((start, end, next)) = next_record(bytes, pos) else {
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

/// Find the next non-blank record at or after `pos`. Returns `(start, end, next)` where
/// `bytes[start..end]` is the record content (one line, sans terminator and trailing `\r`) and
/// `next` is where scanning resumes. Blank (all-whitespace) lines are skipped. `None` at EOF.
fn next_record(bytes: &[u8], mut pos: usize) -> Option<(usize, usize, usize)> {
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

/// A sparse record → byte-offset index over newline-delimited records (analogous to
/// [`crate::index::OffsetIndex`], but newline-based: NDJSON records never contain a raw newline).
struct LineIndex {
    anchors: Vec<u64>,
    row_count: u64,
}

impl LineIndex {
    fn build(bytes: &[u8]) -> Result<Self> {
        Self::build_with(bytes, &CancellationToken::new(), |_| {})
    }

    fn build_with(
        bytes: &[u8],
        cancel: &CancellationToken,
        mut progress: impl FnMut(Progress),
    ) -> Result<Self> {
        let mut anchors = Vec::new();
        let mut row_count: u64 = 0;
        let mut pos = 0usize;
        while let Some((start, _end, next)) = next_record(bytes, pos) {
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
    fn offset_of_row(&self, bytes: &[u8], row: u64) -> Option<usize> {
        if row >= self.row_count {
            return None;
        }
        let mut pos = self.anchors[(row / ANCHOR_INTERVAL) as usize] as usize;
        let skip = row % ANCHOR_INTERVAL;
        let mut i = 0u64;
        loop {
            let (start, _end, next) = next_record(bytes, pos)?;
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

    fn col_names(table: &NdjsonTable) -> Vec<String> {
        table
            .schema()
            .columns
            .iter()
            .map(|c| String::from_utf8_lossy(&c.name).into_owned())
            .collect()
    }

    fn await_indexed(table: &NdjsonTable) {
        for _ in 0..2000 {
            if matches!(table.row_count(), RowCount::Exact(_)) {
                return;
            }
            thread::sleep(Duration::from_millis(1));
        }
        panic!("index did not complete in time");
    }

    fn await_view(table: &NdjsonTable) {
        for _ in 0..2000 {
            if !table.view_status().building {
                return;
            }
            thread::sleep(Duration::from_millis(1));
        }
        panic!("view did not build in time");
    }

    #[test]
    fn columns_are_the_union_of_object_keys_in_first_seen_order() {
        let data = b"{\"name\":\"bob\",\"age\":30}\n{\"name\":\"ann\",\"city\":\"rome\"}\n";
        let table = NdjsonTable::from_bytes(data.to_vec()).unwrap();
        assert_eq!(col_names(&table), vec!["name", "age", "city"]);
        assert!(matches!(table.row_count(), RowCount::Exact(2)));
    }

    #[test]
    fn projects_fields_by_key_with_missing_rendered_empty() {
        let data = b"{\"name\":\"bob\",\"age\":30}\n{\"name\":\"ann\",\"city\":\"rome\"}\n";
        let table = NdjsonTable::from_bytes(data.to_vec()).unwrap();
        // columns: name, age, city
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
        let table = NdjsonTable::from_bytes(data.to_vec()).unwrap();
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
    fn infers_integer_columns_for_alignment() {
        let data = b"{\"name\":\"a\",\"age\":30}\n{\"name\":\"b\",\"age\":25}\n";
        let table = NdjsonTable::from_bytes(data.to_vec()).unwrap();
        let cols = &table.schema().columns;
        assert_eq!(cols[0].inferred, InferredType::Text);
        assert_eq!(cols[1].inferred, InferredType::Integer);
    }

    #[test]
    fn falls_back_to_a_single_value_column_for_non_objects() {
        let data = b"1\n\"hello\"\n[1,2,3]\n";
        let table = NdjsonTable::from_bytes(data.to_vec()).unwrap();
        assert_eq!(col_names(&table), vec!["value"]);
        let viewport = table
            .fetch(&request(0, 3, &[0]), &CancellationToken::new())
            .unwrap();
        assert_eq!(
            cells(&viewport),
            vec![
                vec!["1".to_string()],
                vec!["hello".to_string()],
                vec!["[1,2,3]".to_string()],
            ]
        );
    }

    #[test]
    fn skips_blank_lines() {
        let data = b"{\"a\":1}\n\n   \n{\"a\":2}\n";
        let table = NdjsonTable::from_bytes(data.to_vec()).unwrap();
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
    fn opens_and_reads_a_file_via_mmap() {
        use std::io::Write;
        let mut file = tempfile::NamedTempFile::new().unwrap();
        file.write_all(b"{\"a\":1,\"b\":2}\n{\"a\":3,\"b\":4}\n")
            .unwrap();

        let table = NdjsonTable::open(file.path()).unwrap();
        await_indexed(&table);

        assert!(matches!(table.row_count(), RowCount::Exact(2)));
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

    #[test]
    fn resolves_offsets_across_many_anchors() {
        // More records than ANCHOR_INTERVAL forces multiple anchors.
        let mut data = Vec::new();
        for i in 0..3000u32 {
            data.extend_from_slice(format!("{{\"i\":{i}}}\n").as_bytes());
        }
        let table = NdjsonTable::from_bytes(data).unwrap();
        assert!(matches!(table.row_count(), RowCount::Exact(3000)));
        for row in [0u64, 1023, 1024, 1025, 2999] {
            let viewport = table
                .fetch(&request(row, 1, &[0]), &CancellationToken::new())
                .unwrap();
            assert_eq!(cells(&viewport)[0][0], row.to_string());
        }
    }

    #[test]
    fn global_filter_hides_non_matching_rows() {
        let data = b"{\"name\":\"apple\"}\n{\"name\":\"banana\"}\n{\"name\":\"avocado\"}\n";
        let table = NdjsonTable::from_bytes(data.to_vec()).unwrap();
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
    fn global_sort_orders_rows_numerically() {
        let data = b"{\"n\":3}\n{\"n\":1}\n{\"n\":2}\n";
        let table = NdjsonTable::from_bytes(data.to_vec()).unwrap();
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
}
