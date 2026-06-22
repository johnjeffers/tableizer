//! Column sort (`docs/spec.md` §3.4).
//!
//! Page-local sort (Tier A, the default) reorders only the visible page and is explicitly labelled
//! as such. Global sort (Tier C) is a distinct, named, async job that builds and persists a sort
//! permutation of `(key, rownum)` pairs via a *bespoke* external merge sort (rayon run generation +
//! k-way disk-spill merge) — deliberately NOT delegated to DataFusion/Polars, whose spill has
//! documented pathologies at multi-TB scale.

use crate::viewport::ColumnId;

/// Sort direction.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Default)]
pub enum Direction {
    /// Smallest first.
    #[default]
    Ascending,
    /// Largest first.
    Descending,
}

/// How far a sort reaches — the instant default vs the heavyweight global job. Surfacing this in the
/// UI prevents a single click from silently launching a multi-hour, multi-TB-write operation.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Default)]
pub enum Scope {
    /// Reorder only the currently rendered page (Tier A, instant — but only sorts what is visible).
    #[default]
    PageLocal,
    /// Reorder the entire dataset (Tier C, async, persisted permutation).
    Global,
}

/// A request to sort by a column.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct SortKey {
    /// Column to sort by.
    pub column: ColumnId,
    /// Ascending or descending.
    pub direction: Direction,
    /// Page-local or global.
    pub scope: Scope,
}
