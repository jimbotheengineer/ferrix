//! Search.
//!
//! ## The trick that makes this fast
//!
//! A naive spreadsheet search compares the needle against every cell's text:
//! 200M rows x 8 columns = 1.6 billion string comparisons, each requiring the
//! cell's value to be formatted first. That is tens of seconds at best.
//!
//! Columnar storage with an interned string arena lets us invert the problem.
//! Text cells do not store text — they store a 4-byte id into an arena holding
//! each *distinct* string once. So we:
//!
//! 1. Match the needle against the arena (the 200M-row benchmark has **18**
//!    distinct strings), producing a bitset of matching ids.
//! 2. Scan the columns comparing 4-byte integers against that bitset.
//!
//! Step 1 is 18 string comparisons instead of 1.6 billion. Step 2 is an
//! integer scan bound by memory bandwidth, not by string handling. The result
//! is that search cost tracks the *cardinality* of the data, not its size.
//!
//! Numbers are compared numerically (the needle is parsed once), never by
//! formatting 200M values into strings.

use crate::{CellRef, StringArena, Value};

/// A compiled search query. Parsing and case-folding happen once, here, rather
/// than per cell.
#[derive(Clone, Debug)]
pub struct Query {
    /// The needle, lowercased when the search is case-insensitive.
    needle: String,
    pub case_sensitive: bool,
    pub whole_cell: bool,
    /// Parsed numeric form, when the needle looks like a number.
    numeric: Option<f64>,
    /// Whether to match numbers by their displayed text as well as by value.
    ///
    /// Formatting a number costs ~100ns; over 200M rows that is 20 seconds, so
    /// the caller enables this only for datasets small enough to afford it.
    pub numeric_substring: bool,
}

impl Query {
    /// Compile a query. Returns `None` for an empty needle.
    pub fn new(raw: &str, case_sensitive: bool, whole_cell: bool) -> Option<Self> {
        if raw.is_empty() {
            return None;
        }
        let needle = if case_sensitive {
            raw.to_string()
        } else {
            raw.to_lowercase()
        };
        Some(Self {
            numeric: raw.trim().parse::<f64>().ok(),
            needle,
            case_sensitive,
            whole_cell,
            numeric_substring: false,
        })
    }

    pub fn needle(&self) -> &str {
        &self.needle
    }

    /// Does this query match a string? The haystack is assumed already
    /// lowercased when the search is case-insensitive.
    #[inline]
    pub fn matches_prepared(&self, hay: &str) -> bool {
        if self.whole_cell {
            hay == self.needle
        } else {
            hay.contains(&self.needle)
        }
    }

    /// Match against a raw string, folding case as needed.
    pub fn matches_str(&self, hay: &str) -> bool {
        if self.case_sensitive {
            self.matches_prepared(hay)
        } else {
            self.matches_prepared(&hay.to_lowercase())
        }
    }

    /// Match against a number. Exact value comparison by default; optionally
    /// also substring-matches the number's displayed form.
    #[inline]
    pub fn matches_number(&self, v: f64) -> bool {
        if let Some(n) = self.numeric {
            if v == n {
                return true;
            }
            if self.whole_cell {
                return false;
            }
        }
        if self.numeric_substring {
            return self.matches_prepared(&crate::format_number(v));
        }
        false
    }

    /// Could this query ever match a numeric cell? Lets a scanner skip whole
    /// numeric columns when the needle is plainly textual.
    #[inline]
    pub fn can_match_numbers(&self) -> bool {
        self.numeric.is_some() || self.numeric_substring
    }

    /// Could this query match a boolean cell?
    #[inline]
    pub fn matches_bool(&self, b: bool) -> bool {
        let s = if b { "true" } else { "false" };
        if self.case_sensitive {
            // TRUE/FALSE are displayed uppercase.
            self.matches_prepared(if b { "TRUE" } else { "FALSE" })
        } else {
            self.matches_prepared(s)
        }
    }

    /// Could this query match any error cell? Checked against the full set of
    /// error spellings so a column of errors is not skipped by the scanner's
    /// whole-column guard.
    pub fn matches_any_error(&self) -> bool {
        const ALL: [&str; 8] = [
            "#DIV/0!", "#VALUE!", "#REF!", "#NAME?", "#NUM!", "#N/A", "#NULL!", "#CIRC!",
        ];
        ALL.iter().any(|e| self.matches_str(e))
    }

    /// Match a fully-resolved value. Used by the small-file path and tests.
    pub fn matches_value(&self, v: &Value, arena: &StringArena) -> bool {
        match v {
            Value::Empty => false,
            Value::Number(n) => self.matches_number(*n),
            Value::Bool(b) => self.matches_bool(*b),
            Value::Text(id) => self.matches_str(arena.resolve_or_empty(*id)),
            Value::Error(e) => self.matches_str(e.as_str()),
        }
    }
}

/// A dense bitset over interned string ids.
///
/// Built once per search from the arena, then consulted with a single shift
/// and mask per cell — which is what turns text search into an integer scan.
#[derive(Clone, Debug, Default)]
pub struct IdSet {
    words: Vec<u64>,
    count: usize,
}

