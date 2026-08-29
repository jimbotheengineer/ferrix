//! Subtotals as a VIEW TRANSFORM, exactly like [`crate::sort`].
//!
//! ## Nothing is inserted
//!
//! Excel's Subtotal command writes rows into the sheet. Ferrix does not: a
//! subtotal row is a SYNTHETIC screen row that exists only in the mapping the
//! grid resolves through. The consequences are the whole point of doing it
//! this way:
//!
//! * the underlying rows are byte-identical before and after, so removing
//!   subtotals restores the exact original view by dropping one struct;
//! * sort and filter keep working, because they operate on the DATA rows and
//!   this layer sits above them;
//! * the `.ferrix` base is never rewritten, and on a 200M-row sheet inserting
//!   a million subtotal rows would be exactly the kind of copy the scale
//!   invariant forbids.
//!
//! ## Composing through the ONE resolver
//!
//! There is no second row mapping. [`SubtotalPlan`] is built OVER the rows a
//! sort/filter already resolved — its input is a visible-position sequence,
//! not a data-row sequence — and it is consulted as a STAGE of
//! `RowResolver::resolve`, ahead of the mappings it wraps. So the resolver
//! answers "screen row 7 is the subtotal for group 2" or "screen row 8 is
//! whatever the sort/filter says visible position 6 is", and every caller
//! (painting, row headers, hit-testing, the cell editor) gets the same
//! answer from the same code.
//!
//! ## The scale invariant
//!
//! A plan holds ONE entry per GROUP, not per row: `(first_visible,
//! last_visible, key)` plus the running aggregate. A 200M-row sheet grouped
//! by a column with 500 distinct values is 500 entries. Grouping a column
//! where every value is distinct is the degenerate case and costs one entry
//! per row — which is why [`SubtotalPlan::build`] takes a `max_groups` cap
//! and refuses rather than silently allocating 200M entries. Refusing with a
//! number the user can read beats an out-of-memory kill.
//!
//! ## Grouping rule
//!
//! Excel's: a subtotal is inserted **at each change of value** in the group
//! column, over the rows in their CURRENT view order. Consecutive runs, not
//! global grouping — so subtotalling an unsorted column produces a subtotal
//! per run, which is the honest rendering of what the user is looking at.

use crate::dedupe::KeyCell;
use crate::Value;

/// Which aggregate a subtotal row shows.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SubtotalFn {
    Sum,
    Count,
    Average,
    Min,
    Max,
}

impl SubtotalFn {
    pub fn label(self) -> &'static str {
        match self {
            SubtotalFn::Sum => "Sum",
            SubtotalFn::Count => "Count",
            SubtotalFn::Average => "Average",
            SubtotalFn::Min => "Min",
            SubtotalFn::Max => "Max",
        }
    }
}

/// A running aggregate over one group's rows, held as scalars.
///
/// Four `f64`s and a count per GROUP. Nothing here grows with the group's
/// size, which is what lets a 200M-row group cost the same as a 3-row one.
#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct Agg {
    pub count: u64,
    /// Numeric cells seen. `count` includes non-numeric ones; this does not.
    pub numeric: u64,
    pub sum: f64,
    pub min: f64,
    pub max: f64,
}

impl Agg {
    fn new() -> Self {
        Self {
            count: 0,
            numeric: 0,
            sum: 0.0,
            min: f64::INFINITY,
            max: f64::NEG_INFINITY,
        }
    }

    fn push(&mut self, v: Value) {
        self.count += 1;
        if let Value::Number(n) = v {
            self.numeric += 1;
            self.sum += n;
            self.min = self.min.min(n);
            self.max = self.max.max(n);
        }
    }

