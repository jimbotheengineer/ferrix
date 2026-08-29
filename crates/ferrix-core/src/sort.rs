//! Column sort as a VIEW TRANSFORM: a visible-row -> underlying-row mapping.
//!
//! ## Nothing moves
//!
//! Sorting never rewrites the sheet, never touches the `.ferrix` base, and
//! never marks the workbook dirty. It produces a mapping in exactly the shape
//! [`crate::filter::RowFilter`] already produces, so the grid can resolve a
//! screen row through ONE path whether the view is filtered, sorted, both, or
//! neither.
//!
//! ## Why this is not an [`crate::order::AxisOrder`]
//!
//! `order.rs` stores a display permutation as contiguous ascending *runs*,
//! which is what makes column drag-reorder cost `O(k)` for `k` user edits
//! rather than `O(rows)`. That representation is perfect for a permutation
//! built from a handful of structural moves and actively wrong for one built
//! by sorting: a sort of `n` rows by an unsorted key is, in general, `n`
//! separate runs. Feeding it to `AxisOrder` would blow straight through
//! [`crate::order::AxisOrder::MAX_RUNS`] and, if it did not, would turn every
//! lookup into a binary search over `n` runs to answer what a flat index
//! answers in one load. So sort keeps its own flat index vector and leaves the
//! run engine to the job it is good at.
//!
//! ## The scale invariant
//!
//! A sort must NEVER materialise the column it sorts by. The only thing this
//! module allocates is an index vector over the CANDIDATE rows — 4 bytes per
//! row, plus 8 for the reverse lookup — and cells are read through the
//! caller's storage ([`CellKeys`]) during the comparison itself. On a
//! memory-mapped 12 GB sheet that means the OS pages in the key column and
//! nothing is ever copied into the heap. Peak memory is bounded by the index,
//! never by the cell payload.
//!
//! ## Ordering rules (Excel's, deliberately)
//!
//! * **Stable.** Equal keys keep their previous relative order, so a sort by
//!   B after a sort by A is a genuine secondary ordering.
//! * **Empty cells sort LAST in both directions.** Descending does not float
//!   blanks to the top; they stay at the bottom where a user reading a sorted
//!   column expects "no value" to live. A text cell holding "" counts as
//!   empty, because on screen it is indistinguishable from one.
//! * **Numbers numerically, text case-insensitively.**
//! * **Mixed columns sort by type tag first, then by value within the tag** —
//!   numbers, then text, then booleans, then errors — so a column of mostly
//!   numbers with a stray label does not interleave nonsensically.

use std::cmp::Ordering;

/// Which way a column is sorted.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SortDir {
    Asc,
    Desc,
}

impl SortDir {
    /// The next state in the header-click cycle: asc -> desc -> none.
    pub fn cycle(current: Option<SortDir>) -> Option<SortDir> {
        match current {
            None => Some(SortDir::Asc),
            Some(SortDir::Asc) => Some(SortDir::Desc),
            Some(SortDir::Desc) => None,
        }
    }

    /// Arrow shown in the column header.
    pub fn glyph(self) -> &'static str {
        match self {
            SortDir::Asc => "\u{25B2}",
            SortDir::Desc => "\u{25BC}",
        }
    }
}

/// One column of a (possibly multi-column) sort.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct SortKey {
    /// Display column index.
    pub col: u32,
    pub dir: SortDir,
}

impl SortKey {
    pub fn new(col: u32, dir: SortDir) -> Self {
        Self { col, dir }
    }
}

/// A cell as the sorter sees it: borrowed, never owned.
///
/// Text is a borrow out of the caller's arena/mmap, which is the whole point —
/// comparing two rows must not allocate, or a 200M-row sort would allocate
/// 200M strings.
#[derive(Clone, Copy, Debug)]
pub enum SortCell<'a> {
    Empty,
    Number(f64),
    Bool(bool),
    Text(&'a str),
    Error(&'a str),
}

impl SortCell<'_> {
    /// Empty for sorting purposes. A text cell holding nothing but whitespace
    /// reads as blank on screen, so it sorts as blank too.
    #[inline]
    pub fn is_blank(&self) -> bool {
        match self {
            SortCell::Empty => true,
            SortCell::Text(s) => s.trim().is_empty(),
            _ => false,
        }
    }

    /// Type rank, used before the value in a mixed column.
    #[inline]
    fn tag_rank(&self) -> u8 {
        match self {
            SortCell::Number(_) => 0,
            SortCell::Text(_) => 1,
            SortCell::Bool(_) => 2,
            SortCell::Error(_) => 3,
            SortCell::Empty => 4,
        }
    }
}

