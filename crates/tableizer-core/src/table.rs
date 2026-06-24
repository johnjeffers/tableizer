//! A [`ViewportSource`] over a delimited file with **progressive availability**
//! (`docs/architecture.md`): [`CsvTable::open`] returns immediately (mmap + parse the head for the
//! schema) and builds
//! the offset index on a background thread. Until the index lands, rows are served by streaming from
//! the head; once it lands, lookups are O(1) random access. The row count grows honestly
//! ([`RowCount::AtLeast`] → [`RowCount::Exact`]).
//!
//! Cells hold the exact field bytes the parser yields — type inference is presentational only and
//! never mutates them (the byte-fidelity invariant — see `AGENTS.md`).

use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::thread;

use crate::index::{OffsetIndex, record_reader};
use crate::parse::{Dialect, MAX_RECORD_BYTES};
use crate::search::Matcher;
use crate::source::{Source, map_file};
use crate::{
    CancellationToken, Cell, Column, ColumnId, DataQuality, Error, InferredType, Result, RowCount,
    RowRange, Schema, SortKey, ViewSpec, ViewStatus, Viewport, ViewportRequest, ViewportSource,
};

/// A read-only, byte-faithful table view over a delimited file. Implements [`ViewportSource`] so any
/// GUI can request small row slices, and never blocks on a full scan to show the first screen.
pub struct CsvTable {
    source: Source,
    schema: Schema,
    dialect: Dialect,
    /// The complete index, populated once the background build finishes. `None` while building.
    index: Arc<Mutex<Option<OffsetIndex>>>,
    /// Rows indexed so far — the honest lower-bound row count while the index builds.
    frontier: Arc<AtomicU64>,
    /// Cancels the background builder when the table is dropped.
    cancel: CancellationToken,
    /// Physical records that precede the data (1 if the dialect has a header row, else 0). The
    /// header is excluded from the row count and shifts data-row → physical-record translation.
    header_rows: u64,
    /// Active view: an ordered list of data-rows to display (filter + sort applied), or `None` for
    /// source order. Rebuilt asynchronously by `set_view`.
    view: Arc<Mutex<Option<Vec<u64>>>>,
    /// Whether a view build is currently running.
    view_building: Arc<AtomicBool>,
    /// Cancels the in-flight view build when a new one starts or the table is dropped.
    view_cancel: Mutex<CancellationToken>,
}

impl CsvTable {
    /// Build a table over in-memory bytes (small files, or tests). Indexed synchronously.
    pub fn from_bytes(bytes: impl Into<Arc<[u8]>>, dialect: Dialect) -> Result<Self> {
        let source = Source::Bytes(bytes.into());
        let schema = derive_schema(source.bytes(), &dialect);
        let index = OffsetIndex::build(source.bytes(), &dialect)?;
        Ok(Self {
            frontier: Arc::new(AtomicU64::new(index.row_count())),
            index: Arc::new(Mutex::new(Some(index))),
            source,
            schema,
            header_rows: u64::from(dialect.has_header),
            dialect,
            cancel: CancellationToken::new(),
            view: Arc::new(Mutex::new(None)),
            view_building: Arc::new(AtomicBool::new(false)),
            view_cancel: Mutex::new(CancellationToken::new()),
        })
    }

    /// Open a file by memory-mapping it. Returns **immediately** (mmap + head parse); the offset
    /// index builds on a background thread so the first screen is never gated on a full scan.
    pub fn open(path: &Path, dialect: Dialect) -> Result<Self> {
        let source = open_source(path)?;
        let schema = derive_schema(source.bytes(), &dialect);
        let index = Arc::new(Mutex::new(None));
        let frontier = Arc::new(AtomicU64::new(0));
        let cancel = CancellationToken::new();

        if let Some(cached) = crate::cache::load(path, &dialect) {
            // Cache hit: fully indexed instantly, no background build (skips the O(n) rescan).
            frontier.store(cached.row_count(), Ordering::Relaxed);
            *index.lock().expect("index lock") = Some(cached);
        } else {
            // Cache miss: build in the background and persist the result for next time.
            spawn_index_build(
                source.clone(),
                dialect,
                path.to_path_buf(),
                Arc::clone(&index),
                Arc::clone(&frontier),
                cancel.clone(),
            );
        }

        Ok(Self {
            header_rows: u64::from(dialect.has_header),
            source,
            schema,
            dialect,
            index,
            frontier,
            cancel,
            view: Arc::new(Mutex::new(None)),
            view_building: Arc::new(AtomicBool::new(false)),
            view_cancel: Mutex::new(CancellationToken::new()),
        })
    }
}

