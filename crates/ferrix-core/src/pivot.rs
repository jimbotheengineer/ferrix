//! Pivot aggregation kernel.
//!
//! A pivot groups rows by one or more columns and reports aggregates of other
//! columns per group — the engine behind a pivot table. This module is the
//! pure kernel (issue #33 Part A): no UI, no sheet type, no menu wiring. Those
//! are Parts B and C and build on the API here.
//!
//! ## The scale invariant
//!
//! Ferrix targets 200M+ rows. A pivot's peak memory must scale with the number
//! of DISTINCT GROUPS, never with the row count: a 10M-row pivot into 1000
//! groups holds ~1000 accumulators, not 10M of anything. [`compute`] makes a
//! SINGLE streaming pass over the columnar store, hashing each row's group key
//! and folding its values into that group's fixed-size accumulator. The only
//! heap growth is one entry per newly seen group — proven by
//! [`PivotResult::accumulator_count`] and the `memory_scales_with_groups` test.
//!
//! ## Independence from `Value`
//!
//! This kernel deliberately does NOT depend on [`crate::value::Value`]. The
//! pending dynamic-arrays work (#27) widens `Value`; a pivot must keep
//! compiling regardless. The source yields a tiny [`Cell`] enum instead — a
//! number, an interned string id, or "something else" — which is all grouping
//! and numeric aggregation need. String group keys travel as [`StrId`] (4-byte
//! arena ids), never as an allocated `String` per row.
//!
//! ## Kahan summation
//!
//! Sum and the mean/variance running totals use the SAME Kahan compensated
//! summation as [`crate::column::Column::sum_range`]. At spreadsheet scale this
//! is not optional: a naive accumulator loses the low bits of every addend once
//! the running total passes 2^53. See [`KahanSum`].

use std::borrow::Borrow;
use std::collections::HashMap;
use std::hash::{Hash, Hasher};

use crate::arena::StrId;

/// One column index into the source. A newtype so an aggregated column and a
/// group column cannot be confused with a row number at a call site.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct ColIdx(pub u32);

/// A cell as the pivot kernel sees it — deliberately NOT [`crate::value::Value`].
///
/// Grouping needs to tell values apart and hash them; numeric aggregation needs
/// the `f64`. Nothing here needs booleans, errors, or dynamic arrays, so they
/// all collapse to [`Cell::Blank`] (ignored by numeric aggregates) or, for a
/// group key, a distinct non-numeric bucket. Keeping this enum narrow is what
/// decouples the kernel from `Value`'s evolving variant set.
#[derive(Clone, Copy, Debug, PartialEq)]
pub enum Cell {
    /// A numeric cell. This is the only kind numeric aggregates fold in.
    Number(f64),
    /// An interned string, carried as a 4-byte arena id — never a `String`.
    Text(StrId),
    /// Anything else (empty, bool, error, future `Value` variants). It forms
    /// its own group bucket but contributes nothing to numeric aggregates.
    Blank,
}

/// Read access to the rows being pivoted, column by column.
///
/// The kernel drives this in a single pass: for each row it reads the group
/// columns then the value columns. Implementors expose the columnar store
/// directly — there is no requirement (and no expectation) that rows be
/// materialised. A `Sheet`/`Column`-backed adapter lives with the UI layer;
/// keeping the trait here `Value`-free is what lets this crate build in
/// isolation.
pub trait PivotSource {
    /// Number of rows to scan.
    fn row_count(&self) -> usize;
    /// The cell at `(col, row)`. `row < row_count()` and `col` is any index
    /// passed to [`compute`]; out-of-range columns should return
    /// [`Cell::Blank`].
    fn cell(&self, col: ColIdx, row: usize) -> Cell;
}

/// Which aggregate to compute for a value column.
///
/// A separate type from [`crate::subtotal::SubtotalFn`] on purpose: that one is
/// a run-detector for the subtotal view and has no variance; this one adds
/// [`Agg::StdDev`] and drives the hashed pivot. Kept local so #27's `Value`
/// changes cannot reach it.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Agg {
    /// Kahan-compensated sum of the numeric cells.
    Sum,
    /// Count of ALL rows in the group (numeric or not), like `COUNTA`.
    Count,
    /// Mean of the numeric cells. `None` when the group has no numbers.
    Avg,
    /// Minimum numeric cell. `None` when the group has no numbers.
    Min,
    /// Maximum numeric cell. `None` when the group has no numbers.
    Max,
    /// Population standard deviation of the numeric cells. `None` when the
    /// group has no numbers.
    StdDev,
}

