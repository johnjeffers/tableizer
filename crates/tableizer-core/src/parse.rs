//! Byte-faithful parsing of CSV / TSV / arbitrary-separator text (`docs/spec.md` §3.1).
//!
//! To be built on the `csv` / `csv-core` crates using *byte* records, so the canonical cell value
//! is always the exact source bytes. Encoding handling (BOM-aware, via `encoding_rs`) is
//! lossy-but-*visible*: undecodable bytes render as U+FFFD and are counted, never silently dropped,
//! never panicked on. A max-field / max-record guard defends against single-field DoS.

/// The dialect describing how to split a file into records and fields. Auto-detected as an
/// *editable default*, never a silent authority (sampled from multiple regions, not just the head).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Dialect {
    /// Field separator byte (e.g. `,` for CSV, `\t` for TSV, arbitrary otherwise).
    pub delimiter: u8,
    /// Quote byte; quoted fields may contain embedded delimiters and newlines.
    pub quote: u8,
    /// Whether the first record is a header row.
    pub has_header: bool,
}

impl Default for Dialect {
    fn default() -> Self {
        Self {
            delimiter: b',',
            quote: b'"',
            has_header: true,
        }
    }
}