    /// The value a subtotal row shows for `f`, or `None` when there is
    /// nothing to show (an average of no numbers is not zero).
    pub fn value(&self, f: SubtotalFn) -> Option<f64> {
        match f {
            SubtotalFn::Count => Some(self.count as f64),
            SubtotalFn::Sum => (self.numeric > 0).then_some(self.sum),
            SubtotalFn::Average => (self.numeric > 0).then(|| self.sum / self.numeric as f64),
            SubtotalFn::Min => (self.numeric > 0).then_some(self.min),
            SubtotalFn::Max => (self.numeric > 0).then_some(self.max),
        }
    }
}

/// One group: a run of equal values in the group column.
#[derive(Clone, Debug, PartialEq)]
pub struct Group {
    /// First VISIBLE position (pre-subtotal space) in the group.
    pub first: usize,
    /// Last VISIBLE position (pre-subtotal space) in the group, inclusive.
    pub last: usize,
    /// The group column's value, as text for the label. One short string per
    /// GROUP, never per row.
    pub label: String,
    /// Aggregates, one per aggregated column, in `agg_cols` order.
    pub aggs: Vec<Agg>,
}

impl Group {
    pub fn row_count(&self) -> usize {
        self.last - self.first + 1
    }
}

/// What a screen row is, once subtotals are in play.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SubRow {
    /// A real row. The payload is a position in the space the plan was built
    /// over — i.e. the index to hand to the sort/filter mapping BELOW this
    /// stage, never a data row this layer invented.
    Data(usize),
    /// A synthetic subtotal row for `group`.
    Subtotal(usize),
}

/// Why a subtotal plan could not be built.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum SubtotalError {
    /// More groups than the cap allows. Carries both numbers so the message
    /// can say what actually happened.
    TooManyGroups { found: usize, max: usize },
}

impl std::fmt::Display for SubtotalError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            SubtotalError::TooManyGroups { found, max } => write!(
                f,
                "grouping would create {found} subtotal rows (limit {max}) — \
                 sort by that column first, or group by one with fewer \
                 distinct values"
            ),
        }
    }
}

/// Default cap on groups.
///
/// A subtotal per group is the only per-group allocation, and 100k of them is
/// a few megabytes — far past any readable outline, small enough that hitting
/// the cap is a message rather than a swap storm.
pub const MAX_GROUPS: usize = 100_000;

/// Read access to the rows being subtotalled, in VIEW space.
///
/// `visible` is a position in whatever mapping is already active. The
/// implementor resolves it — through the sort, the filter, or neither — which
/// is precisely why this layer needs no mapping of its own.
pub trait GroupSource {
    /// The group column's value at a visible position.
    fn group_value(&self, visible: usize) -> Value;
    /// A display string for the group column's value.
    fn group_label(&self, visible: usize) -> String;
    /// An aggregated column's value at a visible position.
    fn agg_value(&self, visible: usize, col: u32) -> Value;
}

/// The subtotal mapping: screen row -> [`SubRow`].
///
/// Holds one [`Group`] per run and a prefix index for the reverse lookup.
/// There is no per-row storage anywhere in here.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct SubtotalPlan {
    groups: Vec<Group>,
    /// Display column grouped on.
    group_col: u32,
    /// Display columns aggregated, in the order [`Group::aggs`] holds them.
    agg_cols: Vec<u32>,
    func: Option<SubtotalFn>,
    /// Rows the plan was built over, before subtotal rows were added.
    base_rows: usize,
}

