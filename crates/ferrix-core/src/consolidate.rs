//! Consolidate: aggregate matching ranges from N sheets by row/column key.
//!
//! ## What "by key" means
//!
//! Excel's Consolidate has two modes: by POSITION (add cell to cell) and by
//! LABEL (match the row and column headers, then add). Only the second is
//! implemented here, because the first is trivially wrong the moment two
//! sheets list their regions in a different order — and silently so.
//!
//! Each source range is read as a small header grid: the first column holds
//! ROW keys, the first row holds COLUMN keys, and the interior is data. The
//! union of the keys, in FIRST-SEEN order, becomes the target's shape.
//!
//! ## Missing keys are explicit, never zeroed
//!
//! The rule this module exists to enforce: if Q3 has no "West" row, "West" in
//! the consolidated output is **not** silently short by one quarter's worth.
//! Every output cell carries the count of sheets that CONTRIBUTED to it and
//! the list of sheets that did not ([`Cell::missing_from`]). A SUM over two
//! of three sheets is reported as such, so the user can tell "West sold 0"
//! apart from "West is absent from Q3".
//!
//! [`ConsolidateReport::missing`] is the summary the status line shows. An
//! implementation that treated absence as zero would produce the same totals
//! and no warning at all, which is exactly the failure mode this design
//! rejects.
//!
//! ## The scale invariant
//!
//! Consolidation is a RANGE operation, not a sheet operation: the source
//! ranges are the small labelled blocks a user points at, and the cost is
//! bounded by `row_keys x col_keys`, which is the size of the OUTPUT the user
//! asked for. Nothing here scans a whole sheet, and nothing holds a row.
//! [`ConsolidateRequest::max_cells`] caps the output so a mis-selected
//! 200M-row column is refused with a message rather than accepted and paged
//! to death.

use std::collections::HashMap;

use crate::subtotal::SubtotalFn;

/// How the values landing on one key pair are combined.
///
/// Deliberately the same enum subtotals use: "Sum" must mean the same thing
/// in both features, and two parallel definitions would drift.
pub type ConsolidateFn = SubtotalFn;

/// One source block: a sheet plus a labelled rectangle in it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Source {
    /// Sheet name, used in the missing-key report so a message can name the
    /// sheet the user has to go look at.
    pub sheet: String,
    /// Inclusive data bounds, EXCLUDING the header row and key column: the
    /// caller passes the full selection and [`ConsolidateRequest`] peels the
    /// headers off.
    pub first_row: u32,
    pub last_row: u32,
    pub first_col: u32,
    pub last_col: u32,
}

/// Read access to the sheets being consolidated.
///
/// One call per cell of the SOURCE RANGES, never per sheet row.
pub trait RangeSource {
    /// Display text of a cell, used for keys. Empty string for a blank cell.
    fn label_at(&self, sheet: &str, row: u32, col: u32) -> String;
    /// Numeric value of a cell, or `None` when it holds no number.
    fn number_at(&self, sheet: &str, row: u32, col: u32) -> Option<f64>;
}

/// What to consolidate and where.
#[derive(Clone, Debug)]
pub struct ConsolidateRequest {
    pub sources: Vec<Source>,
    pub func: ConsolidateFn,
    /// Cap on output cells. Bounds the only allocation that scales with the
    /// user's selection.
    pub max_cells: usize,
}

/// Default cap on consolidated output cells: a 1000x1000 result.
pub const MAX_OUTPUT_CELLS: usize = 1_000_000;

/// One consolidated cell.
#[derive(Clone, Debug, PartialEq)]
pub struct Cell {
    /// The aggregate, or `None` when NO sheet contributed a number here.
    /// `None` is not zero and must not be rendered as zero.
    pub value: Option<f64>,
    /// How many sources supplied a number for this key pair.
    pub contributors: usize,
    /// Sources that had the key pair but no NUMBER there, or did not have the
    /// key pair at all. Named, so the report can say which.
    pub missing_from: Vec<String>,
}

impl Cell {
    fn empty() -> Self {
        Self {
            value: None,
            contributors: 0,
            missing_from: Vec::new(),
        }
    }

    /// True when at least one source did not contribute.
    pub fn is_partial(&self) -> bool {
        !self.missing_from.is_empty()
    }
}