impl IdSet {
    /// Match `query` against every distinct string in `arena`.
    ///
    /// This is the whole optimization: cost is O(distinct strings), not
    /// O(cells). A 200M-row file with 18 distinct strings costs 18 compares.
    pub fn from_arena(arena: &StringArena, query: &Query) -> Self {
        let n = arena.len();
        let mut words = vec![0u64; n.div_ceil(64)];
        let mut count = 0usize;
        for i in 0..n {
            let s = arena.resolve_or_empty(crate::StrId(i as u32));
            if query.matches_str(s) {
                words[i >> 6] |= 1u64 << (i & 63);
                count += 1;
            }
        }
        Self { words, count }
    }

    /// Build from an explicit iterator of (id, string) pairs — used by the
    /// memory-mapped reader, whose arena lives in the mapping.
    pub fn from_pairs<'a, I: Iterator<Item = (u32, &'a str)>>(
        len: usize,
        pairs: I,
        query: &Query,
    ) -> Self {
        let mut words = vec![0u64; len.div_ceil(64)];
        let mut count = 0usize;
        for (id, s) in pairs {
            if query.matches_str(s) {
                let i = id as usize;
                if i < len {
                    words[i >> 6] |= 1u64 << (i & 63);
                    count += 1;
                }
            }
        }
        Self { words, count }
    }

    #[inline]
    pub fn contains(&self, id: u32) -> bool {
        let i = id as usize;
        match self.words.get(i >> 6) {
            Some(w) => (w >> (i & 63)) & 1 == 1,
            None => false,
        }
    }

    /// True when no string matched, so entire text columns can be skipped.
    #[inline]
    pub fn is_empty(&self) -> bool {
        self.count == 0
    }

    pub fn len(&self) -> usize {
        self.count
    }
}

/// Outcome of a search.
#[derive(Clone, Debug, Default)]
pub struct SearchResults {
    /// Matches in row-major order, capped at the caller's limit.
    pub matches: Vec<CellRef>,
    /// Total matches found, which may exceed `matches.len()`.
    pub total: usize,
    /// True when `total` exceeded the limit and `matches` was truncated.
    pub truncated: bool,
    pub millis: u128,
    /// Distinct arena strings that matched — surfaced to explain why a search
    /// over billions of cells returned instantly.
    pub matched_strings: usize,
}

impl SearchResults {
    /// The match at `index`, wrapping around the list.
    pub fn wrapped(&self, index: usize) -> Option<CellRef> {
        if self.matches.is_empty() {
            None
        } else {
            Some(self.matches[index % self.matches.len()])
        }
    }

