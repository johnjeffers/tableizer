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

use std::path::Path;

/// A stable, version-independent hash of a path, for naming on-disk derived artifacts (the index
/// cache, saved per-file views). `std`'s `DefaultHasher` is explicitly **not** guaranteed stable
/// across toolchain versions, so a fixed FNV-1a over the path bytes is used instead — the filename a
/// given source maps to never shifts under a compiler upgrade (which would orphan every cache/view).
pub fn stable_hash(path: &Path) -> u64 {
    const FNV_OFFSET: u64 = 0xcbf2_9ce4_8422_2325;
    const FNV_PRIME: u64 = 0x0000_0100_0000_01b3;
    let mut hash = FNV_OFFSET;
    for &byte in path.to_string_lossy().as_bytes() {
        hash ^= u64::from(byte);
        hash = hash.wrapping_mul(FNV_PRIME);
    }
    hash
}

#[cfg(test)]
mod tests {
    use super::stable_hash;
    use std::path::Path;

    #[test]
    fn stable_hash_is_a_pinned_fnv1a_so_the_on_disk_layout_never_drifts() {
        // A fixed vector: if this value ever changes, every persisted cache/view would be orphaned —
        // so the change must be deliberate (and the magic/version bumped), not accidental.
        assert_eq!(
            stable_hash(Path::new("/data/sales.csv")),
            0x83b5_7822_59db_9067
        );
        // Deterministic, and distinct paths don't collide on this trivial pair.
        assert_eq!(
            stable_hash(Path::new("/data/sales.csv")),
            stable_hash(Path::new("/data/sales.csv"))
        );
        assert_ne!(
            stable_hash(Path::new("/a.csv")),
            stable_hash(Path::new("/b.csv"))
        );
    }
}
