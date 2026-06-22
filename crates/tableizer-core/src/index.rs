//! Sparse, persisted, quote-aware row-offset index — the foundational artifact (`docs/spec.md` §4.1).
//!
//! Built by a single quote-aware streaming pass (cancellable, resumable). Stores one [`Anchor`] per
//! fixed byte window, so any anchor is self-resolvable and page-N lookup is a binary search plus a
//! bounded forward parse — never the silently-wrong seek-then-resync heuristic. Persisted in the OS
//! state dir (never beside the source), keyed by `{path, size, mtime, content hash, dialect}` and
//! validated on open; on mismatch the user is prompted to re-index.

/// One sparse anchor: enough state to resume *correct* parsing from a byte boundary.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Anchor {
    /// Byte offset into the source file where this anchor's window begins.
    pub byte_offset: u64,
    /// Number of complete records before this anchor (for row → anchor binary search).
    pub cumulative_records: u64,
    /// Whether the parser is *inside a quoted field* at `byte_offset`. Storing this is what makes
    /// resync decidable: without it, a newline mid-file cannot be classified as a record separator.
    pub in_quoted_field: bool,
}
