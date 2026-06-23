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

impl Dialect {
    /// Auto-detect a dialect from a head sample: pick the delimiter that yields the most consistent
    /// column count, and guess whether the first row is a header. Always an *editable default* — the
    /// user can override (spec §3.1). On a tie, earlier candidates win (comma first).
    pub fn sniff(sample: &[u8]) -> Self {
        const CANDIDATES: [u8; 4] = [b',', b'\t', b';', b'|'];
        let mut best_delim = b',';
        let mut best_score = 0i64;
        for &delim in &CANDIDATES {
            let score = consistency_score(sample, delim);
            if score > best_score {
                best_score = score;
                best_delim = delim;
            }
        }
        Self {
            delimiter: best_delim,
            quote: b'"',
            has_header: sniff_header(sample, best_delim),
        }
    }
}

/// Read up to 20 records with `delimiter` and score by how many share the first record's field
/// count. A delimiter that doesn't split the data (one field per row) scores 0.
fn consistency_score(sample: &[u8], delimiter: u8) -> i64 {
    let mut reader = sniff_reader(sample, delimiter);
    let mut record = csv::ByteRecord::new();
    let mut counts = Vec::new();
    while counts.len() < 20 {
        match reader.read_byte_record(&mut record) {
            Ok(true) => counts.push(record.len()),
            _ => break,
        }
    }
    let Some(&first) = counts.first() else {
        return 0;
    };
    if first <= 1 {
        return 0;
    }
    counts.iter().filter(|&&c| c == first).count() as i64
}

/// Guess a header: the first row is a header if none of its fields parse as a number (data rows
/// usually carry at least one numeric field). Defaults to `true` for an empty sample.
fn sniff_header(sample: &[u8], delimiter: u8) -> bool {
    let mut reader = sniff_reader(sample, delimiter);
    let mut record = csv::ByteRecord::new();
    if reader.read_byte_record(&mut record).unwrap_or(false) {
        !record.iter().any(looks_numeric)
    } else {
        true
    }
}

fn looks_numeric(field: &[u8]) -> bool {
    std::str::from_utf8(field)
        .ok()
        .map(str::trim)
        .is_some_and(|s| !s.is_empty() && s.parse::<f64>().is_ok())
}

fn sniff_reader(sample: &[u8], delimiter: u8) -> csv::Reader<&[u8]> {
    csv::ReaderBuilder::new()
        .delimiter(delimiter)
        .has_headers(false)
        .flexible(true)
        .from_reader(sample)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sniffs_comma_with_header() {
        let d = Dialect::sniff(b"name,age,city\nbob,30,paris\nann,25,rome\n");
        assert_eq!(d.delimiter, b',');
        assert!(d.has_header);
    }

    #[test]
    fn sniffs_tab_delimiter() {
        let d = Dialect::sniff(b"a\tb\tc\n1\t2\t3\n");
        assert_eq!(d.delimiter, b'\t');
    }

    #[test]
    fn sniffs_semicolon_delimiter() {
        let d = Dialect::sniff(b"a;b;c\n1;2;3\n");
        assert_eq!(d.delimiter, b';');
    }

    #[test]
    fn detects_no_header_when_first_row_is_numeric() {
        let d = Dialect::sniff(b"1,2,3\n4,5,6\n");
        assert!(!d.has_header);
    }
}