impl Drop for CsvTable {
    fn drop(&mut self) {
        // Stop the background index + view builds promptly; their `Arc`s keep data alive until they exit.
        self.cancel.cancel();
        self.view_cancel.lock().expect("view cancel lock").cancel();
    }
}

impl ViewportSource for CsvTable {
    fn schema(&self) -> &Schema {
        &self.schema
    }

    fn row_count(&self) -> RowCount {
        // An active view (filter/sort) defines its own row count.
        if let Some(len) = self
            .view
            .lock()
            .expect("view lock")
            .as_ref()
            .map(|rows| rows.len() as u64)
        {
            return RowCount::Exact(len);
        }
        let header = self.header_rows;
        match self.index.lock().expect("index lock").as_ref() {
            Some(index) => RowCount::Exact(index.row_count().saturating_sub(header)),
            None => RowCount::AtLeast(self.frontier.load(Ordering::Relaxed).saturating_sub(header)),
        }
    }

    fn fetch(&self, request: &ViewportRequest, cancel: &CancellationToken) -> Result<Viewport> {
        // Resolve the visible display-rows to *data* rows through the active view (small window).
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
        self.fetch_data_rows(&data_rows, &request.columns, cancel)
    }

    fn fetch_source(
        &self,
        request: &ViewportRequest,
        cancel: &CancellationToken,
    ) -> Result<Viewport> {
        // Source order, ignoring any active filter/sort: data rows are exactly the display range.
        let data_rows = contiguous_rows(request.rows);
        self.fetch_data_rows(&data_rows, &request.columns, cancel)
    }

    fn data_quality(&self) -> DataQuality {
        match self.index.lock().expect("index lock").as_ref() {
            Some(index) => DataQuality {
                ragged_rows: index.ragged_rows(),
                complete: true,
            },
            None => DataQuality::default(),
        }
    }