/// The consolidated result: a small labelled grid plus an honesty report.
#[derive(Clone, Debug, PartialEq)]
pub struct Consolidated {
    /// Row keys, in first-seen order across the sources.
    pub row_keys: Vec<String>,
    /// Column keys, in first-seen order across the sources.
    pub col_keys: Vec<String>,
    /// `cells[r * col_keys.len() + c]`.
    pub cells: Vec<Cell>,
    pub report: ConsolidateReport,
}

impl Consolidated {
    pub fn at(&self, row: usize, col: usize) -> Option<&Cell> {
        if col >= self.col_keys.len() {
            return None;
        }
        self.cells.get(row * self.col_keys.len() + col)
    }

    /// The aggregate at a key pair, by name.
    pub fn get(&self, row_key: &str, col_key: &str) -> Option<&Cell> {
        let r = self.row_keys.iter().position(|k| k == row_key)?;
        let c = self.col_keys.iter().position(|k| k == col_key)?;
        self.at(r, c)
    }
}

/// Counts a status line can state without lying.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct ConsolidateReport {
    pub sources: usize,
    pub row_keys: usize,
    pub col_keys: usize,
    /// Output cells where at least one source contributed nothing.
    pub partial_cells: usize,
    /// Output cells where NO source contributed a number.
    pub empty_cells: usize,
    /// `(sheet, row_key)` pairs absent from that sheet entirely — the
    /// coarse-grained warning, e.g. "Q3 has no West row".
    pub missing: Vec<(String, String)>,
}

/// Why a consolidation could not run.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ConsolidateError {
    NoSources,
    /// A source rectangle has no data area once its headers are peeled off.
    EmptySource(String),
    TooLarge {
        cells: usize,
        max: usize,
    },
}

impl std::fmt::Display for ConsolidateError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            ConsolidateError::NoSources => write!(f, "no source ranges to consolidate"),
            ConsolidateError::EmptySource(s) => write!(
                f,
                "the range on {s} has no data once its header row and key \
                 column are excluded"
            ),
            ConsolidateError::TooLarge { cells, max } => write!(
                f,
                "consolidating would produce {cells} cells (limit {max}) — \
                 select the labelled block, not whole columns"
            ),
        }
    }
}

/// One source's labelled grid, read once.
///
/// Bounded by the source rectangle the user selected, which is the output's
/// own size — this is the only place keys are materialised, and they are
/// short header strings, one per row and column, never one per cell.
struct Block {
    sheet: String,
    row_keys: Vec<String>,
    col_keys: Vec<String>,
    /// `(row_key_index, col_key_index) -> number`. Absent means the source
    /// had no NUMBER there, which is exactly the distinction this feature is
    /// about — so it stays absent rather than becoming 0.0.
    values: HashMap<(usize, usize), f64>,
}

/// A block plus name->index lookups for its two key axes.
///
/// Built once per source so the output fill is `O(output x sources)` rather
/// than a linear key search per cell.
struct Indexed<'a> {
    block: &'a Block,
    rows: HashMap<&'a str, usize>,
    cols: HashMap<&'a str, usize>,
}

impl<'a> Indexed<'a> {
    fn of(block: &'a Block) -> Self {
        let index = |keys: &'a [String]| -> HashMap<&'a str, usize> {
            keys.iter()
                .enumerate()
                .map(|(i, k)| (k.as_str(), i))
                .collect()
        };
        Self {
            rows: index(&block.row_keys),
            cols: index(&block.col_keys),
            block,
        }
    }
}

fn read_block(s: &Source, src: &impl RangeSource) -> Result<Block, ConsolidateError> {
    if s.last_row <= s.first_row || s.last_col <= s.first_col {
        return Err(ConsolidateError::EmptySource(s.sheet.clone()));
    }
    // Header row is `first_row`; key column is `first_col`.
    let col_keys: Vec<String> = (s.first_col + 1..=s.last_col)
        .map(|c| src.label_at(&s.sheet, s.first_row, c))
        .collect();
    let row_keys: Vec<String> = (s.first_row + 1..=s.last_row)
        .map(|r| src.label_at(&s.sheet, r, s.first_col))
        .collect();
    let mut values = HashMap::new();
    for (ri, r) in (s.first_row + 1..=s.last_row).enumerate() {
        for (ci, c) in (s.first_col + 1..=s.last_col).enumerate() {
            if let Some(n) = src.number_at(&s.sheet, r, c) {
                values.insert((ri, ci), n);
            }
        }
    }
    Ok(Block {
        sheet: s.sheet.clone(),
        row_keys,
        col_keys,
        values,
    })
}

