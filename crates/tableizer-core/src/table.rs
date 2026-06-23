//! A [`ViewportSource`] over a delimited file with **progressive availability** (`docs/spec.md` §2,
//! §4.1): [`CsvTable::open`] returns immediately (mmap + parse the head for the schema) and builds
//! the offset index on a background thread. Until the index lands, rows are served by streaming from
//! the head; once it lands, lookups are O(1) random access. The row count grows honestly
//! ([`RowCount::AtLeast`] → [`RowCount::Exact`]).
//!
//! Cells hold the exact field bytes the parser yields — type inference is presentational only and
//! never mutates them (the byte-fidelity invariant, spec §3.1).

use std::fs::File;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::thread;

use memmap2::Mmap;

use crate::index::{OffsetIndex, record_reader};
use crate::parse::Dialect;
use crate::{
    CancellationToken, Cell, Column, ColumnId, DataQuality, Error, InferredType, Result, RowCount,
    RowRange, Schema, Viewport, ViewportRequest, ViewportSource,
};

/// The byte source backing a table: either owned in-memory bytes or a memory-mapped file. Both
/// deref to `&[u8]`, so the rest of the engine is agnostic to which it is. Cheap to clone (the
/// background index builder holds its own `Arc` to keep the bytes alive).
#[derive(Clone)]
enum Source {
    Bytes(Arc<[u8]>),
    Mmap(Arc<Mmap>),
}

impl Source {
    fn bytes(&self) -> &[u8] {
        match self {
            Source::Bytes(b) => b,
            Source::Mmap(m) => m,
        }
    }
}

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
        })
    }
}

impl Drop for CsvTable {
    fn drop(&mut self) {
        // Stop the background index build promptly; its `Arc`s keep the data alive until it exits.
        self.cancel.cancel();
    }
}

impl ViewportSource for CsvTable {
    fn schema(&self) -> &Schema {
        &self.schema
    }

    fn row_count(&self) -> RowCount {
        let header = self.header_rows;
        match self.index.lock().expect("index lock").as_ref() {
            Some(index) => RowCount::Exact(index.row_count().saturating_sub(header)),
            None => RowCount::AtLeast(self.frontier.load(Ordering::Relaxed).saturating_sub(header)),
        }
    }

    fn fetch(&self, request: &ViewportRequest, cancel: &CancellationToken) -> Result<Viewport> {
        let bytes = self.source.bytes();
        // Requests are in *data* rows; the index addresses *physical* records (header included).
        let phys_start = request.rows.start.saturating_add(self.header_rows);
        let guard = self.index.lock().expect("index lock");
        match guard.as_ref() {
            // Index ready: O(1) random access per physical record.
            Some(index) => {
                let total = index.row_count(); // physical
                let phys_end = phys_start
                    .saturating_add(u64::from(request.rows.len))
                    .min(total);
                let mut rows = Vec::new();
                for phys in phys_start..phys_end {
                    if cancel.is_cancelled() {
                        return Err(Error::Cancelled);
                    }
                    let offset = index
                        .offset_of_row(bytes, phys)
                        .expect("a row below the total has an indexed offset");
                    let record = parse_one_record(&bytes[offset as usize..], &self.dialect)
                        .expect("a record exists at an indexed offset");
                    rows.push(project_record(&record, &request.columns));
                }
                Ok(Viewport { rows })
            }
            // Index still building: serve the visible window by streaming from the head.
            None => {
                drop(guard);
                fetch_streaming(
                    bytes,
                    &self.dialect,
                    RowRange {
                        start: phys_start,
                        len: request.rows.len,
                    },
                    &request.columns,
                    cancel,
                )
            }
        }
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

/// Serve `rows` by parsing forward from the head — the path used before the index is ready. O(start),
/// so it is cheap for the first screens and degrades for deep jumps until the index lands.
fn fetch_streaming(
    bytes: &[u8],
    dialect: &Dialect,
    rows: RowRange,
    columns: &[ColumnId],
    cancel: &CancellationToken,
) -> Result<Viewport> {
    let mut reader = record_reader(bytes, dialect);
    let mut record = csv::ByteRecord::new();

    let mut skipped = 0u64;
    while skipped < rows.start {
        if cancel.is_cancelled() {
            return Err(Error::Cancelled);
        }
        if !reader.read_byte_record(&mut record).map_err(parse_io)? {
            return Ok(Viewport::default());
        }
        skipped += 1;
    }

    let mut out = Vec::new();
    for _ in 0..rows.len {
        if cancel.is_cancelled() {
            return Err(Error::Cancelled);
        }
        if !reader.read_byte_record(&mut record).map_err(parse_io)? {
            break;
        }
        out.push(project_record(&record, columns));
    }
    Ok(Viewport { rows: out })
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
    let columns = (0..first.len())
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
                inferred: InferredType::Text,
            }
        })
        .collect();
    Schema { columns }
}

/// Parse exactly one record from the start of `bytes`.
fn parse_one_record(bytes: &[u8], dialect: &Dialect) -> Option<csv::ByteRecord> {
    let mut reader = record_reader(bytes, dialect);
    let mut record = csv::ByteRecord::new();
    if reader.read_byte_record(&mut record).ok()? {
        Some(record)
    } else {
        None
    }
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

/// Memory-map a file read-only.
#[allow(unsafe_code)] // SAFETY justified at the `Mmap::map` call below.
fn map_file(path: &Path) -> Result<Mmap> {
    let file = File::open(path)?;
    // SAFETY: we map a read-only view of `file` and never mutate the mapping. Documented risk:
    // another process truncating the file could cause SIGBUS on later access (spec §4.2); Phase 0
    // accepts this — a SIGBUS guard / positioned-read fallback is planned.
    Ok(unsafe { Mmap::map(&file)? })
}

#[cfg(test)]
mod tests {
    use super::*;
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
            RowRange { start: 1, len: 2 },
            &cols,
            &CancellationToken::new(),
        )
        .unwrap();

        assert_eq!(indexed.rows, streamed.rows);
        assert_eq!(streamed.rows[0][0].0.as_ref(), b"c\nc"); // quoted embedded newline preserved
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
}
