//! Row-offset index — maps a 0-based record index to the byte offset where that record begins, so a
//! viewport can seek directly to any row (`docs/architecture.md`).
//!
//! **Sparse:** one anchor byte-offset is stored every [`ANCHOR_INTERVAL`] records (not one per row),
//! so the index stays small on huge files. [`OffsetIndex::offset_of_row`] seeks to the nearest
//! preceding anchor and re-parses forward at most `ANCHOR_INTERVAL` records. Record boundaries are
//! found with the `csv` crate (never hand-rolled), so an embedded newline inside a quoted field is
//! correctly *not* a boundary. Anchors sit at record boundaries — always outside any quoted field —
//! so a fresh parse resumes from one without tracking quote state.
//!
//! The §4.1 byte-window + stored-quote-parity variant additionally bounds *lookup latency* regardless
//! of row length; it is a later refinement. This record-interval form is the Phase 0 implementation.

use crate::{CancellationToken, Error, Result, parse::Dialect};

/// Records between stored anchors. Lookup re-parses at most this many records from an anchor.
const ANCHOR_INTERVAL: u64 = 1024;

/// Progress reported during an index build.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Progress {
    /// Byte position reached in the source.
    pub bytes_processed: u64,
    /// Records indexed so far (the honest growing row count during a background build).
    pub rows_indexed: u64,
}

/// A row → byte-offset index over a delimited source.
///
/// Holds only sparse anchors (every `ANCHOR_INTERVAL` records) plus the total row count, so its size
/// is `O(rows / ANCHOR_INTERVAL)`. It does not own the source bytes; callers pass them back to
/// [`offset_of_row`](Self::offset_of_row) (the [`crate::table::CsvTable`] that owns the source/mmap
/// drives this).
pub struct OffsetIndex {
    /// `anchors[k]` is the byte offset of record `k * ANCHOR_INTERVAL`.
    anchors: Vec<u64>,
    row_count: u64,
    /// Records whose field count differs from the first record's (a data-quality signal).
    ragged_rows: u64,
    delimiter: u8,
    quote: u8,
}

impl OffsetIndex {
    /// Build an index over `bytes`, parsing records with `dialect`.
    pub fn build(bytes: &[u8], dialect: &Dialect) -> Result<Self> {
        Self::build_with(bytes, dialect, &CancellationToken::new(), |_| {})
    }

