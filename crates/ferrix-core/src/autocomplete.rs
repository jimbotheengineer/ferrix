//! In-cell autocomplete over a column's distinct values (issue #41).
//!
//! ## The scan bound is the whole design
//!
//! A column may hold 200 million rows. Suggesting values from it must never
//! be a function of that number, so this module NEVER walks a column: it walks
//! at most [`SCAN_LIMIT`] rows, from a window around where the user is
//! actually typing, and stops. [`ScanBudget`] records how many rows were
//! really touched so a test can assert the bound rather than trust a comment —
//! `scan_over_200m_rows_touches_at_most_the_budget` does exactly that, and it
//! is the reason the counter is public API rather than a debug field.
//!
//! The distinct set is separately capped at [`MAX_DISTINCT`]. Both caps
//! matter: the scan bound keeps the TIME bounded, the distinct cap keeps the
//! MEMORY bounded when the scanned window happens to be all-unique.
//!
//! ## Why the string arena is used but not enumerated
//!
//! `StringArena` interns every text value once, so equality of text is
//! equality of [`StrId`] and a distinct set can be a set of 4-byte ids rather
//! than of owned strings. That is the arena's "existing distinct values" the
//! criterion asks for, and it is why [`Suggestions`] holds ids.
//!
//! What the arena CANNOT do is enumerate the distinct values *of one column*.
//! Its span table is workbook-wide, so `spans[i]` says nothing about which
//! column used it — and on a mapped base the arena's dedup index is dropped
//! after ingest (`StringArena::shrink_for_readonly`) so there is not even a
//! reverse lookup left. Enumerating the arena would therefore suggest values
//! from *other* columns, which is worse than useless in a spreadsheet where
//! column B is countries and column C is names. Hence: the ids come from the
//! arena, the *membership* comes from a bounded scan of the column itself.
//!
//! ## Escape
//!
//! Dismissal is deliberately not modelled here. [`Suggestions`] is a value
//! with no "open" flag; the UI holds whether the popup is showing and clears
//! it on Escape without touching the edit buffer. Putting the flag in the
//! model is how "Escape dismissed the popup AND reverted my typing" happens.

use crate::{Column, StrId, StringArena, Value};

/// Hard cap on rows touched by one suggestion scan.
///
/// 20,000 rows is a few hundred microseconds and is far more than enough to
/// find the distinct values a user is likely to retype. The number that
/// matters is that it is a CONSTANT: the scan costs the same on a 200-row
/// sheet and on a 200-million-row one.
pub const SCAN_LIMIT: usize = 20_000;

/// Cap on distinct values remembered, per the acceptance criterion (~10k).
pub const MAX_DISTINCT: usize = 10_000;

/// Cap on suggestions handed to the UI. A dropdown nobody can read is not a
/// feature; the rest are reachable by typing one more character.
pub const MAX_SUGGESTIONS: usize = 8;

/// Shortest prefix that triggers a suggestion.
///
/// One character. Zero would pop a list open the instant a cell is entered,
/// over the top of the value the user is trying to see.
pub const MIN_PREFIX: usize = 1;

// ============================================================== scan budget ==

/// How much work a scan was allowed, and how much it actually did.
///
/// `rows_examined` is real: it is incremented in the scan loop, not derived
/// afterwards from the range. That is what makes an assertion on it capable of
/// failing if someone later replaces the windowing with a full pass.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ScanBudget {
    pub limit: usize,
    pub rows_examined: usize,
    /// True when the column had more rows than the scan was allowed to look
    /// at, so the caller knows the distinct set is a sample, not a census.
    pub truncated: bool,
    /// True when [`MAX_DISTINCT`] was reached and further distinct values
    /// were dropped.
    pub distinct_capped: bool,
}

impl Default for ScanBudget {
    fn default() -> Self {
        Self {
            limit: SCAN_LIMIT,
            rows_examined: 0,
            truncated: false,
            distinct_capped: false,
        }
    }
}

impl ScanBudget {
    pub fn with_limit(limit: usize) -> Self {
        Self {
            limit,
            ..Default::default()
        }
    }
}

// ============================================================ distinct set ==

/// Distinct TEXT values seen in one column's scanned window, as arena ids.
///
/// Ids, not strings: the arena already owns exactly one copy of each distinct
/// string, so this set is 4 bytes per distinct value and costs nothing to
/// build. Capped at [`MAX_DISTINCT`] entries, so its footprint has a hard
/// ceiling of ~40 KB regardless of the column.
///
/// Numbers are deliberately absent. Autocompleting `1234` to `12345` while
/// someone types a figure is actively harmful, and a numeric column's distinct
/// set is the one that genuinely is row-count-sized.
#[derive(Clone, Debug, Default)]
pub struct DistinctValues {
    ids: Vec<StrId>,
    pub budget: ScanBudget,
}

impl DistinctValues {
    pub fn len(&self) -> usize {
        self.ids.len()
    }

    pub fn is_empty(&self) -> bool {
        self.ids.is_empty()
    }

