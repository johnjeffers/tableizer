//! Tableizer core engine: byte-faithful parsing, out-of-core indexing, streaming search, and
//! external sort over very large (multi-TB) tabular files. UI-agnostic.
//!
//! The load-bearing seam between the engine and any GUI is [`ViewportSource`]: the UI only ever
//! asks for a small, already-materialised slice of a logical table, so the grid stays a thin,
//! swappable layer (see `docs/architecture.md`). Each module carries the decision it encodes; the
//! format readers (`table`/`json`/`parquet`) are described in `docs/formats.md`.

pub mod cache;
pub mod error;
pub mod export;
pub mod index;
pub mod json;
pub mod parquet;
pub mod parse;
pub mod search;
pub mod sort;
pub mod table;
pub mod viewport;

mod cancel;
mod source;

pub use cancel::CancellationToken;
pub use error::{Error, Result};
pub use export::ExportScope;
pub use index::Progress;
pub use json::{JsonMode, JsonTable};
pub use parquet::ParquetTable;
pub use search::FilterSpec;
pub use sort::{Direction, SortKey};
pub use table::CsvTable;
pub use viewport::{
    Cell, Column, ColumnId, DataQuality, InferredType, RowCount, RowRange, Schema, ViewSpec,
    ViewStatus, Viewport, ViewportRequest, ViewportSource,
};