    /// Build an index, checking `cancel` periodically and reporting [`Progress`] (bytes + rows).
    ///
    /// Returns [`Error::Cancelled`] if cancellation is requested before completion. `progress` is
    /// called with monotonically non-decreasing positions and finally with the totals — this drives
    /// the honest growing row count during a background build (`CsvTable`).
    pub fn build_with(
        bytes: &[u8],
        dialect: &Dialect,
        cancel: &CancellationToken,
        mut progress: impl FnMut(Progress),
    ) -> Result<Self> {
        let mut reader = record_reader(bytes, dialect);
        let mut anchors = Vec::new();
        let mut record = csv::ByteRecord::new();
        let mut row_count: u64 = 0;
        let mut ragged_rows: u64 = 0;
        let mut expected_cols: Option<usize> = None;

        while reader.read_byte_record(&mut record).map_err(parse_io)? {
            match expected_cols {
                None => expected_cols = Some(record.len()),
                Some(cols) if record.len() != cols => ragged_rows += 1,
                _ => {}
            }
            if row_count.is_multiple_of(ANCHOR_INTERVAL) {
                anchors.push(record_offset(&record));
            }
            row_count += 1;

            // Cancellation and progress are checked on a coarse cadence to keep the hot loop tight.
            if row_count.is_multiple_of(4096) {
                if cancel.is_cancelled() {
                    return Err(Error::Cancelled);
                }
                progress(Progress {
                    bytes_processed: record_offset(&record),
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

        Ok(Self {
            anchors,
            row_count,
            ragged_rows,
            delimiter: dialect.delimiter,
            quote: dialect.quote,
        })
    }

    /// Total number of records indexed.
    pub fn row_count(&self) -> u64 {
        self.row_count
    }

    /// Records whose field count differed from the first record's (a data-quality signal).
    pub fn ragged_rows(&self) -> u64 {
        self.ragged_rows
    }

    /// Byte offset where record `row` begins, or `None` if `row` is out of range.
    ///
    /// Seeks to the nearest preceding anchor and re-parses forward, so `bytes` must be the same
    /// source the index was built over.
    pub fn offset_of_row(&self, bytes: &[u8], row: u64) -> Option<u64> {
        if row >= self.row_count {
            return None;
        }
        let anchor_offset = self.anchors[(row / ANCHOR_INTERVAL) as usize];
        let skip = row % ANCHOR_INTERVAL;
        if skip == 0 {
            return Some(anchor_offset);
        }

        let dialect = Dialect {
            delimiter: self.delimiter,
            quote: self.quote,
            has_header: false,
        };
        let mut reader = record_reader(&bytes[anchor_offset as usize..], &dialect);
        let mut record = csv::ByteRecord::new();
        let mut idx = 0u64;
        while reader.read_byte_record(&mut record).ok()? {
            if idx == skip {
                return Some(anchor_offset + record_offset(&record));
            }
            idx += 1;
        }
        None
    }

    /// Serialize the index to a compact little-endian blob for the on-disk cache (§4.7).
    pub(crate) fn serialize(&self) -> Vec<u8> {
        let mut out = Vec::with_capacity(26 + self.anchors.len() * 8);
        out.extend_from_slice(INDEX_MAGIC);
        out.extend_from_slice(&INDEX_VERSION.to_le_bytes());
        out.push(self.delimiter);
        out.push(self.quote);
        out.extend_from_slice(&self.row_count.to_le_bytes());
        out.extend_from_slice(&self.ragged_rows.to_le_bytes());
        out.extend_from_slice(&(self.anchors.len() as u64).to_le_bytes());
        for &anchor in &self.anchors {
            out.extend_from_slice(&anchor.to_le_bytes());
        }
        out
    }

    /// Reconstruct an index from [`serialize`](Self::serialize) output, or `None` if the blob is not
    /// a recognised, intact index (bad magic/version, or truncated). Corrupt anchor counts fail
    /// safely: anchors are only pushed as they are successfully read.
    pub(crate) fn deserialize(bytes: &[u8]) -> Option<Self> {
        let mut pos = 0;
        if read_bytes(bytes, &mut pos, 4)? != INDEX_MAGIC {
            return None;
        }
        if read_u32(bytes, &mut pos)? != INDEX_VERSION {
            return None;
        }
        let delimiter = read_u8(bytes, &mut pos)?;
        let quote = read_u8(bytes, &mut pos)?;
        let row_count = read_u64(bytes, &mut pos)?;
        let ragged_rows = read_u64(bytes, &mut pos)?;
        let anchor_count = read_u64(bytes, &mut pos)?;
        let mut anchors = Vec::new();
        for _ in 0..anchor_count {
            anchors.push(read_u64(bytes, &mut pos)?);
        }
        Some(Self {
            anchors,
            row_count,
            ragged_rows,
            delimiter,
            quote,
        })
    }
}

/// A `csv` reader configured to surface every physical record (no header skipping) and to treat
/// ragged rows as data rather than errors (malformed-row resilience; `docs/formats.md`).
pub(crate) fn record_reader<'a>(bytes: &'a [u8], dialect: &Dialect) -> csv::Reader<&'a [u8]> {
    csv::ReaderBuilder::new()
        .delimiter(dialect.delimiter)
        .quote(dialect.quote)
        .has_headers(false)
        .flexible(true)
        .from_reader(bytes)
}

/// Byte offset of a just-read record. A record read from a reader always carries a position.
fn record_offset(record: &csv::ByteRecord) -> u64 {
    record
        .position()
        .expect("a record read from a reader always carries a byte position")
        .byte()
}

/// Map a `csv` read error into our I/O error. With in-memory/`flexible` reads these are effectively
/// unreachable; malformed-row *content* is not an error here.
fn parse_io(e: csv::Error) -> Error {
    Error::Io(std::io::Error::other(e))
}

/// On-disk index format magic + version (bumped on any layout change to invalidate old caches).
const INDEX_MAGIC: &[u8; 4] = b"TZIX";
const INDEX_VERSION: u32 = 2;

/// Little-endian read helpers for the cache format (shared with [`crate::cache`]). Each returns
/// `None` (rather than panicking) when the buffer is too short — corrupt caches fail safely.
pub(crate) fn read_bytes<'a>(bytes: &'a [u8], pos: &mut usize, n: usize) -> Option<&'a [u8]> {
    let end = pos.checked_add(n)?;
    let slice = bytes.get(*pos..end)?;
    *pos = end;
    Some(slice)
}

pub(crate) fn read_u8(bytes: &[u8], pos: &mut usize) -> Option<u8> {
    Some(read_bytes(bytes, pos, 1)?[0])
}

pub(crate) fn read_u32(bytes: &[u8], pos: &mut usize) -> Option<u32> {
    Some(u32::from_le_bytes(
        read_bytes(bytes, pos, 4)?.try_into().ok()?,
    ))
}