    /// Index of the first match at or after `cell`, for resuming a search from
    /// the current selection rather than the top of the sheet.
    pub fn index_at_or_after(&self, cell: CellRef) -> usize {
        self.matches
            .partition_point(|m| (m.row, m.col) < (cell.row, cell.col))
            % self.matches.len().max(1)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{ErrorKind, StrId};

    fn q(s: &str) -> Query {
        Query::new(s, false, false).unwrap()
    }

    #[test]
    fn empty_query_is_rejected() {
        assert!(Query::new("", false, false).is_none());
    }

    #[test]
    fn substring_is_case_insensitive_by_default() {
        let query = q("NOR");
        assert!(query.matches_str("north"));
        assert!(query.matches_str("North"));
        assert!(query.matches_str("NORTHEAST"));
        assert!(!query.matches_str("south"));
    }

    #[test]
    fn case_sensitive_mode_respects_case() {
        let query = Query::new("North", true, false).unwrap();
        assert!(query.matches_str("North Dakota"));
        assert!(!query.matches_str("north dakota"));
    }

    #[test]
    fn whole_cell_requires_exact_match() {
        let query = Query::new("north", false, true).unwrap();
        assert!(query.matches_str("north"));
        assert!(query.matches_str("NORTH"));
        assert!(!query.matches_str("northeast"));
    }

    #[test]
    fn numbers_match_by_value_not_text() {
        let query = q("416");
        assert!(query.matches_number(416.0));
        assert!(!query.matches_number(4160.0), "substring off by default");
        // 416.0 and 416 are the same number.
        assert!(query.matches_number(416.000));
    }

    #[test]
    fn numeric_substring_is_opt_in() {
        let mut query = q("41");
        assert!(!query.matches_number(416.0));
        query.numeric_substring = true;
        assert!(query.matches_number(416.0), "should match displayed '416'");
        assert!(!query.matches_number(999.0));
    }

    #[test]
    fn textual_query_skips_numeric_columns() {
        assert!(!q("north").can_match_numbers());
        assert!(q("416").can_match_numbers());
        let mut sub = q("north");
        sub.numeric_substring = true;
        assert!(sub.can_match_numbers(), "substring mode must scan numbers");
    }

    #[test]
    fn booleans_and_errors_are_searchable() {
        assert!(q("tru").matches_bool(true));
        assert!(q("FALSE").matches_bool(false));
        assert!(!q("tru").matches_bool(false));

        let arena = StringArena::new();
        let v = Value::Error(ErrorKind::DivZero);
        assert!(q("DIV").matches_value(&v, &arena));
        assert!(!q("REF").matches_value(&v, &arena));
    }

    #[test]
    fn idset_matches_only_matching_strings() {
        let mut arena = StringArena::new();
        let north = arena.intern("north");
        let south = arena.intern("south");
        let northeast = arena.intern("northeast");

        let set = IdSet::from_arena(&arena, &q("north"));
        assert_eq!(set.len(), 2);
        assert!(set.contains(north.0));
        assert!(set.contains(northeast.0));
        assert!(!set.contains(south.0));
    }

    #[test]
    fn idset_is_empty_when_nothing_matches() {
        let mut arena = StringArena::new();
        arena.intern("alpha");
        arena.intern("beta");
        let set = IdSet::from_arena(&arena, &q("zzz"));
        assert!(set.is_empty(), "lets the scanner skip whole columns");
        assert!(!set.contains(0));
    }

    #[test]
    fn idset_cost_is_cardinality_not_row_count() {
        // The core claim: a column of 10M cells drawn from 3 distinct strings
        // costs 3 comparisons to filter, not 10M.
        let mut arena = StringArena::new();
        for i in 0..10_000 {
            arena.intern(["alpha", "beta", "gamma"][i % 3]);
        }
        assert_eq!(arena.len(), 3, "arena dedups");
        let set = IdSet::from_arena(&arena, &q("a"));
        // alpha, beta, gamma all contain 'a'
        assert_eq!(set.len(), 3);
    }

    #[test]
    fn idset_handles_out_of_range_ids() {
        let arena = StringArena::new();
        let set = IdSet::from_arena(&arena, &q("x"));
        assert!(!set.contains(9999), "must not panic on a stale id");
    }

    #[test]
    fn results_wrap_around() {
        let r = SearchResults {
            matches: vec![CellRef::new(0, 0), CellRef::new(5, 0), CellRef::new(9, 0)],
            total: 3,
            ..Default::default()
        };
        assert_eq!(r.wrapped(0), Some(CellRef::new(0, 0)));
        assert_eq!(r.wrapped(2), Some(CellRef::new(9, 0)));
        // Cycling past the end returns to the start.
        assert_eq!(r.wrapped(3), Some(CellRef::new(0, 0)));
        assert_eq!(r.wrapped(4), Some(CellRef::new(5, 0)));
    }

    #[test]
    fn empty_results_wrap_to_nothing() {
        let r = SearchResults::default();
        assert_eq!(r.wrapped(0), None);
        assert_eq!(r.index_at_or_after(CellRef::new(3, 3)), 0);
    }

    #[test]
    fn resumes_from_the_current_selection() {
        let r = SearchResults {
            matches: vec![CellRef::new(2, 0), CellRef::new(7, 0), CellRef::new(9, 1)],
            total: 3,
            ..Default::default()
        };
        assert_eq!(r.index_at_or_after(CellRef::new(0, 0)), 0);
        assert_eq!(r.index_at_or_after(CellRef::new(3, 0)), 1);
        assert_eq!(r.index_at_or_after(CellRef::new(7, 0)), 1);
        assert_eq!(r.index_at_or_after(CellRef::new(8, 0)), 2);
        // Past the last match, wrap to the first.
        assert_eq!(r.index_at_or_after(CellRef::new(99, 0)), 0);
    }

    #[test]
    fn stale_string_id_resolves_safely() {
        let arena = StringArena::new();
        let v = Value::Text(StrId(12345));
        // Must not panic; an unresolvable id is an empty string.
        assert!(!q("anything").matches_value(&v, &arena));
    }

    #[test]
    fn case_sensitive_toggle_changes_matching() {
        // Acceptance criterion from issue #5: case-sensitive "North" must not
        // match "north".
        let insensitive = Query::new("North", false, false).unwrap();
        assert!(insensitive.matches_str("north"));
        assert!(insensitive.matches_str("NORTH"));

        let sensitive = Query::new("North", true, false).unwrap();
        assert!(sensitive.matches_str("North"));
        assert!(!sensitive.matches_str("north"), "case must be respected");
        assert!(!sensitive.matches_str("NORTH"));
    }

    #[test]
    fn whole_cell_toggle_rejects_substrings() {
        // Acceptance criterion: whole-cell "open" must not match "reopened".
        let substring = Query::new("open", false, false).unwrap();
        assert!(substring.matches_str("reopened"));
        assert!(substring.matches_str("open"));

        let whole = Query::new("open", false, true).unwrap();
        assert!(whole.matches_str("open"));
        assert!(!whole.matches_str("reopened"), "substring must not match");
        assert!(!whole.matches_str("opened"));
    }

    #[test]
    fn both_toggles_compose() {
        let q = Query::new("Open", true, true).unwrap();
        assert!(q.matches_str("Open"));
        assert!(!q.matches_str("open"), "wrong case");
        assert!(!q.matches_str("Opened"), "not the whole cell");
        assert!(!q.matches_str("reOpen"), "not the whole cell");
    }
}
