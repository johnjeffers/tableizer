//! The seam between the engine and any GUI.
//!
//! The UI only ever asks for a small slice of a logical table via [`ViewportSource`]; the engine
//! decides how to satisfy it (offset-index seek, sort permutation, filtered result list, ...).
//! Keeping the UI confined to this trait is what makes the grid a swappable layer (`docs/spec.md`
//! §4.5). Note the byte-fidelity guarantee on [`Cell`].

use crate::{CancellationToken, Result};

/// Stable identifier for a column in *source* order, independent of display order.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct ColumnId(pub u32);

/// A single cell value, held as the **exact source bytes**.
///
/// Type inference ([`InferredType`]) is presentational only and never mutates these bytes — this is
/// the byte-fidelity guarantee that keeps display, search, and export faithful to the source.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Cell(pub Box<[u8]>);

/// A presentational type hint used for alignment, sort keys, and formatting. Never authoritative
/// over the raw bytes in [`Cell`].
#[derive(Clone, Copy, Debug, PartialEq, Eq, Default)]
pub enum InferredType {
    /// Default: treated as opaque text (also the fallback for anything ambiguous or over-precise).
    #[default]
    Text,
    /// Looks like an integer (for right-alignment and numeric sort).
    Integer,
    /// Looks like a floating-point number.
    Float,
    /// Looks like a boolean.
    Boolean,
    // Date/time intentionally omitted until the inference + formatting policy is specified.
}

/// One column's metadata. `name` holds the raw header bytes (or a synthetic name when headerless).
#[derive(Clone, Debug)]
pub struct Column {
    /// Stable source-order identifier.
    pub id: ColumnId,
    /// Raw header bytes, preserved exactly (byte fidelity extends to headers).
    pub name: Box<[u8]>,
    /// Presentational type hint.
    pub inferred: InferredType,
}

/// The logical schema of a table.
#[derive(Clone, Debug, Default)]
pub struct Schema {
    /// Columns in source order.
    pub columns: Vec<Column>,
}

/// Total row count — exact once the offset-index build completes, otherwise a growing lower bound
/// while it is still running (progressive availability; see `docs/spec.md` §4.1).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum RowCount {
    /// Indexing finished; this is the exact total.
    Exact(u64),
    /// Indexing in progress; at least this many rows are addressable so far.
    AtLeast(u64),
}

/// A contiguous range of rows in the *active view* (post-filter, post-sort).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct RowRange {
    /// First row index (0-based) within the active view.
    pub start: u64,
    /// Number of rows requested.
    pub len: u32,
}

/// A request for one viewport slice: a row range projected to a set of columns in display order.
#[derive(Clone, Debug)]
pub struct ViewportRequest {
    /// Rows to fetch.
    pub rows: RowRange,
    /// Columns to project, in display order.
    pub columns: Vec<ColumnId>,
}

/// A materialised viewport slice handed to the UI. Row-oriented and small (bounded by the visible
/// area), so it crosses a channel cheaply. `rows[r][c]` is the cell for requested row `r`,
/// projected column `c`.
#[derive(Clone, Debug, Default)]
pub struct Viewport {
    /// Materialised cells, row-major, matching the request's row range and column projection.
    pub rows: Vec<Vec<Cell>>,
}

/// The single seam between the engine and any GUI.
pub trait ViewportSource {
    /// The table's schema (columns, header names, inferred types).
    fn schema(&self) -> &Schema;

    /// Current row count of the active view (may be [`RowCount::AtLeast`] while indexing).
    fn row_count(&self) -> RowCount;

    /// Fetch one viewport slice. Must be cheap (Tier A) once the offset index exists, and must
    /// honour `cancel` so a fast-scrolling UI can abandon in-flight requests.
    fn fetch(&self, request: &ViewportRequest, cancel: &CancellationToken) -> Result<Viewport>;

    /// A coarse data-quality summary surfaced to the user (ragged rows, …). Defaults to empty/unknown
    /// so formats that don't track it need not implement it.
    fn data_quality(&self) -> DataQuality {
        DataQuality::default()
    }
}

/// Coarse data-quality summary for the open table (spec §3.1 / §5).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct DataQuality {
    /// Records whose field count differs from the first row, once known.
    pub ragged_rows: u64,
    /// Whether the figures are final (the index finished building).
    pub complete: bool,
}
