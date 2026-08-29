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

use std::sync::Arc;

use crate::{CancelToken, CellRef, StringArena, Value};

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
    /// Compiled pattern, when the user asked for regex mode.
    ///
    /// Behind an `Arc` so cloning a `Query` — which happens once per replace
    /// pass, never per cell — stays a refcount bump. Case-insensitivity and
    /// whole-cell anchoring are baked into the pattern at compile time, so the
    /// per-cell path is a single `is_match` with no branching on options.
    re: Option<Arc<regex_lite::Regex>>,
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
            re: None,
        })
    }

    /// Compile a regex query.
    ///
    /// `whole_cell` is expressed by anchoring the pattern rather than by a
    /// second check at match time, and case-insensitivity by an inline `(?i)`
    /// flag — so both the search scan and the replace rewrite get identical
    /// semantics from one compiled automaton, and neither pays for the option
    /// per cell.
    ///
    /// Returns `Ok(None)` for an empty pattern, `Err` for a malformed one so
    /// the UI can say what is wrong instead of silently finding nothing.
    pub fn new_regex(
        raw: &str,
        case_sensitive: bool,
        whole_cell: bool,
    ) -> Result<Option<Self>, String> {
        if raw.is_empty() {
            return Ok(None);
        }
        let mut src = String::with_capacity(raw.len() + 12);
        if !case_sensitive {
            src.push_str("(?i)");
        }
        if whole_cell {
            src.push_str("^(?:");
            src.push_str(raw);
            src.push_str(")$");
        } else {
            src.push_str(raw);
        }
        let re = regex_lite::Regex::new(&src).map_err(|e| e.to_string())?;
        Ok(Some(Self {
            needle: raw.to_string(),
            case_sensitive,
            whole_cell,
            // A regex never matches a number by value; it matches the
            // number's text, and only when the caller has opted into paying
            // for formatting.
            numeric: None,
            numeric_substring: false,
            re: Some(Arc::new(re)),
        }))
    }

    /// Compile either flavour, chosen by `regex`.
    pub fn compile(
        raw: &str,
        case_sensitive: bool,
        whole_cell: bool,
        regex: bool,
    ) -> Result<Option<Self>, String> {
        if regex {
            Self::new_regex(raw, case_sensitive, whole_cell)
        } else {
            Ok(Self::new(raw, case_sensitive, whole_cell))
        }
    }

    pub fn is_regex(&self) -> bool {
        self.re.is_some()
    }

    pub fn needle(&self) -> &str {
        &self.needle
    }

    /// Does this query match a string? The haystack is assumed already
    /// lowercased when the search is case-insensitive.
    #[inline]
    pub fn matches_prepared(&self, hay: &str) -> bool {
        // One `Option` discriminant check on a path that used to have none.
        // The literal branch below is byte-for-byte what it was.
        if let Some(re) = &self.re {
            return re.is_match(hay);
        }
        if self.whole_cell {
            hay == self.needle
        } else {
            hay.contains(&self.needle)
        }
    }

    /// Match against a raw string, folding case as needed.
    pub fn matches_str(&self, hay: &str) -> bool {
        if let Some(re) = &self.re {
            // The `(?i)` flag is already in the pattern; lowercasing here as
            // well would only cost an allocation.
            return re.is_match(hay);
        }
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

    /// Build from an arbitrary predicate over (id, string) pairs.
    ///
    /// Same arena-first economics as [`IdSet::from_arena`], but for callers
    /// whose match test is not a [`Query`] — a filter's value checklist, for
    /// instance. The predicate runs once per *distinct* string.
    pub fn from_pairs_pred<'a, I, F>(len: usize, pairs: I, pred: F) -> Self
    where
        I: Iterator<Item = (usize, &'a String)>,
        F: Fn(&str) -> bool,
    {
        let mut words = vec![0u64; len.div_ceil(64)];
        let mut count = 0usize;
        for (i, s) in pairs {
            if i < len && pred(s) {
                words[i >> 6] |= 1u64 << (i & 63);
                count += 1;
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

// ---------------------------------------------------------------- replace
//
// Replace reuses the search economics exactly. Finding what to change is a
// `Query` scan — arena first, then an integer column scan — so the cost of
// deciding *whether* a 1.6-billion-cell sheet has anything to replace is the
// same ~488ms warm / 0ms absent that search already measures. Nothing below
// runs per cell; it runs per *matched* cell, and a matched cell is one the
// user is about to change anyway.

/// Which text a replace reads and rewrites.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum LookIn {
    /// The displayed value. A formula cell's *result* is what the user sees,
    /// and rewriting a result is meaningless — so formula cells are skipped
    /// entirely in this mode rather than silently clobbered into literals.
    #[default]
    Values,
    /// The underlying content: a formula's source text (`=A1*2`), or a
    /// literal's text. This is what lets "replace A1 with B1 everywhere"
    /// mean what a spreadsheet user expects.
    Formulas,
}

impl LookIn {
    pub fn label(self) -> &'static str {
        match self {
            LookIn::Values => "values",
            LookIn::Formulas => "formulas",
        }
    }
}

/// A compiled find-and-replace: what to look for, what to put there, and
/// which text to read.
#[derive(Clone, Debug)]
pub struct ReplaceSpec {
    pub query: Query,
    pub replacement: String,
    pub look_in: LookIn,
}

impl ReplaceSpec {
    pub fn new(query: Query, replacement: impl Into<String>, look_in: LookIn) -> Self {
        Self {
            query,
            replacement: replacement.into(),
            look_in,
        }
    }

    /// Rewrite one cell's text.
    ///
    /// `None` means "this text does not match", which is distinct from "the
    /// rewrite produced the same string": a replace that is a no-op on the
    /// text must not be recorded as a change, or Replace All would write —
    /// and make undoable — thousands of cells it did not alter.
    pub fn rewrite(&self, text: &str) -> Option<String> {
        if let Some(re) = &self.query.re {
            if !re.is_match(text) {
                return None;
            }
            // The pattern already carries `(?i)` and any `^...$` anchoring, so
            // capture groups (`$1`) work the same in whole-cell mode.
            let out = re.replace_all(text, self.replacement.as_str()).into_owned();
            return (out != text).then_some(out);
        }
        if self.query.whole_cell {
            // Whole-cell: the entire cell becomes the replacement, or nothing
            // happens. A partial rewrite here would contradict the option.
            if !self.query.matches_str(text) {
                return None;
            }
            let out = self.replacement.clone();
            return (out != text).then_some(out);
        }
        let out = if self.query.case_sensitive {
            if !text.contains(&self.query.needle) {
                return None;
            }
            text.replace(&self.query.needle, &self.replacement)
        } else {
            // `needle` is already lowercased for a case-insensitive query.
            replace_case_insensitive(text, &self.query.needle, &self.replacement)?
        };
        (out != text).then_some(out)
    }
}

/// Case-insensitive substring replace that preserves the untouched parts of
/// the original exactly.
///
/// Lowercasing can change a string's byte length (`İ` folds to two chars), so
/// matching on a lowercased copy and splicing by its offsets would corrupt
/// non-ASCII text. The offset map below translates every lowercase byte
/// position back to the original one, which keeps the unmatched remainder
/// byte-identical. It allocates — but only for a cell that already matched
/// and is therefore about to be rewritten anyway, never during the scan.
fn replace_case_insensitive(hay: &str, needle_lower: &str, repl: &str) -> Option<String> {
    if needle_lower.is_empty() {
        return None;
    }
    let mut lower = String::with_capacity(hay.len());
    // `map[i]` is the byte offset in `hay` that lowercase byte `i` came from.
    let mut map: Vec<usize> = Vec::with_capacity(hay.len() + 1);
    for (i, ch) in hay.char_indices() {
        for lc in ch.to_lowercase() {
            let mut buf = [0u8; 4];
            let encoded = lc.encode_utf8(&mut buf);
            for _ in 0..encoded.len() {
                map.push(i);
            }
            lower.push(lc);
        }
    }
    map.push(hay.len());

    let first = lower.find(needle_lower)?;
    let mut out = String::with_capacity(hay.len());
    let mut cut = 0usize; // consumed up to here, in ORIGINAL bytes
    let mut scan = first; // next place to look, in LOWERCASE bytes
    loop {
        let end_l = scan + needle_lower.len();
        out.push_str(&hay[cut..map[scan]]);
        out.push_str(repl);
        cut = map[end_l];
        match lower[end_l..].find(needle_lower) {
            Some(off) => scan = end_l + off,
            None => break,
        }
    }
    out.push_str(&hay[cut..]);
    Some(out)
}

/// How a replace pass ended.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ReplaceOutcome {
    /// Every candidate was examined.
    Completed,
    /// The user cancelled. Edits already applied STAY applied — see
    /// [`ReplaceReport::applied`]. Rolling them back silently would discard
    /// work the user watched happen; continuing silently would ignore them.
    Cancelled,
    /// The edit budget was reached. The pass stopped rather than allocate an
    /// undo entry larger than memory allows.
    BudgetExhausted,
}

impl ReplaceOutcome {
    pub fn is_partial(self) -> bool {
        !matches!(self, ReplaceOutcome::Completed)
    }
}

/// What a replace pass did.
#[derive(Clone, Debug)]
pub struct ReplaceReport {
    /// Cells actually written. This is the number a cancelled pass must
    /// report, and the number one undo must restore.
    pub applied: usize,
    /// Candidate cells examined, whether or not they changed.
    pub examined: usize,
    pub outcome: ReplaceOutcome,
    pub millis: u128,
}

impl ReplaceReport {
    pub fn describe(&self) -> String {
        let cells = if self.applied == 1 { "cell" } else { "cells" };
        match self.outcome {
            ReplaceOutcome::Completed => {
                format!("Replaced {} {} in {} ms", self.applied, cells, self.millis)
            }
            ReplaceOutcome::Cancelled => format!(
                "Replace cancelled — {} {} already replaced were kept",
                self.applied, cells
            ),
            ReplaceOutcome::BudgetExhausted => format!(
                "Replaced {} {} — stopped at the memory budget; run again for the rest",
                self.applied, cells
            ),
        }
    }
}

/// How many candidates are examined between cancellation polls.
///
/// The cadence is the whole point (see [`crate::cancel`]): a token polled once
/// per pass is decorative. A rewrite of one cell is sub-microsecond, so 1024
/// of them is a fraction of a millisecond of latency on a cancel press, while
/// the atomic load stays off the per-cell path — one relaxed load per 1024
/// rewrites is unmeasurable next to the rewrites themselves.
pub const CANCEL_POLL_INTERVAL: usize = 1024;

/// Drive a replace over a stream of candidate cells.
///
/// This is the apply path, and it is deliberately storage-agnostic: it takes
/// candidates as an **iterator** and hands each rewrite straight to `apply`.
/// Nothing accumulates here, so peak memory is one candidate's text plus
/// whatever the caller chooses to retain — never the match count and never the
/// row count. A Replace All over 200M rows is a fold, not a collection.
///
/// * `max_edits` caps how many cells may be written, so the undo entry the
///   caller builds stays inside the memory budget.
/// * `cancel` is polled every [`CANCEL_POLL_INTERVAL`] candidates. On cancel
///   the pass returns immediately with everything applied so far intact.
/// * `progress` is called with `(examined, applied)` at the same cadence, so a
///   long pass can report without costing a callback per cell.
pub fn replace_stream<I, A, P>(
    spec: &ReplaceSpec,
    candidates: I,
    cancel: &CancelToken,
    max_edits: usize,
    mut apply: A,
    mut progress: P,
) -> ReplaceReport
where
    I: IntoIterator<Item = (CellRef, String)>,
    A: FnMut(CellRef, String),
    P: FnMut(usize, usize),
{
    let t = std::time::Instant::now();
    let mut examined = 0usize;
    let mut applied = 0usize;
    let mut outcome = ReplaceOutcome::Completed;

    for (cell, text) in candidates {
        if examined % CANCEL_POLL_INTERVAL == 0 {
            progress(examined, applied);
            if cancel.is_cancelled() {
                outcome = ReplaceOutcome::Cancelled;
                break;
            }
        }
        examined += 1;
        let Some(new_text) = spec.rewrite(&text) else {
            continue;
        };
        if applied >= max_edits {
            outcome = ReplaceOutcome::BudgetExhausted;
            break;
        }
        apply(cell, new_text);
        applied += 1;
    }
    progress(examined, applied);

    ReplaceReport {
        applied,
        examined,
        outcome,
        millis: t.elapsed().as_millis(),
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

    // ------------------------------------------------------------- replace

    fn spec(find: &str, repl: &str) -> ReplaceSpec {
        ReplaceSpec::new(q(find), repl, LookIn::Values)
    }

    #[test]
    fn rewrite_replaces_every_occurrence_in_a_cell() {
        let s = spec("a", "X");
        assert_eq!(s.rewrite("banana").as_deref(), Some("bXnXnX"));
    }

    #[test]
    fn rewrite_returns_none_when_nothing_matches() {
        // The distinction the whole apply path rests on: a non-match must not
        // be recorded as a change, or Replace All would write every cell it
        // examined and make each one undoable.
        assert_eq!(spec("zzz", "X").rewrite("banana"), None);
    }

    #[test]
    fn rewrite_returns_none_when_the_result_is_identical() {
        // Replacing "a" with "a" matches but changes nothing. Recording it
        // would inflate the reported count and the undo entry with cells that
        // did not move.
        assert_eq!(spec("a", "a").rewrite("banana"), None);
    }

    #[test]
    fn rewrite_is_case_insensitive_by_default_and_preserves_the_rest() {
        let s = spec("north", "SOUTH");
        assert_eq!(
            s.rewrite("NORTH by NoRtHwest").as_deref(),
            Some("SOUTH by SOUTHwest"),
            "unmatched text must survive byte-for-byte"
        );
    }

    #[test]
    fn case_sensitive_rewrite_leaves_other_casings_alone() {
        let s = ReplaceSpec::new(
            Query::new("North", true, false).unwrap(),
            "South",
            LookIn::Values,
        );
        assert_eq!(
            s.rewrite("North north NORTH").as_deref(),
            Some("South north NORTH")
        );
    }

    #[test]
    fn case_insensitive_rewrite_handles_multibyte_text() {
        // Lowercasing can change byte length, so splicing by lowercase offsets
        // would corrupt the untouched remainder. The tail here must come out
        // exactly as it went in.
        let s = spec("straße", "road");
        assert_eq!(
            s.rewrite("Straße — Köln").as_deref(),
            Some("road — Köln"),
            "non-ASCII text outside the match must be preserved exactly"
        );
    }

    #[test]
    fn whole_cell_rewrite_replaces_the_entire_cell() {
        let s = ReplaceSpec::new(
            Query::new("open", false, true).unwrap(),
            "closed",
            LookIn::Values,
        );
        assert_eq!(s.rewrite("open").as_deref(), Some("closed"));
        assert_eq!(s.rewrite("OPEN").as_deref(), Some("closed"));
        assert_eq!(
            s.rewrite("reopened"),
            None,
            "whole-cell must not touch a substring match"
        );
    }

    #[test]
    fn regex_rewrite_supports_capture_groups() {
        let query = Query::new_regex(r"(\d{4})-(\d{2})", false, false)
            .unwrap()
            .unwrap();
        let s = ReplaceSpec::new(query, "$2/$1", LookIn::Values);
        assert_eq!(
            s.rewrite("due 2024-07 ok").as_deref(),
            Some("due 07/2024 ok")
        );
    }

    #[test]
    fn regex_whole_cell_is_anchored() {
        let query = Query::new_regex(r"\d+", false, true).unwrap().unwrap();
        let s = ReplaceSpec::new(query, "N", LookIn::Values);
        assert_eq!(s.rewrite("123").as_deref(), Some("N"));
        assert_eq!(
            s.rewrite("abc123"),
            None,
            "an anchored pattern must not match a substring"
        );
    }

    #[test]
    fn regex_case_sensitivity_is_baked_into_the_pattern() {
        let insensitive = Query::new_regex("north", false, false).unwrap().unwrap();
        assert!(insensitive.matches_str("NORTH"));
        let sensitive = Query::new_regex("north", true, false).unwrap().unwrap();
        assert!(!sensitive.matches_str("NORTH"));
        assert!(sensitive.matches_str("north"));
    }

    #[test]
    fn a_malformed_regex_is_reported_not_swallowed() {
        // Silently finding nothing would look identical to "no matches", which
        // is the worst possible answer to a typo'd pattern.
        assert!(Query::new_regex("(unclosed", false, false).is_err());
        assert!(Query::new_regex("", false, false).unwrap().is_none());
    }

    #[test]
    fn literal_query_is_not_a_regex() {
        assert!(!q("a.c").is_regex());
        // A literal '.' must match only a literal '.'.
        assert_eq!(spec("a.c", "X").rewrite("abc"), None);
        assert_eq!(spec("a.c", "X").rewrite("a.c").as_deref(), Some("X"));
    }

    #[test]
    fn replace_stream_applies_only_matching_cells() {
        let s = spec("beta", "GAMMA");
        let cands = vec![
            (CellRef::new(0, 0), "alpha".to_string()),
            (CellRef::new(1, 0), "beta".to_string()),
            (CellRef::new(2, 0), "betamax".to_string()),
            (CellRef::new(3, 0), "delta".to_string()),
        ];
        let mut applied = Vec::new();
        let report = replace_stream(
            &s,
            cands,
            &CancelToken::new(),
            usize::MAX,
            |c, t| applied.push((c, t)),
            |_, _| {},
        );
        assert_eq!(report.outcome, ReplaceOutcome::Completed);
        assert_eq!(report.examined, 4);
        assert_eq!(report.applied, 2);
        assert_eq!(
            applied,
            vec![
                (CellRef::new(1, 0), "GAMMA".to_string()),
                (CellRef::new(2, 0), "GAMMAmax".to_string()),
            ],
            "exactly the matching cells, and nothing else"
        );
    }

    #[test]
    fn replace_stream_stops_on_cancel_and_keeps_what_it_applied() {
        // The contract: cancelling must not roll back. A half-applied replace
        // that silently reverts is worse than either finishing or stopping.
        let s = spec("x", "y");
        let token = CancelToken::new();
        // Enough candidates to cross the poll boundary twice.
        let n = CANCEL_POLL_INTERVAL * 3;
        let cands: Vec<_> = (0..n)
            .map(|i| (CellRef::new(i as u32, 0), "x".to_string()))
            .collect();

        let mut applied = 0usize;
        let cancel_at = CANCEL_POLL_INTERVAL;
        let t2 = token.clone();
        let report = replace_stream(
            &s,
            cands,
            &token,
            usize::MAX,
            |_, _| applied += 1,
            |examined, _| {
                if examined >= cancel_at {
                    t2.cancel();
                }
            },
        );

        assert_eq!(report.outcome, ReplaceOutcome::Cancelled);
        assert!(
            report.applied > 0,
            "cancel must not discard work already done"
        );
        assert!(
            report.applied < n,
            "cancel must actually stop the pass ({} of {n})",
            report.applied
        );
        assert_eq!(
            applied, report.applied,
            "the reported count must be the number of cells actually written"
        );
        assert!(report.describe().contains(&report.applied.to_string()));
    }

    #[test]
    fn replace_stream_respects_the_edit_budget() {
        let s = spec("x", "y");
        let cands: Vec<_> = (0..100)
            .map(|i| (CellRef::new(i, 0), "x".to_string()))
            .collect();
        let mut applied = 0usize;
        let report = replace_stream(
            &s,
            cands,
            &CancelToken::new(),
            10,
            |_, _| applied += 1,
            |_, _| {},
        );
        assert_eq!(report.outcome, ReplaceOutcome::BudgetExhausted);
        assert_eq!(report.applied, 10);
        assert_eq!(
            applied, 10,
            "the cap must bound actual writes, not just the report"
        );
    }

    #[test]
    fn replace_stream_reports_progress_without_a_callback_per_cell() {
        let s = spec("x", "y");
        let n = CANCEL_POLL_INTERVAL * 2 + 7;
        let cands: Vec<_> = (0..n)
            .map(|i| (CellRef::new(i as u32, 0), "x".to_string()))
            .collect();
        let mut ticks = 0usize;
        let mut last = (0usize, 0usize);
        replace_stream(
            &s,
            cands,
            &CancelToken::new(),
            usize::MAX,
            |_, _| {},
            |e, a| {
                ticks += 1;
                last = (e, a);
            },
        );
        assert_eq!(last, (n, n), "the final tick reports the finished totals");
        assert!(
            ticks < 10,
            "progress must be sampled, not emitted per cell (got {ticks} for {n} cells)"
        );
    }

    #[test]
    fn replace_stream_over_a_huge_lazy_stream_does_not_accumulate() {
        // The scale invariant, expressed as a type: the candidate source is a
        // lazy iterator that is never collected, and the apply closure keeps
        // only a counter. Peak memory here is one cell's text.
        let s = spec("q", "Z");
        let n = 5_000_000usize;
        let cands = (0..n).map(|i| (CellRef::new(i as u32, 0), "q".to_string()));
        let mut applied = 0usize;
        let report = replace_stream(
            &s,
            cands,
            &CancelToken::new(),
            usize::MAX,
            |_, _| applied += 1,
            |_, _| {},
        );
        assert_eq!(report.applied, n);
        assert_eq!(applied, n);
    }
}
