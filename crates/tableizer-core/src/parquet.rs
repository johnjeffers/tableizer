//! A [`ViewportSource`] over a Parquet file (`docs/formats.md` — the columnar format that proves
//! the format-reader seam).
//!
//! Unlike the text formats there is no offset index to build: the footer metadata already gives the
//! exact row count and the row-group layout, so [`open`](ParquetTable::open) is metadata-only and the
//! row count is [`RowCount::Exact`] from the first frame. A viewport fetch reads **only the visible
//! rows** via a Parquet `RowSelection` (whole row groups outside the window are skipped) and renders
//! the decoded Arrow values to UTF-8 text — there is no source byte for a typed value, so byte
//! fidelity is a faithful *rendering* here (the [`Cell`] carries those rendered bytes unchanged
//! through search / sort / export). Filter and sort do a full decode scan, the same Tier-C cost as
//! the CSV path's in-memory view build (§4.3).

use std::collections::HashMap;
use std::fs::File;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::thread;

use arrow::datatypes::{DataType, SchemaRef};
use arrow::util::display::{ArrayFormatter, FormatOptions};
use parquet::arrow::ProjectionMask;
use parquet::arrow::arrow_reader::{
    ArrowReaderMetadata, ArrowReaderOptions, ParquetRecordBatchReaderBuilder, RowSelection,
    RowSelector,
};
use parquet::file::metadata::PageIndexPolicy;

use crate::search::Matcher;
use crate::{
    CancellationToken, Cell, Column, ColumnId, DataQuality, Error, InferredType, Result, RowCount,
    RowRange, Schema, SortKey, ViewSpec, ViewStatus, Viewport, ViewportRequest, ViewportSource,
};

/// One cached fetch result — what [`ParquetTable::last`] holds so a static frame doesn't re-decode.
struct CachedFetch {
    rows: Vec<u64>,
    columns: Vec<ColumnId>,
    viewport: Viewport,
}

/// A read-only table view over a Parquet file. Row count + schema come from the footer (instant);
/// rows are decoded on demand from only the row groups the viewport touches.
pub struct ParquetTable {
    path: PathBuf,
    /// Cached Arrow reader metadata (footer + page index) — reused for every fetch so the footer is
    /// parsed once and `RowSelection` can skip at page granularity, not whole row groups.
    meta: ArrowReaderMetadata,
    arrow_schema: SchemaRef,
    schema: Schema,
    row_count: u64,
    view: Arc<Mutex<Option<Vec<u64>>>>,
    view_building: Arc<AtomicBool>,
    view_cancel: Mutex<CancellationToken>,
    /// One-entry fetch cache: the grid re-requests the same visible window every frame, so a static
    /// screen must not re-decode. Keyed by the exact rows + columns it served.
    last: Mutex<Option<CachedFetch>>,
}

impl ParquetTable {
    /// Open a Parquet file, reading only its footer metadata (row count, schema, row-group layout)
    /// plus the page index when present (enables page-level skipping for cheap deep random access).
    pub fn open(path: &Path) -> Result<Self> {
        let file = File::open(path)?;
        let options = ArrowReaderOptions::new().with_page_index_policy(PageIndexPolicy::Optional);
        let meta = ArrowReaderMetadata::load(&file, options).map_err(parquet_io)?;
        let arrow_schema = meta.schema().clone();
        let row_count = meta.metadata().file_metadata().num_rows().max(0) as u64;
        let schema = build_schema(&arrow_schema);
        Ok(Self {
            path: path.to_path_buf(),
            meta,
            arrow_schema,
            schema,
            row_count,
            view: Arc::new(Mutex::new(None)),
            view_building: Arc::new(AtomicBool::new(false)),
            view_cancel: Mutex::new(CancellationToken::new()),
            last: Mutex::new(None),
        })
    }
}

impl Drop for ParquetTable {
    fn drop(&mut self) {
        self.view_cancel.lock().expect("view cancel lock").cancel();
    }
}