/// Case-insensitive comparison that allocates nothing.
///
/// `str::to_lowercase` would allocate a `String` per comparison — `n log n`
/// allocations, which on a large column is the difference between a sort that
/// finishes and one that thrashes. Folding lazily over the char iterators
/// costs nothing and handles multi-char lowercase mappings correctly.
fn cmp_text_ci(a: &str, b: &str) -> Ordering {
    let mut la = a.chars().flat_map(char::to_lowercase);
    let mut lb = b.chars().flat_map(char::to_lowercase);
    loop {
        match (la.next(), lb.next()) {
            (None, None) => return Ordering::Equal,
            (None, Some(_)) => return Ordering::Less,
            (Some(_), None) => return Ordering::Greater,
            (Some(x), Some(y)) => match x.cmp(&y) {
                Ordering::Equal => continue,
                other => return other,
            },
        }
    }
}

/// Compare two cells for ASCENDING order, ignoring blanks (handled above).
fn cmp_values(a: &SortCell<'_>, b: &SortCell<'_>) -> Ordering {
    match a.tag_rank().cmp(&b.tag_rank()) {
        Ordering::Equal => {}
        other => return other,
    }
    match (a, b) {
        // NaN cannot participate in a total order, so it is pinned after every
        // real number rather than making the comparator inconsistent — an
        // inconsistent comparator is allowed to panic inside sort_by.
        (SortCell::Number(x), SortCell::Number(y)) => match x.partial_cmp(y) {
            Some(o) => o,
            None => x.is_nan().cmp(&y.is_nan()),
        },
        (SortCell::Text(x), SortCell::Text(y)) => cmp_text_ci(x, y),
        (SortCell::Bool(x), SortCell::Bool(y)) => x.cmp(y),
        (SortCell::Error(x), SortCell::Error(y)) => x.cmp(y),
        _ => Ordering::Equal,
    }
}

/// Read access to the cells being sorted.
///
/// Implemented over whatever the caller's storage is — an in-RAM sheet, a
/// memory-mapped file, or a composite view with an edit overlay. The sorter
/// only ever calls this during a comparison, so the key column is streamed,
/// not copied.
pub trait CellKeys {
    fn key(&self, row: u32, col: u32) -> SortCell<'_>;
}

/// Compare two rows under a full multi-column key.
///
/// Blank-last is decided per key: a blank in the first key sinks the row
/// regardless of direction, and only rows that tie on it move on to the next.
fn cmp_rows(a: u32, b: u32, keys: &[SortKey], src: &impl CellKeys) -> Ordering {
    for k in keys {
        let (ka, kb) = (src.key(a, k.col), src.key(b, k.col));
        let (ba, bb) = (ka.is_blank(), kb.is_blank());
        // Blanks last in BOTH directions, so the direction flip below must
        // never see them.
        match (ba, bb) {
            (true, true) => continue,
            (true, false) => return Ordering::Greater,
            (false, true) => return Ordering::Less,
            (false, false) => {}
        }
        let ord = match k.dir {
            SortDir::Asc => cmp_values(&ka, &kb),
            SortDir::Desc => cmp_values(&kb, &ka),
        };
        if ord != Ordering::Equal {
            return ord;
        }
    }
    Ordering::Equal
}

/// Indirect-sort `rows` (underlying row indices) by `keys`.
///
/// `rows` is the CANDIDATE set: whatever the active filters left. Sorting the
/// filtered rows is what makes sort compose with filtering rather than compete
/// with it.
///
/// Stable, so ties keep the incoming order.
pub fn sort_rows(rows: &[u32], keys: &[SortKey], src: &impl CellKeys) -> Vec<u32> {
    let mut out = rows.to_vec();
    if keys.is_empty() {
        return out;
    }
    // `sort_by` is a stable merge sort. Nothing here reads a whole column:
    // each comparison pulls exactly the cells it needs through `src`.
    out.sort_by(|&a, &b| cmp_rows(a, b, keys, src));
    out
}