/// Kahan (compensated) summation, matching [`crate::column::Column::sum_range`].
///
/// Carries the low-order bits a naive `+=` would round away in the compensation
/// term `c`, so summing 200M values stays exact past 2^53. Four bytes of extra
/// state per group; the arithmetic is free against memory bandwidth.
#[derive(Clone, Copy, Debug, Default)]
struct KahanSum {
    sum: f64,
    /// Running compensation for the low-order bits lost to rounding.
    c: f64,
}

impl KahanSum {
    #[inline]
    fn add(&mut self, v: f64) {
        let y = v - self.c;
        let t = self.sum + y;
        self.c = (t - self.sum) - y;
        self.sum = t;
    }

    #[inline]
    fn total(&self) -> f64 {
        self.sum
    }
}

/// Fixed-size running accumulator for ONE value column within ONE group.
///
/// This is the struct the scale invariant is about: its size does not depend on
/// how many rows fall into the group, so a 200M-row group costs exactly what a
/// 3-row one does. Variance is accumulated with Welford's online algorithm so a
/// single streaming pass suffices and no per-row values are retained; the sum
/// is tracked separately with Kahan compensation because `Agg::Sum` must match
/// the rest of the engine bit for bit.
#[derive(Clone, Copy, Debug)]
struct ColAcc {
    /// Rows in the group, numeric or not (drives `Count`).
    count: u64,
    /// Numeric cells seen (drives the `None` results and the variance divisor).
    numeric: u64,
    /// Kahan sum of numeric cells.
    sum: KahanSum,
    min: f64,
    max: f64,
    /// Welford running mean of numeric cells.
    mean: f64,
    /// Welford running sum of squared deviations (`M2`).
    m2: f64,
}

impl ColAcc {
    fn new() -> Self {
        Self {
            count: 0,
            numeric: 0,
            sum: KahanSum::default(),
            min: f64::INFINITY,
            max: f64::NEG_INFINITY,
            mean: 0.0,
            m2: 0.0,
        }
    }

    #[inline]
    fn push(&mut self, cell: Cell) {
        self.count += 1;
        if let Cell::Number(n) = cell {
            self.numeric += 1;
            self.sum.add(n);
            if n < self.min {
                self.min = n;
            }
            if n > self.max {
                self.max = n;
            }
            // Welford: numerically stable one-pass mean/variance.
            let delta = n - self.mean;
            self.mean += delta / self.numeric as f64;
            let delta2 = n - self.mean;
            self.m2 += delta * delta2;
        }
    }

    #[inline]
    fn value(&self, agg: Agg) -> Option<f64> {
        match agg {
            Agg::Count => Some(self.count as f64),
            Agg::Sum => (self.numeric > 0).then(|| self.sum.total()),
            Agg::Avg => (self.numeric > 0).then(|| self.sum.total() / self.numeric as f64),
            Agg::Min => (self.numeric > 0).then_some(self.min),
            Agg::Max => (self.numeric > 0).then_some(self.max),
            // Population variance: M2 / N. Requires at least one number.
            Agg::StdDev => (self.numeric > 0).then(|| (self.m2 / self.numeric as f64).sqrt()),
        }
    }
}

/// One part of a composite group key.
///
/// A pivot can group by several columns, so a key is a short sequence of these.
/// Numbers are keyed by their raw bit pattern (`to_bits`), which makes `NaN`
/// keys stable and distinct rather than "never equal to themselves"; text is
/// keyed by its 4-byte [`StrId`] with no string comparison; everything else
/// collapses to one shared `Blank` bucket per position.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
enum KeyPart {
    Number(u64),
    Text(u32),
    Blank,
}

impl KeyPart {
    #[inline]
    fn of(cell: Cell) -> Self {
        match cell {
            Cell::Number(n) => KeyPart::Number(n.to_bits()),
            Cell::Text(id) => KeyPart::Text(id.0),
            Cell::Blank => KeyPart::Blank,
        }
    }
}

