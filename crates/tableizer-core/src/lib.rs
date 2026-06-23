//! Tableizer core engine: byte-faithful parsing, out-of-core indexing, streaming search, and
//! external sort over very large (multi-TB) tabular files. UI-agnostic.
//!
//! The load-bearing seam between the engine and any GUI is [`ViewportSource`]: the UI only ever
//! asks for a small, already-materialised slice of a logical table, so the grid stays a thin,
//! swappable layer (see `docs/spec.md` §4). The modules here are currently design-bearing stubs;
//! each carries the decisions it must honour when implemented.

pub mod cache;
pub mod error;
pub mod index;
pub mod parse;
pub mod search;
pub mod sort;
pub mod table;
pub mod viewport;

mod cancel;

pub use cancel::CancellationToken;
pub use error::{Error, Result};
pub use index::Progress;
pub use table::CsvTable;
pub use viewport::{
    Cell, Column, ColumnId, InferredType, RowCount, RowRange, Schema, Viewport, ViewportRequest,
    ViewportSource,
};
