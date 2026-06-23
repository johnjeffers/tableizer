//! Search / filter matching (`docs/spec.md` §3.3).
//!
//! A literal query is escaped to a regex, so one code path serves literal, substring, AND regex —
//! all on the `regex` crate's linear-time automaton (ReDoS-safe; user-supplied patterns are never a
//! backtracking risk). A record matches if **any field** matches; `invert` flips the result.

use crate::{Error, Result};

/// A filter request as plain data (no compiled state), suitable for crossing the
/// [`crate::ViewportSource`] seam.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct FilterSpec {
    /// The query (a literal substring, or a regex if `regex` is set).
    pub query: String,
    /// Interpret `query` as a regular expression rather than a literal substring.
    pub regex: bool,
    /// Show rows that do NOT match (invert search).
    pub invert: bool,
    /// Match `query` case-sensitively. When false (the default), matching ignores case.
    pub case_sensitive: bool,
}

/// A compiled matcher over record fields (case-insensitive unless the spec sets `case_sensitive`).
pub struct Matcher {
    regex: regex::bytes::Regex,
    invert: bool,
}

impl Matcher {
    /// Compile a matcher from a [`FilterSpec`]. Returns [`Error::InvalidPattern`] if a regex query
    /// does not compile (literal queries always compile — they are escaped first).
    pub fn compile(spec: &FilterSpec) -> Result<Self> {
        let pattern = if spec.regex {
            spec.query.clone()
        } else {
            regex::escape(&spec.query)
        };
        let regex = regex::bytes::RegexBuilder::new(&pattern)
            .case_insensitive(!spec.case_sensitive)
            .build()
            .map_err(|e| Error::InvalidPattern(e.to_string()))?;
        Ok(Self {
            regex,
            invert: spec.invert,
        })
    }

    /// Whether `record` matches: any field matches the pattern, XOR-ed with `invert`.
    pub fn matches(&self, record: &csv::ByteRecord) -> bool {
        let any = record.iter().any(|field| self.regex.is_match(field));
        any ^ self.invert
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn record(fields: &[&str]) -> csv::ByteRecord {
        let mut record = csv::ByteRecord::new();
        for field in fields {
            record.push_field(field.as_bytes());
        }
        record
    }

    fn matcher(query: &str, regex: bool, invert: bool) -> Matcher {
        Matcher::compile(&FilterSpec {
            query: query.into(),
            regex,
            invert,
            case_sensitive: false,
        })
        .unwrap()
    }

    #[test]
    fn literal_match_is_a_case_insensitive_substring() {
        let m = matcher("err", false, false);
        assert!(m.matches(&record(&["INFO", "Error: x"])));
        assert!(!m.matches(&record(&["INFO", "ok"])));
    }

    #[test]
    fn regex_match() {
        let m = matcher(r"^\d+$", true, false);
        assert!(m.matches(&record(&["abc", "123"])));
        assert!(!m.matches(&record(&["abc", "12x"])));
    }

    #[test]
    fn invert_negates_the_match() {
        let m = matcher("x", false, true);
        assert!(!m.matches(&record(&["x"]))); // contains x → inverted → false
        assert!(m.matches(&record(&["y"]))); // no x → inverted → true
    }

    #[test]
    fn an_invalid_regex_is_an_error() {
        assert!(matches!(
            Matcher::compile(&FilterSpec {
                query: "(".into(),
                regex: true,
                invert: false,
                case_sensitive: false,
            }),
            Err(Error::InvalidPattern(_))
        ));
    }

    #[test]
    fn case_sensitive_match_respects_case() {
        let spec = |case_sensitive| FilterSpec {
            query: "err".into(),
            regex: false,
            invert: false,
            case_sensitive,
        };
        // Insensitive (default) matches regardless of case.
        let insensitive = Matcher::compile(&spec(false)).unwrap();
        assert!(insensitive.matches(&record(&["ERROR"])));
        // Sensitive matches only the exact case.
        let sensitive = Matcher::compile(&spec(true)).unwrap();
        assert!(!sensitive.matches(&record(&["ERROR"])));
        assert!(sensitive.matches(&record(&["an error"])));
    }
}