    pub fn ids(&self) -> &[StrId] {
        &self.ids
    }

    pub fn heap_bytes(&self) -> usize {
        self.ids.capacity() * std::mem::size_of::<StrId>()
    }

    /// Scan a bounded window of `column` for distinct text ids.
    ///
    /// The window starts at `around` — the row being edited — and runs
    /// forward, wrapping to the start of the column once, so the values
    /// nearest the cursor are found first and a cell at row 199,999,000 does
    /// not suggest only from row 0. At most `budget.limit` rows are read;
    /// `budget.rows_examined` is the truth about how many.
    pub fn scan(column: &Column, around: usize, mut budget: ScanBudget) -> Self {
        let rows = column.len();
        let mut ids: Vec<StrId> = Vec::new();
        if rows == 0 || budget.limit == 0 {
            budget.truncated = rows > 0;
            return Self { ids, budget };
        }
        // Start a little BEFORE the cursor so a value typed into the row above
        // is offered on the row below — the commonest case there is.
        let back = budget.limit / 4;
        let start = around.saturating_sub(back).min(rows.saturating_sub(1));
        budget.truncated = rows > budget.limit;

        let mut seen: std::collections::HashSet<u32> = std::collections::HashSet::new();
        for step in 0..budget.limit {
            // Wrap once, so the window is contiguous in the column even when
            // the cursor is near its end.
            let row = (start + step) % rows;
            if step >= rows {
                break;
            }
            budget.rows_examined += 1;
            if let Value::Text(id) = column.get(row) {
                if ids.len() >= MAX_DISTINCT {
                    budget.distinct_capped = true;
                    // Keep counting rows — the bound being asserted is on rows
                    // read, and stopping early here would make the counter
                    // lie about what a full-budget scan costs.
                    continue;
                }
                if seen.insert(id.0) {
                    ids.push(id);
                }
            }
        }
        Self { ids, budget }
    }
}

// ============================================================= suggestions ==

/// What to offer for a partially typed cell.
///
/// Ordered: values that START WITH the typed prefix first, in the order they
/// were met in the column, then values that merely contain it. Prefix matches
/// first because that is what typing means; containment is a second chance for
/// someone who remembers the middle of a label.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Suggestions {
    /// Matching values, already resolved for display, capped at
    /// [`MAX_SUGGESTIONS`].
    pub items: Vec<String>,
    /// True when more matched than are listed.
    pub truncated: bool,
}

impl Suggestions {
    pub fn is_empty(&self) -> bool {
        self.items.is_empty()
    }

    pub fn len(&self) -> usize {
        self.items.len()
    }

    /// Rank `distinct` against what the user has typed.
    ///
    /// Case-insensitive, and a value identical to the prefix is NOT offered:
    /// suggesting the user the exact thing they already typed is noise, and it
    /// makes Enter ambiguous.
    pub fn rank(distinct: &DistinctValues, arena: &StringArena, prefix: &str) -> Self {
        let p = prefix.trim();
        if p.chars().count() < MIN_PREFIX {
            return Self::default();
        }
        let lower = p.to_lowercase();
        let mut starts: Vec<String> = Vec::new();
        let mut contains: Vec<String> = Vec::new();
        for id in &distinct.ids {
            let s = arena.resolve_or_empty(*id);
            if s.is_empty() {
                continue;
            }
            let sl = s.to_lowercase();
            if sl == lower {
                continue;
            }
            if sl.starts_with(&lower) {
                starts.push(s.to_string());
            } else if sl.contains(&lower) {
                contains.push(s.to_string());
            }
        }
        let total = starts.len() + contains.len();
        starts.extend(contains);
        starts.truncate(MAX_SUGGESTIONS);
        Self {
            items: starts,
            truncated: total > MAX_SUGGESTIONS,
        }
    }

    /// Suggestions restricted to an explicit list — a validation list rule.
    ///
    /// A cell with a list rule on it has an authoritative set of allowed
    /// values, so autocomplete offers those instead of whatever happens to be
    /// in the column. An empty prefix offers the whole list, because that is
    /// the dropdown.
    pub fn from_list(values: &[String], prefix: &str) -> Self {
        let lower = prefix.trim().to_lowercase();
        let mut items: Vec<String> = Vec::new();
        let mut total = 0usize;
        for v in values {
            let vl = v.to_lowercase();
            // A value identical to what is already typed is not offered — the
            // same rule `rank` follows, and here it is load-bearing rather
            // than merely tidy: accepting "Alpha" would otherwise leave a
            // popup containing exactly "Alpha", so the next Enter would accept
            // it again instead of committing, and the cell could never be
            // confirmed at all.
            if !lower.is_empty() && vl == lower {
                continue;
            }
            if lower.is_empty() || vl.contains(&lower) {
                total += 1;
                if items.len() < MAX_SUGGESTIONS {
                    items.push(v.clone());
                }
            }
        }
        Self {
            items,
            truncated: total > MAX_SUGGESTIONS,
        }
    }
}

#[cfg(test)]
mod tests;