impl ViewportSource for ParquetTable {
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
        RowCount::Exact(self.row_count)
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
        self.read_rows(&data_rows, &request.columns, cancel)
    }

    fn fetch_source(
        &self,
        request: &ViewportRequest,
        cancel: &CancellationToken,
    ) -> Result<Viewport> {
        self.read_rows(&contiguous_rows(request.rows), &request.columns, cancel)
    }

    fn data_quality(&self) -> DataQuality {
        DataQuality {
            ragged_rows: 0,
            complete: true,
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
        let path = self.path.clone();
        let meta = self.meta.clone();
        let view = Arc::clone(&self.view);
        let building = Arc::clone(&self.view_building);
        thread::spawn(move || {
            let built = build_view(&path, &meta, matcher, sort, &cancel);
            if let Ok(rows) = built {
                // Re-check cancellation under the view lock so a cancelled build can't overwrite a
                // view the user just cleared (clear/identity-set_view cancel then lock to set None).
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

impl ParquetTable {
    /// Materialise the given data-rows by reading only those rows from Parquet (via a `RowSelection`)
    /// and projecting the requested columns, in the requested order. The rows may be scattered (an
    /// active sort/filter view) or contiguous (source order) — both are one decode pass.
    fn read_rows(
        &self,
        data_rows: &[u64],
        columns: &[ColumnId],
        cancel: &CancellationToken,
    ) -> Result<Viewport> {
        let field_count = self.arrow_schema.fields().len();
        let empty_row = || -> Vec<Cell> {
            columns
                .iter()
                .map(|_| Cell(Vec::new().into_boxed_slice()))
                .collect::<Vec<_>>()
        };
        if data_rows.is_empty() {
            return Ok(Viewport::default());
        }

        // Serve a repeated identical request (every static frame) from the one-entry cache.
        if let Some(cached) = self.last.lock().expect("cache lock").as_ref()
            && cached.rows.as_slice() == data_rows
            && cached.columns.as_slice() == columns
        {
            return Ok(cached.viewport.clone());
        }

        // The distinct, in-range source columns to read, ascending (projection output order).
        let mut roots: Vec<usize> = columns
            .iter()
            .map(|c| c.0 as usize)
            .filter(|&i| i < field_count)
            .collect();
        roots.sort_unstable();
        roots.dedup();
        if roots.is_empty() {
            return Ok(Viewport {
                rows: data_rows.iter().map(|_| empty_row()).collect(),
            });
        }
        let pos_of: HashMap<usize, usize> =
            roots.iter().enumerate().map(|(p, &r)| (r, p)).collect();

        // The distinct, in-range rows to read, ascending — the reader yields them in this order.
        let mut sorted: Vec<u64> = data_rows.to_vec();
        sorted.sort_unstable();
        sorted.dedup();
        sorted.retain(|&r| r < self.row_count);
        if sorted.is_empty() {
            return Ok(Viewport {
                rows: data_rows.iter().map(|_| empty_row()).collect(),
            });
        }

        let mask =
            ProjectionMask::roots(self.meta.metadata().file_metadata().schema_descr(), roots);
        let file = File::open(&self.path)?;
        let reader = ParquetRecordBatchReaderBuilder::new_with_metadata(file, self.meta.clone())
            .with_projection(mask)
            .with_row_selection(row_selection(&sorted))
            .with_batch_size(sorted.len().clamp(1, 8192))
            .build()
            .map_err(parquet_io)?;

        // Decode + render each selected row, keyed by its absolute row index.
        let opts = FormatOptions::default().with_null("");
        let mut rendered: HashMap<u64, Vec<Vec<u8>>> = HashMap::with_capacity(sorted.len());
        let mut k = 0usize;
        for batch in reader {
            if cancel.is_cancelled() {
                return Err(Error::Cancelled);
            }
            let batch = batch.map_err(arrow_io)?;
            let formatters = formatters_for(&batch, &opts)?;
            for r in 0..batch.num_rows() {
                let Some(&abs) = sorted.get(k) else { break };
                k += 1;
                rendered.insert(abs, render_row(&formatters, r));
            }
        }

        // Assemble in the requested row + column order (missing → empty cell).
        let rows = data_rows
            .iter()
            .map(|&dr| {
                let row = rendered.get(&dr);
                columns
                    .iter()
                    .map(|c| {
                        let bytes = pos_of
                            .get(&(c.0 as usize))
                            .and_then(|&p| row.and_then(|cells| cells.get(p)))
                            .cloned()
                            .unwrap_or_default();
                        Cell(bytes.into_boxed_slice())
                    })
                    .collect()
            })
            .collect();
        let viewport = Viewport { rows };
        *self.last.lock().expect("cache lock") = Some(CachedFetch {
            rows: data_rows.to_vec(),
            columns: columns.to_vec(),
            viewport: viewport.clone(),
        });
        Ok(viewport)
    }
}

/// The contiguous data-rows covered by a display `range` (the identity view).
fn contiguous_rows(range: RowRange) -> Vec<u64> {
    (range.start..range.start.saturating_add(u64::from(range.len))).collect()
}

/// Build the display order for a view by scanning the whole file once: keep rows passing `matcher`,
/// and (if `sort` is set) order them by the rendered sort-key field. A filter must render every
/// column (it matches any field), so it reads all columns; a pure sort reads only the sort column.
fn build_view(
    path: &Path,
    meta: &ArrowReaderMetadata,
    matcher: Option<Matcher>,
    sort: Option<SortKey>,
    cancel: &CancellationToken,
) -> Result<Vec<u64>> {
    let file = File::open(path)?;
    let mut builder = ParquetRecordBatchReaderBuilder::new_with_metadata(file, meta.clone());
    // Project to just the sort column when there is no filter (a filter needs every field).
    let sort_pos = match (&matcher, &sort) {
        (None, Some(s)) => {
            let mask = ProjectionMask::roots(
                meta.metadata().file_metadata().schema_descr(),
                [s.column.0 as usize],
            );
            builder = builder.with_projection(mask);
            0 // the only projected column
        }
        _ => sort.map(|s| s.column.0 as usize).unwrap_or(0),
    };
    let reader = builder.with_batch_size(8192).build().map_err(parquet_io)?;

    let opts = FormatOptions::default().with_null("");
    let mut rows: Vec<(Vec<u8>, u64)> = Vec::new();
    let mut abs = 0u64;
    for batch in reader {
        if cancel.is_cancelled() {
            return Err(Error::Cancelled);
        }
        let batch = batch.map_err(arrow_io)?;
        let formatters = formatters_for(&batch, &opts)?;
        for r in 0..batch.num_rows() {
            let row = abs;
            abs += 1;
            if let Some(matcher) = &matcher {
                let fields = render_row(&formatters, r);
                if !matcher.matches_any(fields.iter().map(Vec::as_slice)) {
                    continue;
                }
            }
            let key = match &sort {
                Some(_) => formatters
                    .get(sort_pos)
                    .map(|f| f.value(r).to_string().into_bytes())
                    .unwrap_or_default(),
                None => Vec::new(),
            };
            rows.push((key, row));
        }
    }
    if let Some(sort) = &sort {
        rows.sort_by(|a, b| crate::sort::compare_keys(&a.0, &b.0, sort.direction));
    }
    Ok(rows.into_iter().map(|(_, row)| row).collect())
}

/// One [`ArrayFormatter`] per column of `batch` (renders Arrow values to text; nulls → empty).
fn formatters_for<'a>(
    batch: &'a arrow::record_batch::RecordBatch,
    opts: &'a FormatOptions,
) -> Result<Vec<ArrayFormatter<'a>>> {
    batch
        .columns()
        .iter()
        .map(|a| ArrayFormatter::try_new(a.as_ref(), opts).map_err(arrow_io))
        .collect()
}

/// Render row `r` of a batch to one byte-vector per column.
fn render_row(formatters: &[ArrayFormatter<'_>], r: usize) -> Vec<Vec<u8>> {
    formatters
        .iter()
        .map(|f| f.value(r).to_string().into_bytes())
        .collect()
}

/// A `RowSelection` selecting exactly the rows in `sorted` (ascending, deduped): a skip run for each
/// gap, a select run for each maximal consecutive block. Trailing rows are implicitly skipped.
fn row_selection(sorted: &[u64]) -> RowSelection {
    let mut selectors: Vec<RowSelector> = Vec::new();
    let mut cursor = 0u64;
    let mut i = 0usize;
    while i < sorted.len() {
        let start = sorted[i];
        if start > cursor {
            selectors.push(RowSelector::skip((start - cursor) as usize));
        }
        let mut j = i;
        while j + 1 < sorted.len() && sorted[j + 1] == sorted[j] + 1 {
            j += 1;
        }
        let run = sorted[j] - start + 1;
        selectors.push(RowSelector::select(run as usize));
        cursor = sorted[j] + 1;
        i = j + 1;
    }
    RowSelection::from(selectors)
}

/// Build our viewport [`Schema`] from the Arrow schema: one column per top-level field, named by the
/// field, with a presentational type mapped from the Arrow [`DataType`] (for alignment / sort hints).
fn build_schema(arrow_schema: &SchemaRef) -> Schema {
    let columns = arrow_schema
        .fields()
        .iter()
        .enumerate()
        .map(|(i, field)| Column {
            id: ColumnId(i as u32),
            name: field.name().as_bytes().into(),
            inferred: inferred_type(field.data_type()),
        })
        .collect();
    Schema { columns }
}

/// Map an Arrow [`DataType`] to our presentational [`InferredType`] (alignment + sort-key hint only).
fn inferred_type(dt: &DataType) -> InferredType {
    match dt {
        DataType::Boolean => InferredType::Boolean,
        dt if dt.is_integer() => InferredType::Integer,
        DataType::Float16
        | DataType::Float32
        | DataType::Float64
        | DataType::Decimal128(_, _)
        | DataType::Decimal256(_, _) => InferredType::Float,
        _ => InferredType::Text,
    }
}

fn parquet_io(e: parquet::errors::ParquetError) -> Error {
    Error::Io(std::io::Error::other(e))
}

fn arrow_io(e: arrow::error::ArrowError) -> Error {
    Error::Io(std::io::Error::other(e))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{Direction, FilterSpec};
    use arrow::array::{Int64Array, RecordBatch, StringArray};
    use arrow::datatypes::{Field, Schema as ArrowSchema};
    use parquet::arrow::ArrowWriter;
    use parquet::file::properties::WriterProperties;
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

    /// Write a `name: Utf8, age: Int64` table to a temp Parquet file with small row groups (so
    /// multi-row-group random access is exercised). Returns the temp file (keep it alive).
    fn write_sample(
        names: Vec<Option<&str>>,
        ages: Vec<Option<i64>>,
        row_group_size: usize,
    ) -> tempfile::NamedTempFile {
        let schema = Arc::new(ArrowSchema::new(vec![
            Field::new("name", DataType::Utf8, true),
            Field::new("age", DataType::Int64, true),
        ]));
        let batch = RecordBatch::try_new(
            schema.clone(),
            vec![
                Arc::new(StringArray::from(names)),
                Arc::new(Int64Array::from(ages)),
            ],
        )
        .unwrap();
        let tmp = tempfile::NamedTempFile::new().unwrap();
        let props = WriterProperties::builder()
            .set_max_row_group_row_count(Some(row_group_size))
            .build();
        let file = File::create(tmp.path()).unwrap();
        let mut writer = ArrowWriter::try_new(file, schema, Some(props)).unwrap();
        writer.write(&batch).unwrap();
        writer.close().unwrap();
        tmp
    }

    fn await_view(table: &ParquetTable) {
        for _ in 0..2000 {
            if !table.view_status().building {
                return;
            }
            thread::sleep(Duration::from_millis(1));
        }
        panic!("view did not build in time");
    }

    #[test]
    fn schema_names_and_types_come_from_metadata() {
        let tmp = write_sample(vec![Some("a")], vec![Some(1)], 100);
        let table = ParquetTable::open(tmp.path()).unwrap();
        let cols = &table.schema().columns;
        assert_eq!(cols.len(), 2);
        assert_eq!(cols[0].name.as_ref(), b"name");
        assert_eq!(cols[0].inferred, InferredType::Text);
        assert_eq!(cols[1].name.as_ref(), b"age");
        assert_eq!(cols[1].inferred, InferredType::Integer);
        assert!(matches!(table.row_count(), RowCount::Exact(1)));
    }

    #[test]
    fn fetch_renders_and_projects_in_requested_order() {
        let tmp = write_sample(
            vec![Some("bob"), Some("ann")],
            vec![Some(30), Some(25)],
            100,
        );
        let table = ParquetTable::open(tmp.path()).unwrap();
        // Columns reversed: age, name.
        let viewport = table
            .fetch(&request(0, 2, &[1, 0]), &CancellationToken::new())
            .unwrap();
        assert_eq!(
            cells(&viewport),
            vec![
                vec!["30".to_string(), "bob".to_string()],
                vec!["25".to_string(), "ann".to_string()],
            ]
        );
    }

    #[test]
    fn nulls_render_as_empty() {
        let tmp = write_sample(vec![Some("a"), None], vec![Some(1), None], 100);
        let table = ParquetTable::open(tmp.path()).unwrap();
        let viewport = table
            .fetch(&request(0, 2, &[0, 1]), &CancellationToken::new())
            .unwrap();
        assert_eq!(
            cells(&viewport),
            vec![
                vec!["a".to_string(), "1".to_string()],
                vec![String::new(), String::new()],
            ]
        );
    }

    #[test]
    fn random_access_across_row_groups() {
        let names = (0..5).map(|_| Some("x")).collect();
        let ages = (0..5i64).map(Some).collect();
        let tmp = write_sample(names, ages, 2); // 3 row groups: [0,1] [2,3] [4]
        let table = ParquetTable::open(tmp.path()).unwrap();
        assert!(matches!(table.row_count(), RowCount::Exact(5)));
        // Row 4 lives in the third row group; reading it must skip the first two.
        let viewport = table
            .fetch(&request(4, 1, &[1]), &CancellationToken::new())
            .unwrap();
        assert_eq!(cells(&viewport), vec![vec!["4".to_string()]]);
    }

    #[test]
    fn global_filter_hides_non_matching_rows() {
        let tmp = write_sample(
            vec![Some("apple"), Some("banana"), Some("avocado")],
            vec![Some(1), Some(2), Some(3)],
            2,
        );
        let table = ParquetTable::open(tmp.path()).unwrap();
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
        let tmp = write_sample(
            vec![Some("c"), Some("a"), Some("b")],
            vec![Some(3), Some(1), Some(2)],
            2,
        );
        let table = ParquetTable::open(tmp.path()).unwrap();
        table
            .set_view(&ViewSpec {
                filter: None,
                sort: Some(SortKey {
                    column: ColumnId(1), // age
                    direction: Direction::Ascending,
                }),
            })
            .unwrap();
        await_view(&table);

        let viewport = table
            .fetch(&request(0, 3, &[0, 1]), &CancellationToken::new())
            .unwrap();
        assert_eq!(
            cells(&viewport),
            vec![
                vec!["a".to_string(), "1".to_string()],
                vec!["b".to_string(), "2".to_string()],
                vec!["c".to_string(), "3".to_string()],
            ]
        );
    }
}
