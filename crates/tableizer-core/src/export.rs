//! Export the current view or the source to CSV/TSV (`docs/formats.md`).
//!
//! Two same-family modes: **current view** (active filter + sort, the given visible columns in
//! display order — what the user sees) and **source** (every row/column in source order, ignoring the
//! view). Quoting is delegated to the `csv` writer so the output re-imports to identical fields — the
//! round-trip is covered by a test.

use std::io::Write;

use crate::{
    CancellationToken, ColumnId, Error, Result, RowCount, RowRange, ViewportRequest, ViewportSource,
};

/// Which rows the export draws from.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ExportScope {
    /// What the user sees: the active filter + sort, over the given (visible, ordered) columns.
    CurrentView,
    /// The source faithfully: every row in source order, ignoring any filter/sort.
    Source,
}

/// Write `table` to `writer` as delimited text. `columns` are exported in order; `header`, if given,
/// is written first. Rows are pulled in batches and the export honours `cancel`.
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
        for row in &viewport.rows {
            writer
                .write_record(row.iter().map(|cell| cell.0.as_ref()))
                .map_err(csv_io)?;
        }
        start += viewport.rows.len() as u64;
    }
    writer.flush()?;
    Ok(())
}

fn csv_io(error: csv::Error) -> Error {
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
}