/// A sorted visible-row -> underlying-row mapping.
///
/// Same contract as [`crate::filter::RowFilter`], with one difference that
/// matters: the underlying rows are NOT ascending, so the reverse lookup
/// cannot be a binary search over the values. It gets its own sorted side
/// index — 8 bytes per row, still an index and still never a cell.
#[derive(Clone, Debug, Default)]
pub struct SortOrder {
    /// visible position -> underlying row.
    rows: Vec<u32>,
    /// (underlying row, visible position), ascending by row, for the reverse
    /// lookup.
    by_row: Vec<(u32, u32)>,
    keys: Vec<SortKey>,
}

impl SortOrder {
    /// Build the mapping by sorting `candidates` on `keys`.
    ///
    /// Runs ONCE per sort change, never per frame.
    pub fn build(candidates: &[u32], keys: &[SortKey], src: &impl CellKeys) -> Self {
        let rows = sort_rows(candidates, keys, src);
        Self::from_rows(rows, keys.to_vec())
    }

    /// Wrap an already-computed visible->underlying vector.
    pub fn from_rows(rows: Vec<u32>, keys: Vec<SortKey>) -> Self {
        let mut by_row: Vec<(u32, u32)> = rows
            .iter()
            .enumerate()
            .map(|(v, &r)| (r, v as u32))
            .collect();
        by_row.sort_unstable();
        Self { rows, by_row, keys }
    }

    pub fn keys(&self) -> &[SortKey] {
        &self.keys
    }

    /// Direction currently applied to a display column, if any.
    pub fn dir_of(&self, col: u32) -> Option<SortDir> {
        self.keys.iter().find(|k| k.col == col).map(|k| k.dir)
    }

    pub fn len(&self) -> usize {
        self.rows.len()
    }

    pub fn is_empty(&self) -> bool {
        self.rows.is_empty()
    }

    /// Visible row -> underlying row. `None` past the end.
    #[inline]
    pub fn underlying(&self, visible: usize) -> Option<u32> {
        self.rows.get(visible).copied()
    }

    /// Underlying row -> visible row, or `None` when it is not in the view.
    #[inline]
    pub fn visible_of(&self, underlying: u32) -> Option<usize> {
        self.by_row
            .binary_search_by_key(&underlying, |&(r, _)| r)
            .ok()
            .map(|i| self.by_row[i].1 as usize)
    }

    /// Visible position of `underlying`, or the nearest sensible landing spot
    /// when it is not in the view. Mirrors `RowFilter::visible_at_or_after`,
    /// which the app uses to keep the selection anchored.
    pub fn visible_at_or_after(&self, underlying: u32) -> usize {
        match self.visible_of(underlying) {
            Some(v) => v,
            None => {
                let i = self.by_row.partition_point(|&(r, _)| r < underlying);
                self.by_row.get(i).map(|&(_, v)| v as usize).unwrap_or(0)
            }
        }
    }

    /// All underlying rows, in visible order.
    pub fn rows(&self) -> &[u32] {
        &self.rows
    }

    /// Bytes held by the mapping. Index only — this is the number the scale
    /// invariant is about, and a test asserts it stays proportional to the row
    /// COUNT rather than to the size of the cells sorted.
    pub fn heap_bytes(&self) -> usize {
        self.rows.capacity() * std::mem::size_of::<u32>()
            + self.by_row.capacity() * std::mem::size_of::<(u32, u32)>()
            + self.keys.capacity() * std::mem::size_of::<SortKey>()
    }
}

