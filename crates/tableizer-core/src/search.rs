//! Streaming search engine: literal, substring, and regex over a single cancellable scan
//! (`docs/spec.md` §3.3).
//!
//! Regex MUST be linear-time / ReDoS-safe — the `regex` crate, never a backtracking engine — since
//! patterns are user-supplied. Invert search is the complement of the match predicate. Highlight is
//! Tier A (in place over the paginated view); hide-non-matching is Tier C (a virtualised result list).

/// What to search for.
#[derive(Clone, Debug)]
pub enum Pattern {
    /// A literal byte substring (accelerated with `memchr` / `aho-corasick` when implemented).
    Literal(Vec<u8>),
    /// A regular-expression source string (compiled with the linear-time `regex` engine).
    Regex(String),
}

/// How matches affect the view.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Default)]
pub enum SearchMode {
    /// Highlight matches in place over the normal paginated view (Tier A).
    #[default]
    Highlight,
    /// Hide rows that do not match — a virtualised result list (Tier C).
    HideNonMatching,
}

/// A full search request, including whether to invert the match predicate.
#[derive(Clone, Debug)]
pub struct SearchQuery {
    /// The pattern to match.
    pub pattern: Pattern,
    /// Whether to highlight or filter.
    pub mode: SearchMode,
    /// When true, show rows that do NOT match (invert search).
    pub invert: bool,
}