impl SubtotalPlan {
    /// Build a plan over `rows` visible positions.
    ///
    /// `rows` is the count the mapping BELOW this stage resolves — the sort's
    /// length, the filter's length, or the sheet's row count when neither is
    /// active. Every index this plan produces is in that same space.
    pub fn build(
        rows: usize,
        group_col: u32,
        agg_cols: Vec<u32>,
        func: SubtotalFn,
        src: &impl GroupSource,
        max_groups: usize,
    ) -> Result<Self, SubtotalError> {
        let mut groups: Vec<Group> = Vec::new();
        let mut current: Option<KeyCell> = None;
        for v in 0..rows {
            let key = KeyCell::of(src.group_value(v));
            // A CHANGE OF VALUE starts a new group — Excel's rule, and the
            // reason this is a run detector rather than a hash grouping.
            if current != Some(key) {
                if groups.len() >= max_groups {
                    return Err(SubtotalError::TooManyGroups {
                        // One more than the cap is all we know without
                        // scanning on; report the cap being exceeded rather
                        // than a fabricated total.
                        found: groups.len() + 1,
                        max: max_groups,
                    });
                }
                current = Some(key);
                groups.push(Group {
                    first: v,
                    last: v,
                    label: src.group_label(v),
                    aggs: vec![Agg::new(); agg_cols.len()],
                });
            }
            let g = groups.last_mut().expect("a group was just pushed");
            g.last = v;
            for (i, &c) in agg_cols.iter().enumerate() {
                g.aggs[i].push(src.agg_value(v, c));
            }
        }
        Ok(Self {
            groups,
            group_col,
            agg_cols,
            func: Some(func),
            base_rows: rows,
        })
    }

    pub fn groups(&self) -> &[Group] {
        &self.groups
    }

    pub fn group_col(&self) -> u32 {
        self.group_col
    }

    pub fn agg_cols(&self) -> &[u32] {
        &self.agg_cols
    }

    pub fn func(&self) -> Option<SubtotalFn> {
        self.func
    }

    pub fn is_empty(&self) -> bool {
        self.groups.is_empty()
    }

    /// Screen rows the plan produces: the rows it was built over plus one per
    /// group.
    pub fn len(&self) -> usize {
        self.base_rows + self.groups.len()
    }

    /// How many rows the plan was built over.
    pub fn base_rows(&self) -> usize {
        self.base_rows
    }

    /// Index of the group whose screen span contains row `r`.
    ///
    /// Group `i` occupies screen rows `first_i + i ..= last_i + i + 1`, the
    /// last of which is its subtotal row. `last_i + i + 1` is strictly
    /// increasing in `i`, so this is one binary search over GROUPS —
    /// `O(log groups)`, independent of the row count, which is what keeps it
    /// affordable in the paint loop.
    fn group_index(&self, r: usize) -> usize {
        let (mut lo, mut hi) = (0usize, self.groups.len());
        while lo < hi {
            let mid = (lo + hi) / 2;
            if self.groups[mid].last + mid + 1 < r {
                lo = mid + 1;
            } else {
                hi = mid;
            }
        }
        lo
    }

    /// Screen row -> what it shows. `None` past the end.
    ///
    /// This is the stage function `RowResolver` calls. `SubRow::Data(v)` is a
    /// position to feed to the NEXT stage down, not a data row.
    pub fn resolve(&self, r: usize) -> Option<SubRow> {
        if r >= self.len() {
            return None;
        }
        if self.groups.is_empty() {
            return Some(SubRow::Data(r));
        }
        let i = self.group_index(r).min(self.groups.len() - 1);
        let g = &self.groups[i];
        // Screen space for group i: data rows at first+i ..= last+i, then the
        // subtotal at last+i+1.
        let sub_at = g.last + i + 1;
        if r == sub_at {
            Some(SubRow::Subtotal(i))
        } else {
            Some(SubRow::Data(r - i))
        }
    }

    /// Visible position -> the screen row it now occupies.
    ///
    /// The inverse of [`Self::resolve`] for real rows. A caller holding a
    /// pre-subtotal position (the selection, the cell editor) uses this to
    /// find where the row moved to.
    pub fn screen_of(&self, visible: usize) -> Option<usize> {
        if visible >= self.base_rows {
            return None;
        }
        // Groups that END before this position each contributed one subtotal
        // row above it.
        let before = self.groups.partition_point(|g| g.last < visible);
        Some(visible + before)
    }

