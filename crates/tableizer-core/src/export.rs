//! Export the current view or the source to CSV / TSV, NDJSON, or Parquet (`docs/formats.md`).
//!
//! Two row modes: **current view** (active filter + sort, the given visible columns in display order
//! — what the user sees) and **source** (every row/column in source order, ignoring the view). By the
//! time export runs, cells are already *rendered text*, so values are written as their **displayed
//! text** with no type coercion — the exact bytes survive (CSV re-imports to identical fields, covered
//! by a round-trip test; NDJSON/Parquet write every value as a string, so a numeric-looking id keeps
//! its leading zeros). Typed NDJSON/Parquet export with documented coercion rules is a future
//! refinement (`docs/todo.md`).

use std::io::Write;
use std::sync::Arc;

use arrow::array::{ArrayRef, StringArray};
use arrow::datatypes::{DataType, Field, Schema as ArrowSchema};
use arrow::record_batch::RecordBatch;
use parquet::arrow::ArrowWriter;

use crate::{
    CancellationToken, Cell, ColumnId, Error, Result, RowCount, RowRange, ViewportRequest,
    ViewportSource,
};

/// Which rows the export draws from.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ExportScope {
    /// What the user sees: the active filter + sort, over the given (visible, ordered) columns.
    CurrentView,
    /// The source faithfully: every row in source order, ignoring any filter/sort.
    Source,
}

/// Pull the export's rows in batches (honouring `scope` and `cancel`), invoking `on_batch` for each
/// non-empty batch of materialised rows. The single fetch loop shared by every export format.
fn for_each_batch(
    table: &dyn ViewportSource,
    scope: ExportScope,
    columns: &[ColumnId],
    cancel: &CancellationToken,
    mut on_batch: impl FnMut(&[Vec<Cell>]) -> Result<()>,
) -> Result<()> {
    let total = match table.row_count() {
        RowCount::Exact(n) | RowCount::AtLeast(n) => n,
    };
    const BATCH: u32 = 1024;
    let mut start = 0u64;
    while start < total {
        if cancel.is_cancelled() {
            return Err(Error::Cancelled);
        }
        let len = BATCH.min(u32::try_from(total - start).unwrap_or(BATCH));
        let request = ViewportRequest {
            rows: RowRange { start, len },
            columns: columns.to_vec(),
        };
        let viewport = match scope {
            ExportScope::CurrentView => table.fetch(&request, cancel)?,
            ExportScope::Source => table.fetch_source(&request, cancel)?,
        };
        if viewport.rows.is_empty() {
            break;
        }
        on_batch(&viewport.rows)?;
        start += viewport.rows.len() as u64;
    }
    Ok(())
}

/// Write `table` to `writer` as delimited text. `columns` are exported in order; `header`, if given,
/// is written first. Honours `cancel`.
pub fn export_csv<W: Write>(
    table: &dyn ViewportSource,
    writer: W,
    delimiter: u8,
    scope: ExportScope,
    columns: &[ColumnId],
    header: Option<&[Vec<u8>]>,
    cancel: &CancellationToken,
) -> Result<()> {
    let mut writer = csv::WriterBuilder::new()
        .delimiter(delimiter)
        .from_writer(writer);
    if let Some(header) = header {
        writer
            .write_record(header.iter().map(Vec::as_slice))
            .map_err(csv_io)?;
    }
    for_each_batch(table, scope, columns, cancel, |rows| {
        for row in rows {
            writer
                .write_record(row.iter().map(|cell| cell.0.as_ref()))
                .map_err(csv_io)?;
        }
        Ok(())
    })?;
    writer.flush()?;
    Ok(())
}

/// Write `table` as NDJSON — one JSON object per row, `names` as the keys (aligned with `columns`).
/// Values are the cells' displayed text written as JSON strings (no type coercion).
pub fn export_ndjson<W: Write>(
    table: &dyn ViewportSource,
    mut writer: W,
    scope: ExportScope,
    columns: &[ColumnId],
    names: &[Vec<u8>],
    cancel: &CancellationToken,
) -> Result<()> {
    let keys: Vec<String> = names
        .iter()
        .enumerate()
        .map(|(i, n)| field_name(n, i))
        .collect();
    for_each_batch(table, scope, columns, cancel, |rows| {
        for row in rows {
            // `serde_json::Map` keeps insertion (column) order — the crate has `preserve_order` on.
            let mut obj = serde_json::Map::with_capacity(row.len());
            for (i, cell) in row.iter().enumerate() {
                let key = keys.get(i).cloned().unwrap_or_else(|| format!("col{i}"));
                let value = String::from_utf8_lossy(&cell.0).into_owned();
                obj.insert(key, serde_json::Value::String(value));
            }
            serde_json::to_writer(&mut writer, &serde_json::Value::Object(obj)).map_err(json_io)?;
            writer.write_all(b"\n")?;
        }
        Ok(())
    })?;
    writer.flush()?;
    Ok(())
}