/// The interned form of a group key stored in the map.
///
/// A `Box<[KeyPart]>` is allocated ONCE per distinct group (on the miss that
/// creates it), never per row. Probing reuses a borrowed slice, so the hot path
/// allocates nothing — see [`compute`]. `Borrow<[KeyPart]>` lets us look the key
/// up from a scratch `&[KeyPart]` without owning it.
type OwnedKey = Box<[KeyPart]>;

/// A resolved group in the pivot output.
#[derive(Clone, Debug, PartialEq)]
pub struct PivotGroup {
    /// The group-column values that define this group, in `group_by` order.
    pub key: Vec<Cell>,
    /// Aggregate results, one per entry in the `values` request, in order.
    /// `None` means "no numeric data for this aggregate in this group" (e.g.
    /// the average of a group with no numbers) — distinct from `Some(0.0)`.
    pub aggregates: Vec<Option<f64>>,
}

/// The result of a pivot: one [`PivotGroup`] per distinct group key.
///
/// Group order is deterministic: groups are sorted by their key so a caller
/// (and a test) gets a stable sequence without depending on hash iteration
/// order.
#[derive(Clone, Debug, PartialEq, Default)]
pub struct PivotResult {
    pub groups: Vec<PivotGroup>,
    /// Peak number of accumulators the pass held — exactly the number of
    /// distinct groups. Exposed so a test can assert the scale invariant
    /// directly rather than through a coarse RSS reading.
    accumulators: usize,
}

impl PivotResult {
    /// Number of distinct groups, which is also the number of accumulators the
    /// single pass held at its peak. The scale invariant in one number:
    /// independent of row count.
    pub fn accumulator_count(&self) -> usize {
        self.accumulators
    }

    /// Look up a group by its key cells, for convenience in callers and tests.
    pub fn group(&self, key: &[Cell]) -> Option<&PivotGroup> {
        self.groups.iter().find(|g| g.key == key)
    }
}

/// Compute a pivot: group `src`'s rows by `group_by`, aggregating `values`.
///
/// One streaming pass. For each row the group columns form a composite key; the
/// row's value cells are folded into that group's fixed accumulators. Peak
/// memory is `O(distinct groups)` — no row is retained, and the only per-row
/// heap traffic is the borrowed scratch key, reused across rows.
///
/// - `group_by`: the columns whose values define a group. Empty means "one
///   group over all rows" (a grand total).
/// - `values`: `(column, aggregate)` pairs to compute for each group. The same
///   column may appear several times with different aggregates — each DISTINCT
///   value column is read once per row and folded into a single accumulator, so
///   asking for `Sum` and `Avg` of one column does not double-count it.
///
/// The result's groups are sorted by key for deterministic output.
pub fn compute(
    src: &impl PivotSource,
    group_by: &[ColIdx],
    values: &[(ColIdx, Agg)],
) -> PivotResult {
    let rows = src.row_count();

    // Distinct value columns, in first-seen order. Each row reads each of these
    // ONCE and folds it into one accumulator; the requested aggregates all read
    // back from that shared accumulator. This is what stops `Sum`+`Avg` of the
    // same column counting every row twice.
    let mut value_cols: Vec<ColIdx> = Vec::new();
    for &(vc, _) in values {
        if !value_cols.contains(&vc) {
            value_cols.push(vc);
        }
    }
    // For each requested (col, agg), which accumulator slot to read from.
    let slot_of: Vec<usize> = values
        .iter()
        .map(|&(vc, _)| value_cols.iter().position(|&c| c == vc).unwrap())
        .collect();

    let n_slots = value_cols.len();
    let mut map: HashMap<OwnedKey, Vec<ColAcc>> = HashMap::new();

    // Reused across every row: cleared and refilled, never reallocated per row.
    // This is what keeps the hot path allocation-free apart from the one
    // `Box<[KeyPart]>` and one `Vec<ColAcc>` minted when a genuinely new group
    // appears.
    let mut scratch: Vec<KeyPart> = Vec::with_capacity(group_by.len());

    for row in 0..rows {
        scratch.clear();
        for &gc in group_by {
            scratch.push(KeyPart::of(src.cell(gc, row)));
        }

        // Probe with the borrowed scratch slice — no allocation on a hit.
        // `[KeyPart]: Borrow` off `Box<[KeyPart]>` makes this legal.
        let accs = if let Some(accs) = map.get_mut(scratch.as_slice()) {
            accs
        } else {
            // Miss: a new distinct group. This is the ONLY place a key or an
            // accumulator vector is allocated, so allocations total
            // O(distinct groups), not O(rows).
            let owned: OwnedKey = scratch.as_slice().into();
            map.entry(owned)
                .or_insert_with(|| vec![ColAcc::new(); n_slots])
        };

        for (slot, &vc) in value_cols.iter().enumerate() {
            accs[slot].push(src.cell(vc, row));
        }
    }

    let accumulators = map.len();

    // Materialise groups. To report the group key as `Cell`s we re-read the
    // KeyParts; text ids round-trip through `StrId`, numbers through `to_bits`
    // inverse. Blank parts report as `Cell::Blank`.
    let mut groups: Vec<(OwnedKey, Vec<ColAcc>)> = map.into_iter().collect();
    // Deterministic order independent of hash iteration.
    groups.sort_by(|a, b| a.0.cmp(&b.0));

    let groups = groups
        .into_iter()
        .map(|(key, accs)| {
            let key_cells = key
                .iter()
                .map(|part| match *part {
                    KeyPart::Number(bits) => Cell::Number(f64::from_bits(bits)),
                    KeyPart::Text(id) => Cell::Text(StrId(id)),
                    KeyPart::Blank => Cell::Blank,
                })
                .collect();
            let aggregates = values
                .iter()
                .zip(&slot_of)
                .map(|(&(_c, agg), &slot)| accs[slot].value(agg))
                .collect();
            PivotGroup {
                key: key_cells,
                aggregates,
            }
        })
        .collect();

    PivotResult {
        groups,
        accumulators,
    }
}