    /// The label and aggregate a subtotal row draws in `col`.
    ///
    /// `None` for a column the plan does not aggregate, which is what leaves
    /// the rest of a subtotal row blank rather than repeating a number across
    /// it.
    pub fn cell(&self, group: usize, col: u32) -> Option<SubtotalCell> {
        let g = self.groups.get(group)?;
        let f = self.func?;
        if col == self.group_col {
            return Some(SubtotalCell::Label(format!("{} {}", g.label, f.label())));
        }
        let i = self.agg_cols.iter().position(|&c| c == col)?;
        Some(match g.aggs[i].value(f) {
            Some(v) => SubtotalCell::Number(v),
            None => SubtotalCell::Blank,
        })
    }

    /// Bytes held by the plan. One entry per GROUP — the number the scale
    /// invariant is about, asserted in the tests below.
    pub fn heap_bytes(&self) -> usize {
        let per_group =
            std::mem::size_of::<Group>() + self.agg_cols.len() * std::mem::size_of::<Agg>();
        self.groups.capacity() * per_group
            + self
                .groups
                .iter()
                .map(|g| g.label.capacity())
                .sum::<usize>()
            + self.agg_cols.capacity() * std::mem::size_of::<u32>()
    }
}

/// What a subtotal row shows in one column.
#[derive(Clone, Debug, PartialEq)]
pub enum SubtotalCell {
    /// "East Sum" in the group column.
    Label(String),
    Number(f64),
    /// An aggregated column with nothing numeric in the group.
    Blank,
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A source over a literal table of (group value, aggregated value).
    struct Rows {
        group: Vec<&'static str>,
        vals: Vec<f64>,
    }

    impl GroupSource for Rows {
        fn group_value(&self, v: usize) -> Value {
            // Text is compared through the arena in production; here the run
            // detector only needs distinct keys, so a stable per-string index
            // stands in for an arena id.
            let s = self.group[v];
            let idx = self
                .group
                .iter()
                .position(|x| *x == s)
                .expect("value comes from the vec");
            Value::Number(idx as f64)
        }
        fn group_label(&self, v: usize) -> String {
            self.group[v].to_string()
        }
        fn agg_value(&self, v: usize, _col: u32) -> Value {
            Value::Number(self.vals[v])
        }
    }