pub(crate) fn read_u64(bytes: &[u8], pos: &mut usize) -> Option<u64> {
    Some(u64::from_le_bytes(
        read_bytes(bytes, pos, 8)?.try_into().ok()?,
    ))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reports_the_byte_offset_of_each_record() {
        // Two simple records: "a,b\n" (bytes 0..4) then "c,d\n" (bytes 4..8).
        let csv: &[u8] = b"a,b\nc,d\n";
        let index = OffsetIndex::build(csv, &Dialect::default()).unwrap();

        assert_eq!(index.row_count(), 2);
        assert_eq!(index.offset_of_row(csv, 0), Some(0));
        assert_eq!(index.offset_of_row(csv, 1), Some(4));
        assert_eq!(index.offset_of_row(csv, 2), None);
    }

    #[test]
    fn embedded_newline_inside_quotes_is_not_a_record_boundary() {
        // Record 0 is `"x\ny",1` — the newline at byte 2 is *inside* the quoted field, so it must
        // NOT start a new record; record 1 (`2,3`) begins at byte 8. Naive newline-counting would
        // wrongly see a boundary at byte 3 and report 3 records.
        let csv: &[u8] = b"\"x\ny\",1\n2,3\n";
        let index = OffsetIndex::build(csv, &Dialect::default()).unwrap();

        assert_eq!(index.row_count(), 2);
        assert_eq!(index.offset_of_row(csv, 1), Some(8));
    }

    #[test]
    fn resolves_offsets_across_many_anchors_including_quoted_newlines() {
        // More records than ANCHOR_INTERVAL forces multiple anchors; a quoted embedded newline
        // partway through proves sparse lookup stays quote-aware across anchor boundaries.
        let mut data = Vec::new();
        let mut expected = Vec::new();
        for i in 0..3000u32 {
            expected.push(data.len() as u64);
            if i == 1500 {
                data.extend_from_slice(b"\"emb\nedded\",x\n");
            } else {
                data.extend_from_slice(format!("row{i},val{i}\n").as_bytes());
            }
        }

        let index = OffsetIndex::build(&data, &Dialect::default()).unwrap();

        assert_eq!(index.row_count(), 3000);
        for &row in &[0u64, 1, 1023, 1024, 1025, 1499, 1500, 1501, 2999] {
            assert_eq!(
                index.offset_of_row(&data, row),
                Some(expected[row as usize]),
                "row {row}"
            );
        }
        assert_eq!(index.offset_of_row(&data, 3000), None);
    }

    #[test]
    fn build_is_cancellable() {
        let cancel = CancellationToken::new();
        cancel.cancel();

        let result = OffsetIndex::build_with(b"a,b\nc,d\n", &Dialect::default(), &cancel, |_| {});

        assert!(matches!(result, Err(Error::Cancelled)));
    }

    #[test]
    fn build_reports_non_decreasing_progress_ending_at_total_bytes() {
        let mut data = Vec::new();
        for i in 0..10_000u32 {
            data.extend_from_slice(format!("r{i}\n").as_bytes());
        }
        let cancel = CancellationToken::new();
        let mut seen = Vec::new();

        let index =
            OffsetIndex::build_with(&data, &Dialect::default(), &cancel, |p| seen.push(p)).unwrap();

        assert_eq!(index.row_count(), 10_000);
        assert!(
            !seen.is_empty(),
            "progress should be reported on a large file"
        );
        let last = seen.last().unwrap();
        assert_eq!(last.rows_indexed, 10_000, "final progress reports all rows");
        assert_eq!(
            last.bytes_processed,
            data.len() as u64,
            "final progress is total bytes"
        );
        assert!(
            seen.windows(2)
                .all(|w| w[0].bytes_processed <= w[1].bytes_processed),
            "progress must be non-decreasing"
        );
    }

    #[test]
    fn index_round_trips_through_serialization() {
        let csv: &[u8] = b"a,b\nc,d\ne,f\n";
        let index = OffsetIndex::build(csv, &Dialect::default()).unwrap();

        let restored = OffsetIndex::deserialize(&index.serialize()).expect("valid blob");

        assert_eq!(restored.row_count(), index.row_count());
        for row in 0..index.row_count() {
            assert_eq!(
                restored.offset_of_row(csv, row),
                index.offset_of_row(csv, row),
                "row {row}"
            );
        }
    }

    #[test]
    fn deserialize_rejects_a_bad_blob() {
        assert!(OffsetIndex::deserialize(b"not an index at all").is_none());
        assert!(OffsetIndex::deserialize(&[]).is_none());
    }

    #[test]
    fn counts_ragged_rows_and_round_trips_them() {
        // Record 1 ("c,d,e") has 3 fields vs the first record's 2 → one ragged row.
        let csv: &[u8] = b"a,b\nc,d,e\nf,g\n";
        let index = OffsetIndex::build(csv, &Dialect::default()).unwrap();
        assert_eq!(index.row_count(), 3);
        assert_eq!(index.ragged_rows(), 1);

        let restored = OffsetIndex::deserialize(&index.serialize()).expect("valid blob");
        assert_eq!(restored.ragged_rows(), 1);
    }
}