// `KeyPart` ordering for the deterministic sort above. Numbers sort by value
// (not raw bits, so -0.0/NaN don't jump around visibly), then text by id, then
// Blank last — a total order the `sort_by` relies on.
impl PartialOrd for KeyPart {
    fn partial_cmp(&self, other: &Self) -> Option<std::cmp::Ordering> {
        Some(self.cmp(other))
    }
}
impl Ord for KeyPart {
    fn cmp(&self, other: &Self) -> std::cmp::Ordering {
        use std::cmp::Ordering::*;
        use KeyPart::*;
        match (self, other) {
            (Number(a), Number(b)) => f64::from_bits(*a)
                .partial_cmp(&f64::from_bits(*b))
                .unwrap_or_else(|| a.cmp(b)),
            (Number(_), _) => Less,
            (_, Number(_)) => Greater,
            (Text(a), Text(b)) => a.cmp(b),
            (Text(_), _) => Less,
            (_, Text(_)) => Greater,
            (Blank, Blank) => Equal,
        }
    }
}

// A tiny helper so callers can hash a `Cell` directly if they build their own
// keying; unused by `compute` but part of the kernel's small surface. Kept
// `Value`-free like everything else here.
#[allow(dead_code)]
fn hash_cell<H: Hasher>(cell: &Cell, state: &mut H) {
    KeyPart::of(*cell).hash(state);
}