    fn plan(group: &[&'static str], vals: &[f64], f: SubtotalFn) -> SubtotalPlan {
        let src = Rows {
            group: group.to_vec(),
            vals: vals.to_vec(),
        };
        SubtotalPlan::build(group.len(), 0, vec![1], f, &src, MAX_GROUPS).expect("under the cap")
    }

    /// The screen as a list, for per-row identity assertions.
    fn screen(p: &SubtotalPlan) -> Vec<SubRow> {
        (0..p.len()).map(|r| p.resolve(r).unwrap()).collect()
    }

    #[test]
    fn a_subtotal_row_lands_at_each_change_of_value() {
        let p = plan(
            &["East", "East", "West", "West", "West"],
            &[1.0, 2.0, 3.0, 4.0, 5.0],
            SubtotalFn::Sum,
        );
        use SubRow::*;
        assert_eq!(
            screen(&p),
            vec![
                Data(0),
                Data(1),
                Subtotal(0),
                Data(2),
                Data(3),
                Data(4),
                Subtotal(1),
            ],
            "PER-ROW identity: every original row must still be reachable, in \
             order, with exactly one subtotal after each run"
        );
        assert_eq!(p.len(), 7, "5 data rows + 2 subtotals");
    }

    #[test]
    fn the_subtotal_row_shows_the_group_aggregate() {
        let p = plan(
            &["East", "East", "West"],
            &[10.0, 5.0, 100.0],
            SubtotalFn::Sum,
        );
        assert_eq!(p.cell(0, 1), Some(SubtotalCell::Number(15.0)));
        assert_eq!(p.cell(1, 1), Some(SubtotalCell::Number(100.0)));
        assert_eq!(
            p.cell(0, 0),
            Some(SubtotalCell::Label("East Sum".to_string()))
        );
        assert_eq!(
            p.cell(0, 9),
            None,
            "an un-aggregated column stays blank rather than repeating the sum"
        );
    }

    #[test]
    fn every_aggregate_function_reports_its_own_answer() {
        let g = ["A", "A", "A"];
        let v = [2.0, 8.0, 5.0];
        for (f, want) in [
            (SubtotalFn::Sum, 15.0),
            (SubtotalFn::Count, 3.0),
            (SubtotalFn::Average, 5.0),
            (SubtotalFn::Min, 2.0),
            (SubtotalFn::Max, 8.0),
        ] {
            let p = plan(&g, &v, f);
            assert_eq!(
                p.cell(0, 1),
                Some(SubtotalCell::Number(want)),
                "{} over {v:?}",
                f.label()
            );
        }
    }

    #[test]
    fn a_repeated_value_after_a_gap_is_a_second_group() {
        // Excel subtotals runs, not global groups. An unsorted column
        // therefore gets a subtotal per RUN, which is the honest rendering.
        let p = plan(&["East", "West", "East"], &[1.0, 2.0, 4.0], SubtotalFn::Sum);
        assert_eq!(p.groups().len(), 3, "three runs, not two distinct values");
        assert_eq!(p.cell(0, 1), Some(SubtotalCell::Number(1.0)));
        assert_eq!(p.cell(2, 1), Some(SubtotalCell::Number(4.0)));
    }

    #[test]
    fn screen_of_is_the_exact_inverse_of_resolve() {
        let p = plan(
            &["A", "A", "B", "C", "C", "C"],
            &[1.0, 2.0, 3.0, 4.0, 5.0, 6.0],
            SubtotalFn::Sum,
        );
        for v in 0..p.base_rows() {
            let s = p.screen_of(v).expect("every data row has a screen row");
            assert_eq!(
                p.resolve(s),
                Some(SubRow::Data(v)),
                "screen_of({v}) = {s} must resolve back to Data({v}) — a \
                 mismatch here is the in-cell editor drawing over the wrong row"
            );
        }
        assert_eq!(p.screen_of(p.base_rows()), None, "past the end");
    }

    #[test]
    fn removing_subtotals_restores_the_exact_original_view() {
        let g = ["A", "A", "B", "B", "C"];
        let v = [1.0, 2.0, 3.0, 4.0, 5.0];
        let p = plan(&g, &v, SubtotalFn::Sum);
        // Dropping the plan is what "remove subtotals" does. The identity the
        // resolver falls back to must be the original sequence, exactly.
        let with: Vec<usize> = screen(&p)
            .into_iter()
            .filter_map(|r| match r {
                SubRow::Data(d) => Some(d),
                SubRow::Subtotal(_) => None,
            })
            .collect();
        assert_eq!(
            with,
            (0..g.len()).collect::<Vec<_>>(),
            "every original row, in the original order, with nothing dropped \
             or duplicated — a count-only assertion would pass even if two \
             rows swapped"
        );
    }

    #[test]
    fn an_empty_view_produces_an_empty_plan() {
        let p = plan(&[], &[], SubtotalFn::Sum);
        assert!(p.is_empty());
        assert_eq!(p.len(), 0);
        assert_eq!(p.resolve(0), None);
    }

    #[test]
    fn text_in_an_aggregated_column_is_counted_but_not_summed() {
        struct Mixed;
        impl GroupSource for Mixed {
            fn group_value(&self, _v: usize) -> Value {
                Value::Number(0.0)
            }
            fn group_label(&self, _v: usize) -> String {
                "All".into()
            }
            fn agg_value(&self, v: usize, _c: u32) -> Value {
                if v == 1 {
                    Value::Text(crate::StrId(7))
                } else {
                    Value::Number(10.0)
                }
            }
        }
        let p = SubtotalPlan::build(3, 0, vec![1], SubtotalFn::Sum, &Mixed, MAX_GROUPS).unwrap();
        assert_eq!(p.cell(0, 1), Some(SubtotalCell::Number(20.0)));
        let c = SubtotalPlan::build(3, 0, vec![1], SubtotalFn::Count, &Mixed, MAX_GROUPS).unwrap();
        assert_eq!(
            c.cell(0, 1),
            Some(SubtotalCell::Number(3.0)),
            "COUNT counts the row; SUM ignores the text"
        );
        let a =
            SubtotalPlan::build(3, 0, vec![1], SubtotalFn::Average, &Mixed, MAX_GROUPS).unwrap();
        assert_eq!(
            a.cell(0, 1),
            Some(SubtotalCell::Number(10.0)),
            "AVERAGE divides by the NUMERIC count, not the row count"
        );
    }

    #[test]
    fn a_group_with_no_numbers_shows_blank_not_zero() {
        struct AllText;
        impl GroupSource for AllText {
            fn group_value(&self, _v: usize) -> Value {
                Value::Number(0.0)
            }
            fn group_label(&self, _v: usize) -> String {
                "T".into()
            }
            fn agg_value(&self, _v: usize, _c: u32) -> Value {
                Value::Empty
            }
        }
        let p = SubtotalPlan::build(3, 0, vec![1], SubtotalFn::Sum, &AllText, MAX_GROUPS).unwrap();
        assert_eq!(
            p.cell(0, 1),
            Some(SubtotalCell::Blank),
            "a sum of nothing is not 0 — showing 0 would invent data"
        );
    }

    #[test]
    fn too_many_groups_is_refused_with_a_number_not_an_allocation() {
        let g: Vec<&'static str> = vec!["a", "b", "c", "d", "e"];
        let src = Rows {
            group: g.clone(),
            vals: vec![1.0; 5],
        };
        // Every value distinct: 5 runs against a cap of 3.
        let err = SubtotalPlan::build(5, 0, vec![1], SubtotalFn::Sum, &src, 3).unwrap_err();
        assert_eq!(
            err,
            SubtotalError::TooManyGroups { found: 4, max: 3 },
            "the build must stop AT the cap rather than finishing the scan"
        );
        assert!(err.to_string().contains("limit 3"));
    }

    /// THE scale assertion for subtotals: one entry per GROUP.
    #[test]
    fn a_plan_costs_groups_not_rows() {
        struct Big {
            rows: usize,
            groups: usize,
        }
        impl GroupSource for Big {
            fn group_value(&self, v: usize) -> Value {
                Value::Number((v / (self.rows / self.groups)) as f64)
            }
            fn group_label(&self, v: usize) -> String {
                format!("g{}", v / (self.rows / self.groups))
            }
            fn agg_value(&self, v: usize, _c: u32) -> Value {
                Value::Number(v as f64)
            }
        }
        const ROWS: usize = 2_000_000;
        const GROUPS: usize = 100;
        let src = Big {
            rows: ROWS,
            groups: GROUPS,
        };
        let p = SubtotalPlan::build(ROWS, 0, vec![1], SubtotalFn::Sum, &src, MAX_GROUPS).unwrap();
        assert_eq!(p.groups().len(), GROUPS);
        assert_eq!(p.len(), ROWS + GROUPS);
        let one_u32_per_row = ROWS * 4;
        assert!(
            p.heap_bytes() * 100 < one_u32_per_row,
            "plan held {} bytes for {GROUPS} groups over {ROWS} rows — it \
             must be far below even one u32 per row ({one_u32_per_row})",
            p.heap_bytes()
        );
        // Resolution is still exact at the far end.
        assert_eq!(
            p.resolve(p.len() - 1),
            Some(SubRow::Subtotal(GROUPS - 1)),
            "the last screen row is the last group's subtotal"
        );
        assert_eq!(p.resolve(p.len() - 2), Some(SubRow::Data(ROWS - 1)));
    }
}