/// Fold a list of contributed numbers with `f`.
fn combine(f: ConsolidateFn, vals: &[f64]) -> Option<f64> {
    if vals.is_empty() {
        // COUNT of nothing is 0, but a cell no source mentioned is genuinely
        // absent rather than counted zero — the caller decides, and it says
        // `None`.
        return None;
    }
    Some(match f {
        ConsolidateFn::Sum => vals.iter().sum(),
        ConsolidateFn::Count => vals.len() as f64,
        ConsolidateFn::Average => vals.iter().sum::<f64>() / vals.len() as f64,
        ConsolidateFn::Min => vals.iter().copied().fold(f64::INFINITY, f64::min),
        ConsolidateFn::Max => vals.iter().copied().fold(f64::NEG_INFINITY, f64::max),
    })
}

/// Consolidate `req.sources` into one labelled grid.
pub fn consolidate(
    req: &ConsolidateRequest,
    src: &impl RangeSource,
) -> Result<Consolidated, ConsolidateError> {
    if req.sources.is_empty() {
        return Err(ConsolidateError::NoSources);
    }
    let blocks: Vec<Block> = req
        .sources
        .iter()
        .map(|s| read_block(s, src))
        .collect::<Result<_, _>>()?;

    // Union of keys, FIRST-SEEN order. Not sorted: the first sheet's layout
    // is the one the user is looking at, and reordering it would make the
    // result unrecognisable.
    let mut row_keys: Vec<String> = Vec::new();
    let mut col_keys: Vec<String> = Vec::new();
    for b in &blocks {
        for k in &b.row_keys {
            if !row_keys.iter().any(|x| x == k) {
                row_keys.push(k.clone());
            }
        }
        for k in &b.col_keys {
            if !col_keys.iter().any(|x| x == k) {
                col_keys.push(k.clone());
            }
        }
    }
    let cells = row_keys.len() * col_keys.len();
    if cells > req.max_cells {
        return Err(ConsolidateError::TooLarge {
            cells,
            max: req.max_cells,
        });
    }

    // Per-block key lookups, so the fill below is O(output x sources).
    let indexed: Vec<Indexed<'_>> = blocks.iter().map(Indexed::of).collect();

    let mut out = Vec::with_capacity(cells);
    let mut partial_cells = 0usize;
    let mut empty_cells = 0usize;
    let mut vals: Vec<f64> = Vec::with_capacity(blocks.len());
    for rk in &row_keys {
        for ck in &col_keys {
            vals.clear();
            let mut cell = Cell::empty();
            for ix in &indexed {
                let b = ix.block;
                let found = ix
                    .rows
                    .get(rk.as_str())
                    .zip(ix.cols.get(ck.as_str()))
                    .and_then(|(&r, &c)| b.values.get(&(r, c)).copied());
                match found {
                    Some(n) => {
                        vals.push(n);
                        cell.contributors += 1;
                    }
                    // NOT zeroed. The sheet is named so the user can see
                    // which quarter is short.
                    None => cell.missing_from.push(b.sheet.clone()),
                }
            }
            cell.value = combine(req.func, &vals);
            if cell.contributors == 0 {
                empty_cells += 1;
            } else if cell.is_partial() {
                partial_cells += 1;
            }
            out.push(cell);
        }
    }

    // Coarse warning: a key a whole sheet never mentions.
    let mut missing = Vec::new();
    for b in &blocks {
        for rk in &row_keys {
            if !b.row_keys.iter().any(|k| k == rk) {
                missing.push((b.sheet.clone(), rk.clone()));
            }
        }
    }

    Ok(Consolidated {
        report: ConsolidateReport {
            sources: blocks.len(),
            row_keys: row_keys.len(),
            col_keys: col_keys.len(),
            partial_cells,
            empty_cells,
            missing,
        },
        row_keys,
        col_keys,
        cells: out,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Three quarterly sheets, described as literal grids.
    struct Sheets {
        grids: HashMap<String, Vec<Vec<&'static str>>>,
    }

    impl Sheets {
        fn new(pairs: &[(&str, &[&[&'static str]])]) -> Self {
            let mut grids = HashMap::new();
            for (name, rows) in pairs {
                grids.insert(
                    name.to_string(),
                    rows.iter().map(|r| r.to_vec()).collect::<Vec<_>>(),
                );
            }
            Self { grids }
        }
        fn raw(&self, sheet: &str, row: u32, col: u32) -> &'static str {
            self.grids
                .get(sheet)
                .and_then(|g| g.get(row as usize))
                .and_then(|r| r.get(col as usize))
                .copied()
                .unwrap_or("")
        }
    }

    impl RangeSource for Sheets {
        fn label_at(&self, sheet: &str, row: u32, col: u32) -> String {
            self.raw(sheet, row, col).to_string()
        }
        fn number_at(&self, sheet: &str, row: u32, col: u32) -> Option<f64> {
            self.raw(sheet, row, col).parse().ok()
        }
    }

    fn src_of(sheet: &str, rows: u32, cols: u32) -> Source {
        Source {
            sheet: sheet.to_string(),
            first_row: 0,
            last_row: rows - 1,
            first_col: 0,
            last_col: cols - 1,
        }
    }

    fn q_sheets() -> Sheets {
        Sheets::new(&[
            (
                "Q1",
                &[
                    &["", "Widgets", "Gadgets"],
                    &["East", "10", "1"],
                    &["West", "20", "2"],
                ],
            ),
            (
                "Q2",
                &[
                    &["", "Widgets", "Gadgets"],
                    &["East", "100", "5"],
                    &["West", "200", "6"],
                ],
            ),
            // Q3 has NO West row. This is the whole point of the feature.
            ("Q3", &[&["", "Widgets", "Gadgets"], &["East", "1000", "7"]]),
        ])
    }

    fn run(func: ConsolidateFn) -> Consolidated {
        let s = q_sheets();
        let req = ConsolidateRequest {
            sources: vec![src_of("Q1", 3, 3), src_of("Q2", 3, 3), src_of("Q3", 2, 3)],
            func,
            max_cells: MAX_OUTPUT_CELLS,
        };
        consolidate(&req, &s).expect("valid request")
    }

    #[test]
    fn matching_keys_aggregate_across_sheets() {
        let c = run(ConsolidateFn::Sum);
        assert_eq!(c.row_keys, vec!["East", "West"]);
        assert_eq!(c.col_keys, vec!["Widgets", "Gadgets"]);
        let east = c.get("East", "Widgets").unwrap();
        assert_eq!(east.value, Some(1110.0));
        assert_eq!(east.contributors, 3);
        assert!(
            !east.is_partial(),
            "East is on every sheet, so nothing is missing"
        );
        assert_eq!(c.get("East", "Gadgets").unwrap().value, Some(13.0));
    }

    #[test]
    fn a_key_missing_from_one_sheet_is_reported_not_zeroed() {
        let c = run(ConsolidateFn::Sum);
        let west = c.get("West", "Widgets").unwrap();
        assert_eq!(
            west.value,
            Some(220.0),
            "the total is the two sheets that HAVE West"
        );
        assert_eq!(west.contributors, 2, "not 3");
        assert_eq!(
            west.missing_from,
            vec!["Q3".to_string()],
            "the user must be able to see WHICH sheet is short — a silent \
             zero would produce the same 220 with no warning at all"
        );
        assert!(
            c.report
                .missing
                .contains(&("Q3".to_string(), "West".to_string())),
            "report.missing must name the sheet/key pair; got {:?}",
            c.report.missing
        );
        assert_eq!(c.report.partial_cells, 2, "both West columns are partial");
    }

    #[test]
    fn average_divides_by_contributors_not_by_source_count() {
        let c = run(ConsolidateFn::Average);
        // West: 20 and 200 over TWO sheets. Zeroing Q3 would give 73.33.
        assert_eq!(
            c.get("West", "Widgets").unwrap().value,
            Some(110.0),
            "averaging over the sheets that HAVE the key; treating Q3 as 0 \
             would give 73.33 and look plausible"
        );
        assert_eq!(c.get("East", "Widgets").unwrap().value, Some(1110.0 / 3.0));
    }

    #[test]
    fn count_counts_contributing_sheets() {
        let c = run(ConsolidateFn::Count);
        assert_eq!(c.get("East", "Widgets").unwrap().value, Some(3.0));
        assert_eq!(c.get("West", "Widgets").unwrap().value, Some(2.0));
    }

    #[test]
    fn min_and_max_ignore_absent_sheets() {
        let mn = run(ConsolidateFn::Min);
        assert_eq!(mn.get("West", "Widgets").unwrap().value, Some(20.0));
        let mx = run(ConsolidateFn::Max);
        assert_eq!(mx.get("West", "Widgets").unwrap().value, Some(200.0));
        assert_eq!(
            mn.get("East", "Widgets").unwrap().value,
            Some(10.0),
            "a zero-filled absent sheet would make every MIN 0"
        );
    }

    #[test]
    fn a_cell_no_source_has_is_none_rather_than_zero() {
        let s = Sheets::new(&[
            ("A", &[&["", "X"], &["r1", "5"]]),
            ("B", &[&["", "Y"], &["r2", "6"]]),
        ]);
        let req = ConsolidateRequest {
            sources: vec![src_of("A", 2, 2), src_of("B", 2, 2)],
            func: ConsolidateFn::Sum,
            max_cells: MAX_OUTPUT_CELLS,
        };
        let c = consolidate(&req, &s).unwrap();
        assert_eq!(c.row_keys, vec!["r1", "r2"]);
        assert_eq!(c.col_keys, vec!["X", "Y"]);
        let hole = c.get("r1", "Y").unwrap();
        assert_eq!(
            hole.value, None,
            "no source has (r1, Y); reporting 0 would invent a data point"
        );
        assert_eq!(hole.contributors, 0);
        assert_eq!(c.report.empty_cells, 2, "(r1,Y) and (r2,X)");
    }

    #[test]
    fn keys_keep_first_seen_order_across_sheets() {
        let s = Sheets::new(&[
            ("A", &[&["", "c1"], &["zeta", "1"], &["alpha", "2"]]),
            ("B", &[&["", "c1"], &["mid", "3"]]),
        ]);
        let req = ConsolidateRequest {
            sources: vec![src_of("A", 3, 2), src_of("B", 2, 2)],
            func: ConsolidateFn::Sum,
            max_cells: MAX_OUTPUT_CELLS,
        };
        let c = consolidate(&req, &s).unwrap();
        assert_eq!(
            c.row_keys,
            vec!["zeta", "alpha", "mid"],
            "the first sheet's layout survives; sorting would scramble the \
             order the user is looking at"
        );
    }

    #[test]
    fn non_numeric_cells_are_missing_contributions_not_zeros() {
        let s = Sheets::new(&[
            ("A", &[&["", "X"], &["r1", "5"]]),
            ("B", &[&["", "X"], &["r1", "n/a"]]),
        ]);
        let req = ConsolidateRequest {
            sources: vec![src_of("A", 2, 2), src_of("B", 2, 2)],
            func: ConsolidateFn::Sum,
            max_cells: MAX_OUTPUT_CELLS,
        };
        let c = consolidate(&req, &s).unwrap();
        let cell = c.get("r1", "X").unwrap();
        assert_eq!(cell.value, Some(5.0));
        assert_eq!(cell.contributors, 1);
        assert_eq!(cell.missing_from, vec!["B".to_string()]);
    }

    #[test]
    fn an_oversized_selection_is_refused_with_the_numbers() {
        let s = Sheets::new(&[("A", &[&["", "X"], &["r1", "1"]])]);
        let req = ConsolidateRequest {
            sources: vec![src_of("A", 2, 2)],
            func: ConsolidateFn::Sum,
            max_cells: 0,
        };
        let err = consolidate(&req, &s).unwrap_err();
        assert_eq!(err, ConsolidateError::TooLarge { cells: 1, max: 0 });
        assert!(err.to_string().contains("limit 0"));
    }

    #[test]
    fn a_headerless_range_is_refused_rather_than_read_as_data() {
        let s = Sheets::new(&[("A", &[&["1"]])]);
        let req = ConsolidateRequest {
            sources: vec![src_of("A", 1, 1)],
            func: ConsolidateFn::Sum,
            max_cells: MAX_OUTPUT_CELLS,
        };
        assert_eq!(
            consolidate(&req, &s).unwrap_err(),
            ConsolidateError::EmptySource("A".to_string())
        );
    }

    #[test]
    fn no_sources_is_an_error_not_an_empty_success() {
        let s = Sheets::new(&[]);
        let req = ConsolidateRequest {
            sources: Vec::new(),
            func: ConsolidateFn::Sum,
            max_cells: MAX_OUTPUT_CELLS,
        };
        assert_eq!(
            consolidate(&req, &s).unwrap_err(),
            ConsolidateError::NoSources
        );
    }
}