/// Apply a header click to a sort spec: asc -> desc -> none on that column.
///
/// `additive` is the shift-click case: the column joins the existing spec as a
/// secondary key instead of replacing it ("sort by this, then by...").
pub fn cycle_click(keys: &mut Vec<SortKey>, col: u32, additive: bool) {
    let existing = keys.iter().position(|k| k.col == col);
    let current = existing.map(|i| keys[i].dir);
    let next = SortDir::cycle(current);
    if !additive {
        // A plain click replaces the whole spec: one column, one order.
        keys.clear();
        if let Some(dir) = next {
            keys.push(SortKey::new(col, dir));
        }
        return;
    }
    match (existing, next) {
        (Some(i), Some(dir)) => keys[i].dir = dir,
        (Some(i), None) => {
            keys.remove(i);
        }
        (None, Some(dir)) => keys.push(SortKey::new(col, dir)),
        (None, None) => {}
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A source that answers from a borrowed table of cells. Nothing here is
    /// copied per comparison, which is the property the real storage has too.
    struct Cells<'a> {
        cols: Vec<Vec<SortCell<'a>>>,
    }

    impl CellKeys for Cells<'_> {
        fn key(&self, row: u32, col: u32) -> SortCell<'_> {
            self.cols
                .get(col as usize)
                .and_then(|c| c.get(row as usize))
                .copied()
                .unwrap_or(SortCell::Empty)
        }
    }

    fn nums(v: &[f64]) -> Vec<SortCell<'static>> {
        v.iter().map(|&n| SortCell::Number(n)).collect()
    }

    fn all(n: usize) -> Vec<u32> {
        (0..n as u32).collect()
    }

    #[test]
    fn ascending_then_descending_by_number() {
        let c = Cells {
            cols: vec![nums(&[30.0, 10.0, 20.0])],
        };
        let rows = all(3);
        let asc = sort_rows(&rows, &[SortKey::new(0, SortDir::Asc)], &c);
        assert_eq!(asc, vec![1, 2, 0]);
        let desc = sort_rows(&rows, &[SortKey::new(0, SortDir::Desc)], &c);
        assert_eq!(desc, vec![0, 2, 1]);
    }

    #[test]
    fn text_sorts_case_insensitively() {
        let c = Cells {
            cols: vec![vec![
                SortCell::Text("banana"),
                SortCell::Text("Apple"),
                SortCell::Text("cherry"),
            ]],
        };
        let asc = sort_rows(&all(3), &[SortKey::new(0, SortDir::Asc)], &c);
        assert_eq!(
            asc,
            vec![1, 0, 2],
            "case-sensitive byte order would put 'Apple' first for the wrong \
             reason and 'banana' after 'Zebra'"
        );
        let c2 = Cells {
            cols: vec![vec![SortCell::Text("Zebra"), SortCell::Text("apple")]],
        };
        assert_eq!(
            sort_rows(&all(2), &[SortKey::new(0, SortDir::Asc)], &c2),
            vec![1, 0],
            "'apple' must precede 'Zebra'; raw byte order says otherwise"
        );
    }

    #[test]
    fn empty_cells_land_last_in_both_directions() {
        let c = Cells {
            cols: vec![vec![
                SortCell::Number(5.0),
                SortCell::Empty,
                SortCell::Number(1.0),
                SortCell::Text("   "),
                SortCell::Number(9.0),
            ]],
        };
        let asc = sort_rows(&all(5), &[SortKey::new(0, SortDir::Asc)], &c);
        assert_eq!(asc, vec![2, 0, 4, 1, 3], "blanks must sink ascending");
        let desc = sort_rows(&all(5), &[SortKey::new(0, SortDir::Desc)], &c);
        assert_eq!(
            desc,
            vec![4, 0, 2, 1, 3],
            "blanks must STILL sink descending — this is the Excel rule and \
             the one a naive reverse() gets wrong"
        );
    }

    #[test]
    fn sort_is_stable() {
        // Ten rows, five distinct keys, each appearing twice. A stable sort
        // keeps the lower original row first within every tie.
        let c = Cells {
            cols: vec![nums(&[2.0, 1.0, 2.0, 1.0, 2.0, 1.0, 2.0, 1.0, 2.0, 1.0])],
        };
        let asc = sort_rows(&all(10), &[SortKey::new(0, SortDir::Asc)], &c);
        assert_eq!(asc, vec![1, 3, 5, 7, 9, 0, 2, 4, 6, 8]);
        let desc = sort_rows(&all(10), &[SortKey::new(0, SortDir::Desc)], &c);
        assert_eq!(
            desc,
            vec![0, 2, 4, 6, 8, 1, 3, 5, 7, 9],
            "descending must stay stable too — ties keep ascending row order"
        );
    }

    #[test]
    fn mixed_types_group_by_tag_then_value() {
        let c = Cells {
            cols: vec![vec![
                SortCell::Text("beta"),
                SortCell::Number(2.0),
                SortCell::Bool(true),
                SortCell::Text("alpha"),
                SortCell::Number(1.0),
                SortCell::Empty,
            ]],
        };
        let asc = sort_rows(&all(6), &[SortKey::new(0, SortDir::Asc)], &c);
        assert_eq!(
            asc,
            vec![4, 1, 3, 0, 2, 5],
            "numbers, then text, then bools, then blanks"
        );
    }

    #[test]
    fn multi_column_sorts_by_the_second_key_within_ties() {
        let c = Cells {
            cols: vec![
                nums(&[1.0, 2.0, 1.0, 2.0]),
                vec![
                    SortCell::Text("d"),
                    SortCell::Text("b"),
                    SortCell::Text("c"),
                    SortCell::Text("a"),
                ],
            ],
        };
        let out = sort_rows(
            &all(4),
            &[SortKey::new(0, SortDir::Asc), SortKey::new(1, SortDir::Asc)],
            &c,
        );
        assert_eq!(out, vec![2, 0, 3, 1]);
        // Second key descending flips only within a first-key group.
        let out2 = sort_rows(
            &all(4),
            &[
                SortKey::new(0, SortDir::Asc),
                SortKey::new(1, SortDir::Desc),
            ],
            &c,
        );
        assert_eq!(out2, vec![0, 2, 1, 3]);
    }

    #[test]
    fn sorting_only_the_filtered_rows() {
        // The composition contract in miniature: the candidate set is what the
        // filter kept, and the output is a permutation OF THAT SET — never of
        // the whole sheet.
        let c = Cells {
            cols: vec![nums(&[50.0, 10.0, 40.0, 20.0, 30.0])],
        };
        let filtered = vec![1u32, 2, 4]; // rows the filter kept
        let out = sort_rows(&filtered, &[SortKey::new(0, SortDir::Asc)], &c);
        assert_eq!(out, vec![1, 4, 2]);
        assert!(
            !out.contains(&0) && !out.contains(&3),
            "a filtered-out row must not reappear because of a sort"
        );
    }

    #[test]
    fn the_click_cycle_is_asc_desc_none() {
        let mut keys: Vec<SortKey> = Vec::new();
        cycle_click(&mut keys, 2, false);
        assert_eq!(keys, vec![SortKey::new(2, SortDir::Asc)]);
        cycle_click(&mut keys, 2, false);
        assert_eq!(keys, vec![SortKey::new(2, SortDir::Desc)]);
        cycle_click(&mut keys, 2, false);
        assert!(keys.is_empty(), "the third click must clear the sort");
        // A different column starts its own cycle and replaces the old one.
        cycle_click(&mut keys, 1, false);
        cycle_click(&mut keys, 3, false);
        assert_eq!(keys, vec![SortKey::new(3, SortDir::Asc)]);
    }

    #[test]
    fn shift_click_appends_a_secondary_key() {
        let mut keys: Vec<SortKey> = Vec::new();
        cycle_click(&mut keys, 0, false);
        cycle_click(&mut keys, 1, true);
        assert_eq!(
            keys,
            vec![SortKey::new(0, SortDir::Asc), SortKey::new(1, SortDir::Asc)]
        );
        cycle_click(&mut keys, 1, true);
        assert_eq!(keys[1].dir, SortDir::Desc, "the secondary key cycles too");
        cycle_click(&mut keys, 1, true);
        assert_eq!(
            keys,
            vec![SortKey::new(0, SortDir::Asc)],
            "cycling a secondary key off drops just that key"
        );
    }

    #[test]
    fn mapping_round_trips_both_directions() {
        let c = Cells {
            cols: vec![nums(&[30.0, 10.0, 20.0])],
        };
        let o = SortOrder::build(&all(3), &[SortKey::new(0, SortDir::Asc)], &c);
        assert_eq!(o.len(), 3);
        assert_eq!(o.underlying(0), Some(1));
        assert_eq!(o.underlying(2), Some(0));
        assert_eq!(o.underlying(3), None);
        for v in 0..o.len() {
            let u = o.underlying(v).unwrap();
            assert_eq!(o.visible_of(u), Some(v), "visible {v} did not round-trip");
        }
        assert_eq!(o.dir_of(0), Some(SortDir::Asc));
        assert_eq!(o.dir_of(1), None);
    }

    #[test]
    fn reverse_lookup_works_on_a_non_ascending_mapping() {
        // RowFilter can binary-search its own values because they ascend. A
        // sorted mapping does not, and this is where a copied-from-RowFilter
        // reverse lookup would silently return the wrong visible row.
        let o = SortOrder::from_rows(vec![7, 2, 9, 0], Vec::new());
        assert_eq!(o.visible_of(7), Some(0));
        assert_eq!(o.visible_of(2), Some(1));
        assert_eq!(o.visible_of(9), Some(2));
        assert_eq!(o.visible_of(0), Some(3));
        assert_eq!(o.visible_of(5), None, "row 5 is not in the view");
    }

    /// A source that hands out borrows into a fixed word table and asserts it
    /// is never asked to build anything per row.
    struct LazyWords {
        rows: u32,
    }

    const WORDS: [&str; 8] = [
        "delta", "alpha", "echo", "bravo", "golf", "charlie", "hotel", "foxtrot",
    ];

    impl CellKeys for LazyWords {
        fn key(&self, row: u32, _col: u32) -> SortCell<'_> {
            assert!(row < self.rows);
            // Every tenth row is blank, so the blank-last rule is exercised at
            // scale too.
            if row % 10 == 9 {
                return SortCell::Empty;
            }
            SortCell::Text(WORDS[(row as usize * 7 + 3) % WORDS.len()])
        }
    }

    #[test]
    fn a_large_sort_never_materialises_the_column() {
        // THE scale invariant. One million rows of text keys: the mapping must
        // cost the INDEX (4 bytes per row, plus 8 for the reverse lookup) and
        // nothing at all per cell. If a future change collects keys into a
        // Vec<String> to sort them, this fails immediately — a million short
        // strings is ~40 MB of heap, an order of magnitude over the bound.
        const N: u32 = 1_000_000;
        let src = LazyWords { rows: N };
        let candidates: Vec<u32> = (0..N).collect();
        let o = SortOrder::build(&candidates, &[SortKey::new(0, SortDir::Asc)], &src);

        assert_eq!(o.len(), N as usize);
        let bound = N as usize * (4 + 8) + 4096;
        assert!(
            o.heap_bytes() <= bound,
            "sort mapping used {} bytes for {N} rows (bound {bound}); a \
             materialised key column is the only way to exceed this",
            o.heap_bytes()
        );

        // And it is genuinely sorted, blanks last.
        let mut prev: Option<&str> = None;
        let mut blanks_started = false;
        for v in 0..o.len() {
            let r = o.underlying(v).unwrap();
            match src.key(r, 0) {
                SortCell::Empty => blanks_started = true,
                SortCell::Text(s) => {
                    assert!(!blanks_started, "a value appeared after a blank");
                    if let Some(p) = prev {
                        assert!(p <= s, "{p} sorted before {s}");
                    }
                    prev = Some(s);
                }
                other => panic!("unexpected {other:?}"),
            }
        }
        assert!(blanks_started, "the blanks must be in there somewhere");
    }

    #[test]
    fn an_empty_key_set_is_the_identity() {
        let c = Cells {
            cols: vec![nums(&[3.0, 1.0, 2.0])],
        };
        assert_eq!(sort_rows(&all(3), &[], &c), vec![0, 1, 2]);
    }

    #[test]
    fn nan_does_not_break_the_comparator() {
        // An inconsistent comparator may panic inside sort_by. NaN has to be
        // pinned somewhere definite rather than compared honestly.
        let c = Cells {
            cols: vec![nums(&[f64::NAN, 2.0, f64::NAN, 1.0])],
        };
        let out = sort_rows(&all(4), &[SortKey::new(0, SortDir::Asc)], &c);
        assert_eq!(out.len(), 4);
        assert_eq!(&out[..2], &[3, 1], "real numbers come first, in order");
    }
}