    fn set_view(&self, spec: &ViewSpec) -> Result<()> {
        // Compile the filter up front so an invalid regex errors synchronously.
        let matcher = match &spec.filter {
            Some(filter) => Some(Matcher::compile(filter)?),
            None => None,
        };
        let sort = spec.sort;

        // Cancel any in-flight build and install a fresh cancellation token.
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
        let dialect = self.dialect;
        let header_rows = self.header_rows;
        let view = Arc::clone(&self.view);
        let building = Arc::clone(&self.view_building);
        thread::spawn(move || {
            let built = build_view(
                source.bytes(),
                &dialect,
                header_rows,
                matcher,
                sort,
                &cancel,
            );
            if let Ok(rows) = built {
                // Re-check cancellation while holding the view lock. `clear_view` / an identity
                // `set_view` cancel first and *then* take this lock to install `None`, so checking
                // under the lock makes the store atomic with "is this result still wanted" — a
                // cancelled build can't resurrect a view the user just cleared.
                let mut guard = view.lock().expect("view lock");
                if !cancel.is_cancelled() {
                    *guard = Some(rows);
                }
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

impl CsvTable {
    /// Materialise the given data-rows (already resolved through any view) into a viewport: each row
    /// is addressed via the offset index. Falls back to head-streaming if the index isn't ready yet.
    fn fetch_data_rows(
        &self,
        data_rows: &[u64],
        columns: &[ColumnId],
        cancel: &CancellationToken,
    ) -> Result<Viewport> {
        let bytes = self.source.bytes();
        let guard = self.index.lock().expect("index lock");
        let Some(index) = guard.as_ref() else {
            // Index not ready → stream from the head, resolving each *data-row* index. Doing it by
            // data-row (not the raw display range) keeps an active filter/sort correct in the brief
            // gap before the index lands, instead of falling back to source order.
            drop(guard);
            return fetch_streaming(
                bytes,
                &self.dialect,
                self.header_rows,
                data_rows,
                columns,
                cancel,
            );
        };
        let total_physical = index.row_count();
        let mut rows = Vec::with_capacity(data_rows.len());
        for &data_row in data_rows {
            if cancel.is_cancelled() {
                return Err(Error::Cancelled);
            }
            let phys = data_row.saturating_add(self.header_rows);
            // A stale-but-undetected index (file changed under an identical size+mtime) could hand
            // back an out-of-range or unparseable offset. Degrade to an empty row rather than
            // panicking — a wrong fetch must never crash a data viewer.
            let cells = (phys < total_physical)
                .then(|| index.offset_of_row(bytes, phys))
                .flatten()
                .and_then(|offset| parse_one_record(&bytes[offset as usize..], &self.dialect))
                .map_or_else(
                    || empty_row(columns),
                    |record| project_record(&record, columns),
                );
            rows.push(cells);
        }
        Ok(Viewport { rows })
    }
}

/// The contiguous data-rows covered by a display `range` (the identity view).
fn contiguous_rows(range: RowRange) -> Vec<u64> {
    (range.start..range.start.saturating_add(u64::from(range.len))).collect()
}

/// Build the display order for a view: scan every data record, keep those passing `matcher`, and (if
/// `sort` is set) order them by the sort-key field. Returns data-row indices in display order. Sorts
/// in memory — the spill-to-disk external merge sort (§4.3) is the documented refinement.
fn build_view(
    bytes: &[u8],
    dialect: &Dialect,
    header_rows: u64,
    matcher: Option<Matcher>,
    sort: Option<SortKey>,
    cancel: &CancellationToken,
) -> Result<Vec<u64>> {
    let mut reader = record_reader(bytes, dialect);
    let mut record = csv::ByteRecord::new();
    let mut rows: Vec<(Vec<u8>, u64)> = Vec::new();
    let mut physical = 0u64;

    while reader.read_byte_record(&mut record).map_err(parse_io)? {
        let phys = physical;
        physical += 1;
        if phys < header_rows {
            continue;
        }
        if phys.is_multiple_of(4096) && cancel.is_cancelled() {
            return Err(Error::Cancelled);
        }
        if let Some(matcher) = &matcher
            && !matcher.matches(&record)
        {
            continue;
        }
        let key = match &sort {
            Some(sort) => record
                .get(sort.column.0 as usize)
                .unwrap_or_default()
                .to_vec(),
            None => Vec::new(),
        };
        rows.push((key, phys - header_rows));
    }

    if let Some(sort) = &sort {
        rows.sort_by(|a, b| crate::sort::compare_keys(&a.0, &b.0, sort.direction));
    }
    Ok(rows.into_iter().map(|(_, data_row)| data_row).collect())
}

/// Spawn the background index build, publishing the growing row count via `frontier` and storing the
/// finished index into `slot`.
fn spawn_index_build(
    source: Source,
    dialect: Dialect,
    save_path: PathBuf,
    slot: Arc<Mutex<Option<OffsetIndex>>>,
    frontier: Arc<AtomicU64>,
    cancel: CancellationToken,
) {
    thread::spawn(move || {
        let progress_frontier = Arc::clone(&frontier);
        let built = OffsetIndex::build_with(source.bytes(), &dialect, &cancel, |p| {
            progress_frontier.store(p.rows_indexed, Ordering::Relaxed);
        });
        if let Ok(index) = built {
            crate::cache::save(&save_path, &dialect, &index); // persist for the next open
            frontier.store(index.row_count(), Ordering::Relaxed);
            *slot.lock().expect("index lock") = Some(index);
        }
        // On cancel/error the slot stays `None`; the table is being dropped or failed.
    });
}

/// Serve the requested `data_rows` by parsing forward from the head — the path used before the index
/// is ready. Resolves by data-row index (not a contiguous range), so it is correct for an active
/// filter/sort view as well as source order. O(max data-row), so it is cheap for the first screens
/// and degrades for deep jumps until the index lands.
fn fetch_streaming(
    bytes: &[u8],
    dialect: &Dialect,
    header_rows: u64,
    data_rows: &[u64],
    columns: &[ColumnId],
    cancel: &CancellationToken,
) -> Result<Viewport> {
    let Some(&max_data_row) = data_rows.iter().max() else {
        return Ok(Viewport::default());
    };
    let wanted: HashSet<u64> = data_rows.iter().copied().collect();
    let mut found: HashMap<u64, Vec<Cell>> = HashMap::with_capacity(data_rows.len());
    let mut reader = record_reader(bytes, dialect);
    let mut record = csv::ByteRecord::new();
    let mut phys = 0u64;
    let max_phys = max_data_row.saturating_add(header_rows);
    while phys <= max_phys {
        if cancel.is_cancelled() {
            return Err(Error::Cancelled);
        }
        if !reader.read_byte_record(&mut record).map_err(parse_io)? {
            break;
        }
        if let Some(data_row) = phys.checked_sub(header_rows)
            && wanted.contains(&data_row)
        {
            found.insert(data_row, project_record(&record, columns));
        }
        phys += 1;
    }
    let rows = data_rows
        .iter()
        .map(|dr| found.get(dr).cloned().unwrap_or_else(|| empty_row(columns)))
        .collect();
    Ok(Viewport { rows })
}

/// A row of empty cells, one per requested column — the placeholder for a row that can't be served
/// (missing from a streamed window, or an offset that failed to parse against a stale index).
fn empty_row(columns: &[ColumnId]) -> Vec<Cell> {
    columns.iter().map(|_| Cell(Box::default())).collect()
}

/// Project a record onto the requested columns, in order, as byte-faithful cells.
fn project_record(record: &csv::ByteRecord, columns: &[ColumnId]) -> Vec<Cell> {
    columns
        .iter()
        .map(|c| {
            let field: &[u8] = record.get(c.0 as usize).unwrap_or_default();
            Cell(field.into())
        })
        .collect()
}

/// Derive the schema from the first record. With a header dialect, column names are the header
/// field bytes (preserved exactly); otherwise positional (`col0`, `col1`, …). All typed as text.
fn derive_schema(bytes: &[u8], dialect: &Dialect) -> Schema {
    let Some(first) = parse_one_record(bytes, dialect) else {
        return Schema::default();
    };
    let column_count = first.len();
    let types = infer_column_types(bytes, dialect, column_count);
    let columns = (0..column_count)
        .map(|i| {
            let name: Box<[u8]> = if dialect.has_header {
                let field: &[u8] = first.get(i).unwrap_or_default();
                field.into()
            } else {
                format!("col{i}").into_bytes().into_boxed_slice()
            };
            Column {
                id: ColumnId(i as u32),
                name,
                inferred: types.get(i).copied().unwrap_or_default(),
            }
        })
        .collect();
    Schema { columns }
}

/// Records sampled from the head to infer column types (presentational only — alignment/formatting).
const TYPE_SAMPLE_ROWS: usize = 200;

/// Infer a presentational type per column by sampling the first [`TYPE_SAMPLE_ROWS`] data records: a
/// column is `Integer`/`Float`/`Boolean` only if every non-empty sampled value fits (empties are
/// treated as nulls and never disqualify a type); otherwise `Text`. Never mutates cell bytes.
fn infer_column_types(bytes: &[u8], dialect: &Dialect, column_count: usize) -> Vec<InferredType> {
    // Cap the sampled head so a pathological giant first field can't balloon inference into memory.
    let mut reader = record_reader(bounded(bytes), dialect);
    let mut record = csv::ByteRecord::new();
    let mut all_int = vec![true; column_count];
    let mut all_float = vec![true; column_count];
    let mut all_bool = vec![true; column_count];
    let mut any_value = vec![false; column_count];

    let skip = usize::from(dialect.has_header);
    let (mut physical, mut sampled) = (0usize, 0usize);
    while sampled < TYPE_SAMPLE_ROWS && reader.read_byte_record(&mut record).unwrap_or(false) {
        if physical < skip {
            physical += 1;
            continue;
        }
        physical += 1;
        sampled += 1;
        for c in 0..column_count {
            let Ok(text) = std::str::from_utf8(record.get(c).unwrap_or_default()) else {
                all_int[c] = false;
                all_float[c] = false;
                all_bool[c] = false;
                continue;
            };
            let text = text.trim();
            if text.is_empty() {
                continue; // empty = null; doesn't disqualify any type
            }
            any_value[c] = true;
            all_int[c] &= text.parse::<i64>().is_ok();
            all_float[c] &= text.parse::<f64>().is_ok();
            all_bool[c] &= matches!(text.to_ascii_lowercase().as_str(), "true" | "false");
        }
    }

    (0..column_count)
        .map(|c| {
            if !any_value[c] {
                InferredType::Text
            } else if all_int[c] {
                InferredType::Integer
            } else if all_float[c] {
                InferredType::Float
            } else if all_bool[c] {
                InferredType::Boolean
            } else {
                InferredType::Text
            }
        })
        .collect()
}

/// Parse exactly one record from the start of `bytes`. The input is capped at [`MAX_RECORD_BYTES`]
/// so a single unterminated field can't be read whole into memory on the random-access path.
fn parse_one_record(bytes: &[u8], dialect: &Dialect) -> Option<csv::ByteRecord> {
    let mut reader = record_reader(bounded(bytes), dialect);
    let mut record = csv::ByteRecord::new();
    if reader.read_byte_record(&mut record).ok()? {
        Some(record)
    } else {
        None
    }
}

/// The leading slice of `bytes` capped at [`MAX_RECORD_BYTES`] (resource-exhaustion guard).
fn bounded(bytes: &[u8]) -> &[u8] {
    &bytes[..bytes.len().min(MAX_RECORD_BYTES)]
}

/// Map a `csv` read error into our I/O error (malformed-row *content* is not an error here).
fn parse_io(e: csv::Error) -> Error {
    Error::Io(std::io::Error::other(e))
}

/// Build the byte source for a file: memory-mapped for single-byte encodings, or transcoded to UTF-8
/// for UTF-16 (byte-level CSV parsing needs single-byte delimiters). UTF-16 is read fully into memory
/// — fine for the small exports it's typically used for; huge UTF-16 files are not streamed.
fn open_source(path: &Path) -> Result<Source> {
    let mmap = map_file(path)?;
    if let Some((encoding, _)) = encoding_rs::Encoding::for_bom(&mmap)
        && (std::ptr::eq(encoding, encoding_rs::UTF_16LE)
            || std::ptr::eq(encoding, encoding_rs::UTF_16BE))
    {
        let (utf8, _, _) = encoding.decode(&mmap); // also strips the BOM
        return Ok(Source::Bytes(Arc::from(utf8.into_owned().into_bytes())));
    }
    Ok(Source::Mmap(Arc::new(mmap)))
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

    /// A dialect that treats every physical record as data (no header), for tests that assert on
    /// raw row contents.
    fn no_header() -> Dialect {
        Dialect {
            has_header: false,
            ..Dialect::default()
        }
    }

    /// Point the index cache at a fresh tempdir so tests never touch the real OS state dir. Keep the
    /// returned guard alive for the test's duration.
    fn isolate_cache() -> tempfile::TempDir {
        let dir = tempfile::tempdir().unwrap();
        // SAFETY: each open-test holds its own cache guard; concurrent env reads only ever see *some*
        // tempdir, and no test depends on a cross-test cache hit.
        #[allow(unsafe_code)]
        unsafe {
            std::env::set_var("TABLEIZER_CACHE_DIR", dir.path());
        }
        dir
    }

    /// Encode text as UTF-16LE with a BOM (for the transcoding test).
    fn to_utf16le(s: &str) -> Vec<u8> {
        let mut out = vec![0xFF, 0xFE];
        for unit in s.encode_utf16() {
            out.extend_from_slice(&unit.to_le_bytes());
        }
        out
    }

    /// Wait for a background build to finish (tiny test files complete almost instantly).
    fn await_indexed(table: &CsvTable) {
        for _ in 0..2000 {
            if matches!(table.row_count(), RowCount::Exact(_)) {
                return;
            }
            thread::sleep(Duration::from_millis(1));
        }
        panic!("index did not complete in time");
    }

    /// Wait for an async view (filter/sort) build to finish.
    fn await_view(table: &CsvTable) {
        for _ in 0..2000 {
            if !table.view_status().building {
                return;
            }
            thread::sleep(Duration::from_millis(1));
        }
        panic!("view did not build in time");
    }

    #[test]
    fn fetches_cells_as_exact_field_bytes() {
        let table = CsvTable::from_bytes(b"a,b\nc,d\n".to_vec(), no_header()).unwrap();

        assert!(matches!(table.row_count(), RowCount::Exact(2)));
        let viewport = table
            .fetch(&request(0, 2, &[0, 1]), &CancellationToken::new())
            .unwrap();

        assert_eq!(viewport.rows.len(), 2);
        assert_eq!(viewport.rows[0][0].0.as_ref(), b"a");
        assert_eq!(viewport.rows[0][1].0.as_ref(), b"b");
        assert_eq!(viewport.rows[1][0].0.as_ref(), b"c");
        assert_eq!(viewport.rows[1][1].0.as_ref(), b"d");
    }

    #[test]
    fn projects_only_requested_columns_in_the_requested_order() {
        let table = CsvTable::from_bytes(b"a,b,c\n".to_vec(), no_header()).unwrap();

        let viewport = table
            .fetch(&request(0, 1, &[2, 0]), &CancellationToken::new())
            .unwrap();

        assert_eq!(viewport.rows[0][0].0.as_ref(), b"c");
        assert_eq!(viewport.rows[0][1].0.as_ref(), b"a");
    }

    #[test]
    fn schema_has_one_column_per_field() {
        let table = CsvTable::from_bytes(b"a,b,c\n1,2,3\n".to_vec(), no_header()).unwrap();
        assert_eq!(table.schema().columns.len(), 3);
    }

    #[test]
    fn opens_and_reads_a_file_via_mmap() {
        use std::io::Write;
        let _cache = isolate_cache();

        let mut file = tempfile::NamedTempFile::new().unwrap();
        file.write_all(b"x,y\n1,2\n3,4\n").unwrap();

        let table = CsvTable::open(file.path(), no_header()).unwrap();
        await_indexed(&table);

        assert!(matches!(table.row_count(), RowCount::Exact(3)));
        let viewport = table
            .fetch(&request(1, 2, &[0, 1]), &CancellationToken::new())
            .unwrap();
        assert_eq!(viewport.rows[0][0].0.as_ref(), b"1");
        assert_eq!(viewport.rows[1][1].0.as_ref(), b"4");
    }

    #[test]
    fn streaming_fetch_matches_indexed_fetch() {
        // The progressive (pre-index) path must return byte-identical cells to the indexed path —
        // this is the regression guard for first-paint-before-index.
        let data = b"a,b\n\"c\nc\",d\ne,f\n".to_vec();
        let dialect = no_header();
        let cols = [ColumnId(0), ColumnId(1)];

        let table = CsvTable::from_bytes(data.clone(), dialect).unwrap();
        let indexed = table
            .fetch(&request(1, 2, &[0, 1]), &CancellationToken::new())
            .unwrap();
        let streamed = fetch_streaming(
            &data,
            &dialect,
            0,
            &[1, 2],
            &cols,
            &CancellationToken::new(),
        )
        .unwrap();

        assert_eq!(indexed.rows, streamed.rows);
        assert_eq!(streamed.rows[0][0].0.as_ref(), b"c\nc"); // quoted embedded newline preserved
    }

    #[test]
    fn streaming_fallback_resolves_scattered_view_rows() {
        // The pre-index fallback must honour an active view's row mapping, not just source order:
        // requesting data-rows [2, 0] returns those rows in that order (with header skipped).
        let data = b"h\nr0\nr1\nr2\n".to_vec();
        let cols = [ColumnId(0)];
        let streamed = fetch_streaming(
            &data,
            &Dialect::default(),
            1, // one header row
            &[2, 0],
            &cols,
            &CancellationToken::new(),
        )
        .unwrap();
        assert_eq!(streamed.rows[0][0].0.as_ref(), b"r2");
        assert_eq!(streamed.rows[1][0].0.as_ref(), b"r0");
    }

    #[test]
    fn header_row_names_columns_and_is_excluded_from_data() {
        let table =
            CsvTable::from_bytes(b"name,age\nbob,30\nann,25\n".to_vec(), Dialect::default())
                .unwrap();

        // The header is excluded from the data view.
        assert!(matches!(table.row_count(), RowCount::Exact(2)));
        // Column names come from the header row.
        assert_eq!(table.schema().columns[0].name.as_ref(), b"name");
        assert_eq!(table.schema().columns[1].name.as_ref(), b"age");
        // Data row 0 is the first record *after* the header.
        let viewport = table
            .fetch(&request(0, 1, &[0, 1]), &CancellationToken::new())
            .unwrap();
        assert_eq!(viewport.rows[0][0].0.as_ref(), b"bob");
        assert_eq!(viewport.rows[0][1].0.as_ref(), b"30");
    }

    #[test]
    fn opens_a_utf16le_file_by_transcoding_to_utf8() {
        use std::io::Write;
        let _cache = isolate_cache();

        let mut file = tempfile::NamedTempFile::new().unwrap();
        file.write_all(&to_utf16le("a,b\n1,2\n3,4\n")).unwrap();

        let table = CsvTable::open(file.path(), no_header()).unwrap();
        await_indexed(&table);

        // UTF-16 was transcoded to UTF-8, so the structure parses and cells are UTF-8 bytes.
        assert!(matches!(table.row_count(), RowCount::Exact(3)));
        let viewport = table
            .fetch(&request(0, 3, &[0, 1]), &CancellationToken::new())
            .unwrap();
        assert_eq!(viewport.rows[0][0].0.as_ref(), b"a");
        assert_eq!(viewport.rows[2][1].0.as_ref(), b"4");
    }

    #[test]
    fn global_sort_orders_rows_by_a_column() {
        // Header "n"; data rows 3, 1, 2 → ascending sort yields 1, 2, 3.
        let table = CsvTable::from_bytes(b"n\n3\n1\n2\n".to_vec(), Dialect::default()).unwrap();
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
        assert_eq!(viewport.rows[0][0].0.as_ref(), b"1");
        assert_eq!(viewport.rows[1][0].0.as_ref(), b"2");
        assert_eq!(viewport.rows[2][0].0.as_ref(), b"3");
    }

    #[test]
    fn global_filter_hides_non_matching_rows() {
        let table = CsvTable::from_bytes(
            b"name\napple\nbanana\navocado\n".to_vec(),
            Dialect::default(),
        )
        .unwrap();
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
        assert_eq!(viewport.rows.len(), 1);
        assert_eq!(viewport.rows[0][0].0.as_ref(), b"avocado");
    }

    #[test]
    fn clear_view_returns_to_source_order() {
        let table = CsvTable::from_bytes(b"n\n3\n1\n2\n".to_vec(), Dialect::default()).unwrap();
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
        table.clear_view();

        let viewport = table
            .fetch(&request(0, 3, &[0]), &CancellationToken::new())
            .unwrap();
        assert_eq!(viewport.rows[0][0].0.as_ref(), b"3"); // source order restored
    }

    #[test]
    fn infers_numeric_columns_for_alignment() {
        let table =
            CsvTable::from_bytes(b"name,age\nalice,30\nbob,25\n".to_vec(), Dialect::default())
                .unwrap();
        let columns = &table.schema().columns;
        assert_eq!(columns[0].inferred, InferredType::Text); // "name" is text
        assert_eq!(columns[1].inferred, InferredType::Integer); // "age" is integer
    }
}
