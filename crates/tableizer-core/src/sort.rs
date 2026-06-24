//! Column sort (`docs/architecture.md`).
//!
//! Sort is **global** and applied through the async "view" ([`crate::table::CsvTable`]): the engine
//! extracts the sort-key field for every (filtered) row and orders them. With infinite virtualised
//! scroll there is no page-local sort. Keys compare numerically when both parse as numbers, otherwise
//! byte-lexicographically — so `"10"` sorts after `"9"`. Two integer keys are compared as exact
//! integers (`i128`) before falling back to `f64`, so large IDs beyond `f64`'s 2^53 exact range
//! (e.g. `9007199254740993` vs `…992`) still order correctly rather than collapsing to equal.
//!
//! The current implementation sorts in memory; a spill-to-disk external merge sort is the documented
//! refinement for datasets whose key+rownum set exceeds RAM (§4.3).

use std::cmp::Ordering;

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

/// A request to sort by a column.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct SortKey {
    /// Column whose field is the sort key.
    pub column: ColumnId,
    /// Ascending or descending.
    pub direction: Direction,
}

/// Compare two key fields: as exact integers when both parse as integers, else numerically as
/// `f64` when both parse as numbers, else byte-lexicographically.
pub(crate) fn compare_keys(a: &[u8], b: &[u8], direction: Direction) -> Ordering {
    let ordering = match (parse_int(a), parse_int(b)) {
        // Exact integer compare first, so IDs past f64's 2^53 precision don't collapse to equal.
        (Some(x), Some(y)) => x.cmp(&y),
        _ => match (parse_number(a), parse_number(b)) {
            (Some(x), Some(y)) => x.partial_cmp(&y).unwrap_or(Ordering::Equal),
            _ => a.cmp(b),
        },
    };
    match direction {
        Direction::Ascending => ordering,
        Direction::Descending => ordering.reverse(),
    }
}

/// Parse a key as an exact integer (`i128` covers up to ~38 digits without precision loss).
fn parse_int(bytes: &[u8]) -> Option<i128> {
    std::str::from_utf8(bytes).ok()?.trim().parse::<i128>().ok()
}

fn parse_number(bytes: &[u8]) -> Option<f64> {
    std::str::from_utf8(bytes).ok()?.trim().parse::<f64>().ok()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn numeric_keys_sort_numerically_not_lexically() {
        // Lexically "10" < "9"; numerically 10 > 9.
        assert_eq!(
            compare_keys(b"10", b"9", Direction::Ascending),
            Ordering::Greater
        );
    }

    #[test]
    fn text_keys_sort_lexically() {
        assert_eq!(
            compare_keys(b"apple", b"banana", Direction::Ascending),
            Ordering::Less
        );
    }

    #[test]
    fn descending_reverses_the_order() {
        assert_eq!(
            compare_keys(b"a", b"b", Direction::Descending),
            Ordering::Greater
        );
    }

    #[test]
    fn large_integers_beyond_f64_precision_compare_exactly() {
        // 9007199254740992 == 2^53; both of these round to it as f64, so an f64-only compare would
        // wrongly report them equal. Exact-integer compare keeps them ordered.
        assert_eq!(
            compare_keys(
                b"9007199254740993",
                b"9007199254740992",
                Direction::Ascending
            ),
            Ordering::Greater
        );
        assert_eq!(
            compare_keys(
                b"9007199254740993",
                b"9007199254740993",
                Direction::Ascending
            ),
            Ordering::Equal
        );
    }

    #[test]
    fn integer_and_non_integer_numbers_still_compare_numerically() {
        // One side isn't an integer, so the f64 path handles it: 10 > 9.5, not lexical.
        assert_eq!(
            compare_keys(b"10", b"9.5", Direction::Ascending),
            Ordering::Greater
        );
    }
}