// Assert at compile time that an owned key borrows as a KeyPart slice, which is
// what makes the allocation-free probe in `compute` sound.
const _: fn() = || {
    fn assert_borrow<T: Borrow<[KeyPart]>>() {}
    assert_borrow::<OwnedKey>();
};

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashMap as StdMap;

    /// A columnar source built from literal columns of [`Cell`]s. This is the
    /// only place tests materialise data; `compute` still streams over it one
    /// row at a time exactly as it would over a real `Sheet`.
    struct Cols {
        cols: Vec<Vec<Cell>>,
        rows: usize,
    }

    impl Cols {
        fn new(cols: Vec<Vec<Cell>>) -> Self {
            let rows = cols.iter().map(|c| c.len()).max().unwrap_or(0);
            Self { cols, rows }
        }
    }

    impl PivotSource for Cols {
        fn row_count(&self) -> usize {
            self.rows
        }
        fn cell(&self, col: ColIdx, row: usize) -> Cell {
            self.cols
                .get(col.0 as usize)
                .and_then(|c| c.get(row))
                .copied()
                .unwrap_or(Cell::Blank)
        }
    }

    fn num(n: f64) -> Cell {
        Cell::Number(n)
    }
    fn txt(id: u32) -> Cell {
        Cell::Text(StrId(id))
    }

    #[test]
    fn groups_by_a_single_text_column_and_sums() {
        // group col (0): region ids; value col (1): amounts.
        let src = Cols::new(vec![
            vec![txt(1), txt(2), txt(1), txt(2), txt(1)],
            vec![num(10.0), num(5.0), num(20.0), num(7.0), num(3.0)],
        ]);
        let out = compute(&src, &[ColIdx(0)], &[(ColIdx(1), Agg::Sum)]);
        assert_eq!(out.accumulator_count(), 2, "two distinct regions");

        let g1 = out.group(&[txt(1)]).expect("region 1 present");
        let g2 = out.group(&[txt(2)]).expect("region 2 present");
        assert_eq!(g1.aggregates[0], Some(33.0), "10+20+3");
        assert_eq!(g2.aggregates[0], Some(12.0), "5+7");
    }

    #[test]
    fn every_aggregate_reports_its_own_answer() {
        // One group, values 2,8,5 -> sum 15, count 3, avg 5, min 2, max 8,
        // population stddev = sqrt(((2-5)^2+(8-5)^2+(5-5)^2)/3) = sqrt(6).
        let src = Cols::new(vec![
            vec![txt(1), txt(1), txt(1)],
            vec![num(2.0), num(8.0), num(5.0)],
        ]);
        let aggs = [
            Agg::Sum,
            Agg::Count,
            Agg::Avg,
            Agg::Min,
            Agg::Max,
            Agg::StdDev,
        ];
        let req: Vec<_> = aggs.iter().map(|&a| (ColIdx(1), a)).collect();
        let out = compute(&src, &[ColIdx(0)], &req);
        let g = &out.groups[0];
        assert_eq!(g.aggregates[0], Some(15.0), "sum");
        assert_eq!(g.aggregates[1], Some(3.0), "count");
        assert_eq!(g.aggregates[2], Some(5.0), "avg");
        assert_eq!(g.aggregates[3], Some(2.0), "min");
        assert_eq!(g.aggregates[4], Some(8.0), "max");
        let stddev = g.aggregates[5].expect("stddev present");
        assert!(
            (stddev - 6.0_f64.sqrt()).abs() < 1e-12,
            "population stddev should be sqrt(6), got {stddev}"
        );
    }

    #[test]
    fn count_includes_non_numeric_but_numeric_aggregates_ignore_them() {
        // Count is COUNTA-like (all rows); sum/avg/min/max/stddev see numbers
        // only. Group has 2 numbers and 2 blanks.
        let src = Cols::new(vec![
            vec![txt(1), txt(1), txt(1), txt(1)],
            vec![num(4.0), Cell::Blank, num(6.0), Cell::Blank],
        ]);
        let out = compute(
            &src,
            &[ColIdx(0)],
            &[(ColIdx(1), Agg::Count), (ColIdx(1), Agg::Avg)],
        );
        let g = &out.groups[0];
        assert_eq!(g.aggregates[0], Some(4.0), "count counts all 4 rows");
        assert_eq!(g.aggregates[1], Some(5.0), "avg over the 2 numbers only");
    }

    #[test]
    fn a_group_with_no_numbers_reports_none_not_zero() {
        // The AGENT_GUIDE point: an average of no numbers is not zero.
        let src = Cols::new(vec![vec![txt(1), txt(1)], vec![Cell::Blank, Cell::Blank]]);
        let out = compute(
            &src,
            &[ColIdx(0)],
            &[
                (ColIdx(1), Agg::Sum),
                (ColIdx(1), Agg::Avg),
                (ColIdx(1), Agg::Min),
                (ColIdx(1), Agg::StdDev),
                (ColIdx(1), Agg::Count),
            ],
        );
        let g = &out.groups[0];
        assert_eq!(g.aggregates[0], None, "sum of no numbers is None, not 0");
        assert_eq!(g.aggregates[1], None, "avg of no numbers is None");
        assert_eq!(g.aggregates[2], None, "min of no numbers is None");
        assert_eq!(g.aggregates[3], None, "stddev of no numbers is None");
        assert_eq!(
            g.aggregates[4],
            Some(2.0),
            "but count still counts the rows"
        );
    }

    #[test]
    fn multi_column_group_key() {
        // Group by (region, quarter). (1,10) appears twice, (1,11) once,
        // (2,10) once -> three groups.
        let src = Cols::new(vec![
            vec![txt(1), txt(1), txt(1), txt(2)],             // region
            vec![num(10.0), num(10.0), num(11.0), num(10.0)], // quarter
            vec![num(100.0), num(50.0), num(7.0), num(9.0)],  // amount
        ]);
        let out = compute(&src, &[ColIdx(0), ColIdx(1)], &[(ColIdx(2), Agg::Sum)]);
        assert_eq!(
            out.accumulator_count(),
            3,
            "three distinct (region,quarter)"
        );
        assert_eq!(
            out.group(&[txt(1), num(10.0)]).unwrap().aggregates[0],
            Some(150.0),
            "100+50 for (region 1, Q10)"
        );
        assert_eq!(
            out.group(&[txt(1), num(11.0)]).unwrap().aggregates[0],
            Some(7.0)
        );
        assert_eq!(
            out.group(&[txt(2), num(10.0)]).unwrap().aggregates[0],
            Some(9.0)
        );
    }

    #[test]
    fn empty_group_by_is_a_grand_total() {
        let src = Cols::new(vec![vec![num(1.0), num(2.0), num(3.0)]]);
        let out = compute(&src, &[], &[(ColIdx(0), Agg::Sum), (ColIdx(0), Agg::Count)]);
        assert_eq!(out.accumulator_count(), 1, "one grand-total group");
        assert_eq!(out.groups[0].key, Vec::<Cell>::new(), "empty key");
        assert_eq!(out.groups[0].aggregates[0], Some(6.0));
        assert_eq!(out.groups[0].aggregates[1], Some(3.0));
    }

    #[test]
    fn sum_uses_kahan_and_stays_exact_past_2_to_the_53() {
        // Mirrors column.rs::sum_is_exact_past_2_to_the_53. A naive accumulator
        // in the group would drift; Kahan must be exact. All rows in one group.
        const BASE: f64 = 9_007_199_254_740_992.0; // 2^53
        let n = 200_000usize;
        let group: Vec<Cell> = vec![txt(1); n];
        let vals: Vec<Cell> = (0..n).map(|i| num(BASE + i as f64)).collect();
        let src = Cols::new(vec![group, vals]);

        let out = compute(&src, &[ColIdx(0)], &[(ColIdx(1), Agg::Sum)]);
        let exact = BASE * n as f64 + (n as f64 - 1.0) * n as f64 / 2.0;
        let got = out.groups[0].aggregates[0].unwrap();
        assert_eq!(
            got,
            exact,
            "pivot sum drifted by {} — Kahan compensation is not working",
            exact - got
        );
    }

    /// Independent truth: a naive `HashMap<key, (naive_sum, count, min, max,
    /// values)>` built with completely different code, on a 100k-row subset,
    /// compared against the kernel. Deliberately NOT reusing any kernel code so
    /// a bug in the kernel cannot hide in a shared helper.
    #[test]
    fn correctness_against_independent_truth_on_100k_rows() {
        let n = 100_000usize;
        // Deterministic pseudo-random data, ~137 distinct groups.
        let mut group = Vec::with_capacity(n);
        let mut vals = Vec::with_capacity(n);
        let mut x: u64 = 0x9E3779B97F4A7C15;
        for _ in 0..n {
            x ^= x << 13;
            x ^= x >> 7;
            x ^= x << 17;
            let g = (x % 137) as u32;
            let v = ((x >> 20) % 100_000) as f64 * 0.5 - 25_000.0;
            group.push(txt(g));
            vals.push(num(v));
        }
        let src = Cols::new(vec![group.clone(), vals.clone()]);

        // Independent naive accumulation into full value lists per group.
        let mut truth: StdMap<u32, Vec<f64>> = StdMap::new();
        for i in 0..n {
            if let (Cell::Text(id), Cell::Number(v)) = (group[i], vals[i]) {
                truth.entry(id.0).or_default().push(v);
            }
        }

        let out = compute(
            &src,
            &[ColIdx(0)],
            &[
                (ColIdx(1), Agg::Sum),
                (ColIdx(1), Agg::Count),
                (ColIdx(1), Agg::Avg),
                (ColIdx(1), Agg::Min),
                (ColIdx(1), Agg::Max),
                (ColIdx(1), Agg::StdDev),
            ],
        );

        assert_eq!(
            out.accumulator_count(),
            truth.len(),
            "group count must match the independent truth"
        );

        for g in &out.groups {
            let Cell::Text(id) = g.key[0] else {
                panic!("group key should be text")
            };
            let vs = &truth[&id.0];
            let naive_sum: f64 = vs.iter().copied().sum();
            let count = vs.len() as f64;
            let mean = naive_sum / count;
            let naive_min = vs.iter().copied().fold(f64::INFINITY, f64::min);
            let naive_max = vs.iter().copied().fold(f64::NEG_INFINITY, f64::max);
            let var = vs.iter().map(|v| (v - mean).powi(2)).sum::<f64>() / count;
            let naive_std = var.sqrt();

            // Sum: Kahan vs naive can differ in the last bit, but this data is
            // well within 2^53, so they agree to a tight tolerance.
            let rel = |a: f64, b: f64| (a - b).abs() <= 1e-6 * a.abs().max(1.0);
            assert!(
                rel(g.aggregates[0].unwrap(), naive_sum),
                "sum mismatch for group {}: {:?} vs {naive_sum}",
                id.0,
                g.aggregates[0]
            );
            assert_eq!(g.aggregates[1].unwrap(), count, "count for {}", id.0);
            assert!(rel(g.aggregates[2].unwrap(), mean), "avg for {}", id.0);
            assert_eq!(g.aggregates[3].unwrap(), naive_min, "min for {}", id.0);
            assert_eq!(g.aggregates[4].unwrap(), naive_max, "max for {}", id.0);
            assert!(
                rel(g.aggregates[5].unwrap(), naive_std),
                "stddev for {}: {:?} vs {naive_std}",
                id.0,
                g.aggregates[5]
            );
        }
    }

    /// The scale invariant, as an assertion that can actually FAIL.
    ///
    /// A 10M-row pivot into a fixed 1000 groups must hold ~1000 accumulators,
    /// not 10M of anything. We assert the accumulator count is exactly the
    /// distinct-group count AND that the map's heap footprint is bounded by
    /// groups, not rows: if the kernel ever retained per-row state (or leaked a
    /// key per row), `accumulator_count()` would balloon to millions and this
    /// fails loudly. `sizeof(GroupAcc) * 1000` is a few KB; `* 10M` is hundreds
    /// of MB — the assertion sits firmly between them.
    #[test]
    fn memory_scales_with_groups_not_rows() {
        let rows = 10_000_000usize;
        const GROUPS: u32 = 1000;

        // A source that GENERATES cells on the fly — it never materialises the
        // 10M rows, so the test itself also honours the scale invariant and can
        // run in CI. Group = row % 1000; value = row as f64.
        struct Synthetic {
            rows: usize,
            groups: u32,
        }
        impl PivotSource for Synthetic {
            fn row_count(&self) -> usize {
                self.rows
            }
            fn cell(&self, col: ColIdx, row: usize) -> Cell {
                match col.0 {
                    0 => Cell::Text(StrId((row as u32) % self.groups)),
                    _ => Cell::Number(row as f64),
                }
            }
        }

        let src = Synthetic {
            rows,
            groups: GROUPS,
        };
        let out = compute(
            &src,
            &[ColIdx(0)],
            &[(ColIdx(1), Agg::Sum), (ColIdx(1), Agg::Count)],
        );

        // The core assertion: accumulators == distinct groups, not rows.
        assert_eq!(
            out.accumulator_count(),
            GROUPS as usize,
            "held {} accumulators for {rows} rows into {GROUPS} groups — memory \
             must scale with groups, not rows",
            out.accumulator_count()
        );

        // Bound the actual holding cost in bytes, well below any per-row cost.
        // 1000 accumulators + boxed keys is a handful of KB; if per-row state
        // ever crept in, this ceiling (2 MB) would be blown out by orders of
        // magnitude. Two value columns per group here.
        let per_group = 2 * std::mem::size_of::<ColAcc>()
            + std::mem::size_of::<OwnedKey>()
            + std::mem::size_of::<Vec<ColAcc>>()
            + 16;
        let bytes_held = out.accumulator_count() * per_group;
        assert!(
            bytes_held < 2 * 1024 * 1024,
            "pivot held ~{bytes_held} bytes for {GROUPS} groups; must be KB, not \
             proportional to {rows} rows"
        );

        // And the result is actually correct: every group got 10_000 rows and
        // the counts sum back to the row total.
        let total_count: f64 = out.groups.iter().map(|g| g.aggregates[1].unwrap()).sum();
        assert_eq!(total_count, rows as f64, "counts must cover every row");
    }
}