/// Write `table` as Parquet. Every column is a UTF-8 string of the cell's displayed text (no type
/// coercion); `names` are the column names (aligned with `columns`).
pub fn export_parquet<W: Write + Send>(
    table: &dyn ViewportSource,
    writer: W,
    scope: ExportScope,
    columns: &[ColumnId],
    names: &[Vec<u8>],
    cancel: &CancellationToken,
) -> Result<()> {
    let fields: Vec<Field> = names
        .iter()
        .enumerate()
        .map(|(i, n)| Field::new(field_name(n, i), DataType::Utf8, true))
        .collect();
    let schema = Arc::new(ArrowSchema::new(fields));
    let mut parquet = ArrowWriter::try_new(writer, schema.clone(), None).map_err(parquet_io)?;
    let ncols = columns.len();
    for_each_batch(table, scope, columns, cancel, |rows| {
        let mut cols: Vec<Vec<Option<String>>> = vec![Vec::with_capacity(rows.len()); ncols];
        for row in rows {
            for (c, col) in cols.iter_mut().enumerate() {
                col.push(
                    row.get(c)
                        .map(|cell| String::from_utf8_lossy(&cell.0).into_owned()),
                );
            }
        }
        let arrays: Vec<ArrayRef> = cols
            .into_iter()
            .map(|vals| Arc::new(StringArray::from_iter(vals)) as ArrayRef)
            .collect();
        let batch = RecordBatch::try_new(schema.clone(), arrays).map_err(arrow_io)?;
        parquet.write(&batch).map_err(parquet_io)?;
        Ok(())
    })?;
    parquet.close().map_err(parquet_io)?;
    Ok(())
}

/// A non-empty column name, falling back to a positional `col{i}` for an empty/headerless column.
fn field_name(name: &[u8], index: usize) -> String {
    if name.is_empty() {
        format!("col{index}")
    } else {
        String::from_utf8_lossy(name).into_owned()
    }
}

fn csv_io(error: csv::Error) -> Error {
    Error::Io(std::io::Error::other(error))
}

fn json_io(error: serde_json::Error) -> Error {
    Error::Io(std::io::Error::other(error))
}

fn parquet_io(error: parquet::errors::ParquetError) -> Error {
    Error::Io(std::io::Error::other(error))
}

fn arrow_io(error: arrow::error::ArrowError) -> Error {
    Error::Io(std::io::Error::other(error))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::CsvTable;
    use crate::Viewport;
    use crate::parse::Dialect;

    fn no_header() -> Dialect {
        Dialect {
            has_header: false,
            ..Dialect::default()
        }
    }

    fn cells(viewport: &Viewport) -> Vec<Vec<Vec<u8>>> {
        viewport
            .rows
            .iter()
            .map(|row| row.iter().map(|cell| cell.0.to_vec()).collect())
            .collect()
    }

    #[test]
    fn export_round_trips_through_reimport() {
        // Includes a quoted field containing the delimiter — the writer must re-quote it.
        let original = b"a,b\n\"x,y\",2\n3,4\n".to_vec();
        let table = CsvTable::from_bytes(original, no_header()).unwrap();
        let columns = [ColumnId(0), ColumnId(1)];

        let mut out = Vec::new();
        export_csv(
            &table,
            &mut out,
            b',',
            ExportScope::Source,
            &columns,
            None,
            &CancellationToken::new(),
        )
        .unwrap();

        let reimported = CsvTable::from_bytes(out, no_header()).unwrap();
        let request = ViewportRequest {
            rows: RowRange { start: 0, len: 10 },
            columns: columns.to_vec(),
        };
        let before = table.fetch(&request, &CancellationToken::new()).unwrap();
        let after = reimported
            .fetch(&request, &CancellationToken::new())
            .unwrap();
        assert_eq!(cells(&before), cells(&after));
    }

    #[test]
    fn export_ndjson_writes_one_text_object_per_row() {
        let table =
            CsvTable::from_bytes(b"name,age\nbob,30\nann,25\n".to_vec(), Dialect::default())
                .unwrap();
        let columns = [ColumnId(0), ColumnId(1)];
        let names = [b"name".to_vec(), b"age".to_vec()];

        let mut out = Vec::new();
        export_ndjson(
            &table,
            &mut out,
            ExportScope::Source,
            &columns,
            &names,
            &CancellationToken::new(),
        )
        .unwrap();

        // Keys in column order; values as their displayed text (strings).
        assert_eq!(
            String::from_utf8(out).unwrap(),
            "{\"name\":\"bob\",\"age\":\"30\"}\n{\"name\":\"ann\",\"age\":\"25\"}\n"
        );
    }

    #[test]
    fn export_parquet_round_trips_through_the_reader() {
        let table =
            CsvTable::from_bytes(b"name,age\nbob,30\nann,25\n".to_vec(), Dialect::default())
                .unwrap();
        let columns = [ColumnId(0), ColumnId(1)];
        let names = [b"name".to_vec(), b"age".to_vec()];

        let tmp = tempfile::NamedTempFile::new().unwrap();
        export_parquet(
            &table,
            std::fs::File::create(tmp.path()).unwrap(),
            ExportScope::Source,
            &columns,
            &names,
            &CancellationToken::new(),
        )
        .unwrap();

        let reimported = crate::ParquetTable::open(tmp.path()).unwrap();
        assert_eq!(reimported.schema().columns[0].name.as_ref(), b"name");
        let viewport = reimported
            .fetch(
                &ViewportRequest {
                    rows: RowRange { start: 0, len: 2 },
                    columns: columns.to_vec(),
                },
                &CancellationToken::new(),
            )
            .unwrap();
        assert_eq!(cells(&viewport)[0], vec![b"bob".to_vec(), b"30".to_vec()]);
        assert_eq!(cells(&viewport)[1], vec![b"ann".to_vec(), b"25".to_vec()]);
    }
}
