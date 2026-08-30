//! Rewriting formula references so they follow a structural edit.
//!
//! When column B moves to position D, `=SUM(B1:B10)` must still sum the same
//! data — which now lives at D. The reference has to move with it.
//!
//! ## Why this is not `offset_formula`
//!
//! [`crate::fill::offset_formula`] shifts references by a constant delta and
//! deliberately **leaves `$` anchors alone** — that is what `$` means when you
//! fill.
//!
//! A reorder is the opposite case. `$A$1` means "always this cell", and after
//! the column holding that cell is dragged elsewhere, "this cell" is at a new
//! address. Pinning it would make `$A$1` point at whatever column happened to
//! slide into position A — silently reading the wrong data, which is far worse
//! than a visible `#REF!`. So a remap moves relative and absolute references
//! alike, and the `$` markers are **preserved in the output** rather than
//! honoured as a reason not to move. Excel behaves the same way.
//!
//! That distinction is exactly why the rewrite is textual. `Expr::Ref` throws
//! the `$` flags away, so an AST rewrite could move the reference but could not
//! put the `$` back — every absolute reference in the workbook would quietly
//! become relative, and the damage would only surface the next time someone
//! filled a formula.
//!
//! ## Deleted references
//!
//! A reference to a column that no longer exists becomes `#REF!`, the way a
//! spreadsheet has always signalled it. It is not clamped to a neighbouring
//! column: silently reading different data is the failure mode this whole
//! module exists to prevent.
//!
//! ## Ranges
//!
//! A range is remapped as a **unit**. Mapping `A1:C10`'s endpoints
//! independently can invert them into `C1:A10` when the reorder moves A past C,
//! and an inverted range is not what the user wrote. Both endpoints are mapped
//! and then re-normalised so the range still describes a rectangle.

use crate::refscan::{self, ParsedRef};

/// The `#REF!` text a broken reference collapses to.
pub const REF_ERROR: &str = "#REF!";

/// How one axis moved, as a mapping from old index to new.
///
/// `None` means the entry was deleted, and any reference to it becomes
/// `#REF!`. Implemented over the display permutation by the caller, so this
/// module never needs to know how the order is stored.
pub trait AxisMap {
    /// Where the entry formerly at `old` is now, or `None` if it is gone.
    fn map(&self, old: u32) -> Option<u32>;
}

/// An axis that did not move.
pub struct Identity;

impl AxisMap for Identity {
    #[inline]
    fn map(&self, old: u32) -> Option<u32> {
        Some(old)
    }
}

impl<F: Fn(u32) -> Option<u32>> AxisMap for F {
    #[inline]
    fn map(&self, old: u32) -> Option<u32> {
        self(old)
    }
}

/// Rewrite every reference in `src` through `cols` and `rows`.
///
/// Absolute markers are preserved on the output but do NOT prevent the move —
/// see the module docs for why that is the correct and safer behaviour for a
/// structural edit.
pub fn remap_formula<C: AxisMap, R: AxisMap>(src: &str, cols: &C, rows: &R) -> String {
    let words = refscan::scan(src);

    // Resolve every word once, so a range can consult its partner.
    let mapped: Vec<Option<Option<ParsedRef>>> = words
        .iter()
        .map(|w| {
            refscan::parse_ref(&src[w.start..w.end]).map(|p| {
                match (cols.map(p.col), rows.map(p.row)) {
                    (Some(col), Some(row)) => Some(ParsedRef {
                        col,
                        row,
                        // The `$` the user wrote survives verbatim. This is the
                        // half of the round-trip an AST rewrite cannot do.
                        abs_col: p.abs_col,
                        abs_row: p.abs_row,
                    }),
                    // Either axis gone: the whole reference is broken.
                    _ => None,
                }
            })
        })
        .collect();

    // Re-normalise ranges. Mapping endpoints independently can invert them, and
    // `C1:A10` is not a rectangle the user asked for.
    let mut fixed = mapped.clone();
    for i in 0..words.len() {
        let opens_range = refscan::range_follows(src, words[i].end);
        if !opens_range || i + 1 >= words.len() {
            continue;
        }
        let (Some(Some(a)), Some(Some(b))) = (&mapped[i], &mapped[i + 1]) else {
            continue;
        };
        let (lo_c, hi_c) = (a.col.min(b.col), a.col.max(b.col));
        let (lo_r, hi_r) = (a.row.min(b.row), a.row.max(b.row));
        // Keep each endpoint's own anchoring; only the coordinates swap.
        fixed[i] = Some(Some(ParsedRef {
            col: lo_c,
            row: lo_r,
            ..*a
        }));
        fixed[i + 1] = Some(Some(ParsedRef {
            col: hi_c,
            row: hi_r,
            ..*b
        }));
    }

    refscan::rewrite(src, &words, |i, _| match &fixed[i] {
        // Not a reference at all — leave the text exactly as written.
        None => None,
        Some(None) => Some(REF_ERROR.to_string()),
        Some(Some(p)) => Some(p.render()),
    })
}

/// Convenience: remap columns only, leaving rows alone. The common case, since
/// column reorder is the unrestricted one.
pub fn remap_columns<C: AxisMap>(src: &str, cols: &C) -> String {
    remap_formula(src, cols, &Identity)
}

/// Convenience: remap rows only.
pub fn remap_rows<R: AxisMap>(src: &str, rows: &R) -> String {
    remap_formula(src, &Identity, rows)
}

/// Rewrite a formula for a COPY/PASTE that lands `(drow, dcol)` away from
/// where it was copied.
///
/// This is the entry point the clipboard uses, and it is deliberately not
/// [`remap_formula`]. The two cases pull in opposite directions:
///
/// * a **structural** remap (a column dragged elsewhere) moves absolute
///   references too, because `$A$1` means "always this cell" and that cell has
///   a new address — see the module docs; whereas
/// * a **paste** is a fill by another name. `$A$1` means "always this cell"
///   and the cell has NOT moved, so the anchor must pin the reference. Moving
///   it would break the single most common spreadsheet idiom there is: a
///   column of `=B2*$F$1` copied down.
///
/// So this delegates to [`crate::fill::offset_formula`], which honours `$`,
/// rather than reimplementing the shift. One scanner, one set of rules about
/// what counts as a reference: two implementations that could disagree about
/// whether `LOG10(` is a reference is precisely the bug `refscan` exists to
/// make impossible.
///
/// Like everything in this family it rewrites the formula TEXT. A round trip
/// through the parser would drop every `$`, which for a paste is not a
/// cosmetic loss — it is the difference between `=B2*$F$1` and `=B2*F1`.
pub fn paste_formula(src: &str, drow: i64, dcol: i64) -> String {
    crate::fill::offset_formula(src, drow, dcol)
}

/// A rectangle of cells, 0-based and inclusive on all four sides.
///
/// The unit a block move works in. `contains` is the test that decides whether
/// a reference points *at* a cell that is moving, which is the whole question
/// [`remap_block`] answers.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct CellRect {
    /// Top row (inclusive).
    pub top: u32,
    /// Left column (inclusive).
    pub left: u32,
    /// Bottom row (inclusive).
    pub bottom: u32,
    /// Right column (inclusive).
    pub right: u32,
}

impl CellRect {
    /// A rectangle from two corners in any order.
    pub fn new(a_row: u32, a_col: u32, b_row: u32, b_col: u32) -> Self {
        CellRect {
            top: a_row.min(b_row),
            left: a_col.min(b_col),
            bottom: a_row.max(b_row),
            right: a_col.max(b_col),
        }
    }

    /// True when `(row, col)` is inside the rectangle.
    #[inline]
    pub fn contains(&self, row: u32, col: u32) -> bool {
        row >= self.top && row <= self.bottom && col >= self.left && col <= self.right
    }
}

/// Rewrite every reference in `src` that points AT a cell inside `block` so it
/// follows the block by `(d_row, d_col)`.
///
/// ## What this is for — and why it is not [`remap_formula`]
///
/// Dragging a rectangular block of cells to a new home is Excel's
/// "move-with-refs": a formula *elsewhere on the sheet* that reads a cell the
/// user just dragged must keep reading it at its new address. `=D1` where `D1`
/// moved down five rows becomes `=D6`.
///
/// This is a RECTANGULAR predicate — both axes together — so it cannot be
/// expressed as [`remap_formula`]'s two independent [`AxisMap`]s. A reference
/// at the block's row but a different column is NOT inside the block and must
/// not move; an axis map would move it. So this walks each reference's
/// `(row, col)` and shifts only the ones the rectangle actually contains.
///
/// ## Absolute markers, ranges, and partial overlap
///
/// * Absolute markers ride along but do not pin — same rule and same reason as
///   [`remap_formula`]: `$D$1` means "always this cell", and that cell moved,
///   so the reference moves and the `$` is preserved in the output. Excel does
///   this too.
/// * A range moves ONLY when BOTH endpoints are inside the block. A range that
///   straddles the block boundary (`A1:D10` when only `A1:B10` moved) is left
///   exactly as written — Excel does not silently reshape a range whose data
///   only partly moved, and shifting one endpoint would resize it into a
///   rectangle the user never asked for.
/// * Like the rest of this family it edits the formula TEXT, so every `$`,
///   every space, and every reference it does not touch comes out
///   byte-identical.
pub fn remap_block(src: &str, block: &CellRect, d_row: i64, d_col: i64) -> String {
    if d_row == 0 && d_col == 0 {
        return src.to_string();
    }
    let words = refscan::scan(src);

    // Shift one parsed reference, or leave it where it is if it is not inside
    // the block. `None` only when the shift would push it off the top/left
    // edge, which cannot happen for a move whose destination is on the sheet
    // but is guarded anyway so a corner case renders nothing rather than wrap.
    let shifted = |p: &ParsedRef| -> Option<ParsedRef> {
        if !block.contains(p.row, p.col) {
            return Some(*p);
        }
        let row = i64::from(p.row).checked_add(d_row)?;
        let col = i64::from(p.col).checked_add(d_col)?;
        if row < 0 || col < 0 {
            return None;
        }
        Some(ParsedRef {
            row: row as u32,
            col: col as u32,
            abs_col: p.abs_col,
            abs_row: p.abs_row,
        })
    };

    refscan::rewrite(src, &words, |i, w| {
        let p = refscan::parse_ref(&src[w.start..w.end])?;
        // A range endpoint only moves when its PARTNER endpoint is also inside
        // the block, so a straddling range is left untouched. Detect the range
        // by the same colon rule the rest of the module uses.
        let opens = refscan::range_follows(src, w.end);
        if opens {
            // Left endpoint: consult the right endpoint.
            let partner = words
                .get(i + 1)
                .and_then(|w2| refscan::parse_ref(&src[w2.start..w2.end]));
            match partner {
                Some(q) if block.contains(p.row, p.col) && block.contains(q.row, q.col) => {
                    shifted(&p).map(|r| r.render())
                }
                // Either endpoint outside the block: the range straddles or
                // sits wholly outside, so leave this endpoint verbatim.
                _ => None,
            }
        } else if i > 0 && refscan::range_follows(src, words[i - 1].end) {
            // Right endpoint of a range: consult the left endpoint.
            let partner = refscan::parse_ref(&src[words[i - 1].start..words[i - 1].end]);
            match partner {
                Some(q) if block.contains(p.row, p.col) && block.contains(q.row, q.col) => {
                    shifted(&p).map(|r| r.render())
                }
                _ => None,
            }
        } else {
            // A standalone single-cell reference.
            if block.contains(p.row, p.col) {
                shifted(&p).map(|r| r.render())
            } else {
                None
            }
        }
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashMap;

    /// An explicit old -> new mapping; anything absent is unchanged.
    fn map_of(pairs: &[(u32, u32)]) -> impl Fn(u32) -> Option<u32> + use<> {
        let m: HashMap<u32, u32> = pairs.iter().copied().collect();
        move |old| Some(m.get(&old).copied().unwrap_or(old))
    }

    /// A mapping that deletes the listed indices.
    fn deleting(gone: &[u32]) -> impl Fn(u32) -> Option<u32> + use<> {
        let g: Vec<u32> = gone.to_vec();
        move |old| (!g.contains(&old)).then_some(old)
    }

    #[test]
    fn a_reference_follows_its_column() {
        // THE acceptance criterion: move column B (1) to D (3), and
        // =SUM(B1:B10) must still reference the same data.
        let cols = map_of(&[(1, 3), (2, 1), (3, 2)]);
        assert_eq!(remap_columns("=SUM(B1:B10)", &cols), "=SUM(D1:D10)");
        assert_eq!(remap_columns("=B5", &cols), "=D5");
        // And the columns that shuffled to fill the gap follow too.
        assert_eq!(remap_columns("=C1+D1", &cols), "=B1+C1");
    }

    #[test]
    fn absolute_references_move_but_keep_their_dollars() {
        // The critical difference from fill: `$` must NOT pin a reference
        // against a structural edit, or it would silently read the column that
        // slid into that position. But the marker itself must survive.
        let cols = map_of(&[(0, 5)]);
        assert_eq!(remap_columns("=$A$1", &cols), "=$F$1");
        assert_eq!(remap_columns("=$A1", &cols), "=$F1");
        assert_eq!(remap_columns("=A$1", &cols), "=F$1");
        assert_eq!(remap_columns("=A1", &cols), "=F1");
    }

    #[test]
    fn dollars_survive_a_remap_that_changes_nothing() {
        // A no-op remap must be byte-identical, or every save would churn.
        let src = "=SUM($A$1:A1)*LOG10(B2)+\"text\"";
        assert_eq!(remap_formula(src, &Identity, &Identity), src);
    }

    #[test]
    fn rows_can_be_remapped_independently() {
        let rows = map_of(&[(0, 9), (9, 0)]);
        assert_eq!(remap_rows("=A1+A10", &rows), "=A10+A1");
        // Column stays put when only rows moved.
        assert_eq!(remap_rows("=$C$1", &rows), "=$C$10");
    }

    #[test]
    fn both_axes_remap_together() {
        let cols = map_of(&[(0, 1)]);
        let rows = map_of(&[(0, 4)]);
        assert_eq!(remap_formula("=A1", &cols, &rows), "=B5");
    }

    #[test]
    fn a_deleted_column_becomes_ref_error_not_a_wrong_neighbour() {
        // Clamping to a neighbour would silently sum different data. A visible
        // #REF! is the whole point.
        let cols = deleting(&[1]);
        assert_eq!(remap_columns("=B1*2", &cols), "=#REF!*2");
        assert_eq!(remap_columns("=SUM(B1:B9)", &cols), "=SUM(#REF!:#REF!)");
        // Neighbouring references are untouched.
        assert_eq!(remap_columns("=A1+C1", &cols), "=A1+C1");
    }

    #[test]
    fn a_deleted_row_also_breaks_the_reference() {
        let rows = deleting(&[4]);
        assert_eq!(remap_rows("=A5", &rows), "=#REF!");
        assert_eq!(remap_rows("=A4+A6", &rows), "=A4+A6");
    }

    #[test]
    fn ranges_are_renormalised_rather_than_inverted() {
        // Move A past C. Mapping endpoints independently would produce
        // =SUM(D1:C10) — backwards, and not what the user wrote.
        let cols = map_of(&[(0, 3), (1, 0), (2, 1), (3, 2)]);
        let out = remap_columns("=SUM(A1:C10)", &cols);
        assert_eq!(out, "=SUM(B1:D10)", "range inverted or lost rows");
        // A range that stays in order is unaffected by the normalisation.
        assert_eq!(remap_columns("=SUM(B1:C1)", &cols), "=SUM(A1:B1)");
    }

    #[test]
    fn range_normalisation_keeps_each_endpoints_anchoring() {
        // The running-total idiom `$A$1:A1` must keep its mixed anchoring even
        // when the endpoints swap coordinates.
        let cols = map_of(&[(0, 3), (3, 0)]);
        let out = remap_columns("=SUM($A$1:D1)", &cols);
        // A -> D and D -> A, so the endpoints swap; the $ stay on the left.
        assert_eq!(out, "=SUM($A$1:D1)");
    }

    #[test]
    fn function_names_are_never_treated_as_references() {
        // The LOG10 trap, inherited from the scanner.
        let cols = map_of(&[(0, 1)]);
        assert_eq!(remap_columns("=LOG10(A1)", &cols), "=LOG10(B1)");
        assert_eq!(remap_columns("=SUM(A1:A3)", &cols), "=SUM(B1:B3)");
    }

    #[test]
    fn text_literals_and_sheet_names_are_untouched() {
        let cols = map_of(&[(0, 1)]);
        assert_eq!(remap_columns("=\"A1\"&A1", &cols), "=\"A1\"&B1");
        assert_eq!(remap_columns("=Sheet1!A1", &cols), "=Sheet1!B1");
        // 'Q1 2024' must not be rewritten into 'R1 2024'.
        assert_eq!(remap_columns("='Q1 2024'!A1", &cols), "='Q1 2024'!B1");
    }

    #[test]
    fn spacing_and_case_are_preserved() {
        let cols = map_of(&[(0, 1)]);
        assert_eq!(remap_columns("= A1 + $A$2 ", &cols), "= B1 + $B$2 ");
    }

    #[test]
    fn wide_column_names_survive_the_round_trip() {
        let cols = map_of(&[(26, 0), (0, 26)]);
        assert_eq!(remap_columns("=AA1", &cols), "=A1");
        assert_eq!(remap_columns("=A1", &cols), "=AA1");
    }

    #[test]
    fn remapping_is_stable_under_repetition() {
        // Applying the same identity remap twice must not drift.
        let src = "=SUM($B$2:D4)/LOG10(E5)";
        let once = remap_formula(src, &Identity, &Identity);
        let twice = remap_formula(&once, &Identity, &Identity);
        assert_eq!(once, src);
        assert_eq!(twice, src);
    }

    // --- paste (issue #30) ---

    #[test]
    fn a_pasted_formula_offsets_its_relative_references() {
        // Copy =A1+B1 from row 1 and paste it three rows down: it must read
        // the row it landed on, not the row it came from.
        assert_eq!(paste_formula("=A1+B1", 3, 0), "=A4+B4");
        assert_eq!(paste_formula("=A1", 0, 2), "=C1");
        assert_eq!(paste_formula("=SUM(A1:A3)", 1, 0), "=SUM(A2:A4)");
    }

    #[test]
    fn a_pasted_formula_pins_its_absolute_references() {
        // THE difference from a structural remap, and the reason paste has
        // its own entry point. `=B2*$F$1` copied down must still point at F1;
        // if the anchor moved, every tax-rate and exchange-rate column in
        // every spreadsheet ever written would silently read the wrong cell.
        assert_eq!(paste_formula("=B2*$F$1", 5, 0), "=B7*$F$1");
        assert_eq!(paste_formula("=$A1", 2, 0), "=$A3", "$col pins the column");
        assert_eq!(paste_formula("=A$1", 2, 0), "=A$1", "$row pins the row");
        assert_eq!(paste_formula("=$A$1", 9, 9), "=$A$1");
    }

    #[test]
    fn a_pasted_formula_keeps_every_dollar_it_started_with() {
        // The AST-round-trip failure this whole family exists to prevent: a
        // parse-then-render would come back with the `$` gone and nothing
        // would look wrong until the next fill.
        let out = paste_formula("=SUM($B$2:$B$9)/$C$1+D5", 4, 0);
        assert_eq!(out, "=SUM($B$2:$B$9)/$C$1+D9");
        assert_eq!(out.matches('$').count(), 6, "every $ must survive: {out}");
    }

    #[test]
    fn pasting_in_place_changes_nothing() {
        let src = "=SUM($A$1:A1)*LOG10(B2)+\"text\"";
        assert_eq!(paste_formula(src, 0, 0), src);
    }

    #[test]
    fn a_pasted_formula_does_not_rewrite_text_or_function_names() {
        assert_eq!(paste_formula("=LOG10(A1)", 1, 0), "=LOG10(A2)");
        assert_eq!(paste_formula("=\"A1\"&A1", 1, 0), "=\"A1\"&A2");
    }

    // --- A1# spill-range references survive a structural remap (#27 P4) ----

    #[test]
    fn a_spill_range_anchor_follows_its_column_and_keeps_the_hash() {
        // Move column B (1) to D (3): a `B1#` spill-range anchor follows to D1,
        // and the `#` suffix — a verbatim trailing byte, never part of the ref
        // word — rides along, exactly as it does through a fill.
        let cols = map_of(&[(1, 3), (2, 1), (3, 2)]);
        assert_eq!(remap_columns("=B1#", &cols), "=D1#");
        assert_eq!(remap_columns("=SUM(B1#)", &cols), "=SUM(D1#)");
    }

    #[test]
    fn a_spill_range_anchor_on_a_deleted_column_breaks_like_any_reference() {
        // Deleting the column a spill-range anchors on breaks the reference to
        // `#REF!`, the same as a plain reference — the `#` cannot rescue a
        // reference whose target no longer exists. The mechanical text rewrite
        // leaves the `#` behind (`#REF!#`), which the parser absorbs back to a
        // clean `#REF!` (see `parser::a_broken_spill_anchor_reparses_as_ref_error`).
        let cols = deleting(&[0]);
        assert_eq!(remap_columns("=A1#", &cols), "=#REF!#");
    }

    #[test]
    fn a_pasted_spill_range_shifts_its_anchor() {
        assert_eq!(paste_formula("=A1#", 1, 1), "=B2#");
        assert_eq!(paste_formula("=$A$1#+B2", 1, 0), "=$A$1#+B3");
    }

    // --- block move-with-refs (#82) ------------------------------------------

    #[test]
    fn a_reference_to_a_moved_cell_follows_the_block() {
        // THE acceptance criterion for #82's move-with-refs: D1 is dragged
        // down five rows, and a formula elsewhere that read D1 must read D6.
        let block = CellRect::new(0, 3, 0, 3); // D1 alone
        assert_eq!(remap_block("=D1", &block, 5, 0), "=D6");
        assert_eq!(remap_block("=D1*2+A1", &block, 5, 0), "=D6*2+A1");
    }

    #[test]
    fn a_reference_outside_the_block_is_left_alone() {
        // E1 shares D1's row but not its column, so a row-only block that only
        // contains D1 must not touch a reference to E1.
        let block = CellRect::new(0, 3, 0, 3); // D1
        assert_eq!(remap_block("=E1", &block, 5, 0), "=E1");
        // A1 is on neither the block's row nor its column.
        assert_eq!(remap_block("=A1", &block, 5, 0), "=A1");
    }

    #[test]
    fn moved_absolute_references_keep_their_dollars() {
        // $ rides along but does not pin: the cell moved, so the reference
        // moves, and the marker survives verbatim in the output.
        let block = CellRect::new(0, 3, 0, 3); // D1
        assert_eq!(remap_block("=$D$1", &block, 5, 0), "=$D$6");
        assert_eq!(remap_block("=D$1", &block, 5, 0), "=D$6");
        assert_eq!(remap_block("=$D1", &block, 5, 0), "=$D6");
    }

    #[test]
    fn a_range_wholly_inside_the_block_moves_as_a_unit() {
        // B2:C3 is entirely inside the moved block, so both endpoints shift.
        let block = CellRect::new(1, 1, 2, 2); // B2:C3
        assert_eq!(remap_block("=SUM(B2:C3)", &block, 0, 3), "=SUM(E2:F3)");
    }

    #[test]
    fn a_range_that_straddles_the_block_is_left_untouched() {
        // The block moved only B2:C3, but the formula reads A1:C3 — a range
        // that only partly moved. Reshaping it into a rectangle the user never
        // wrote is worse than leaving it, so it is left exactly as written.
        let block = CellRect::new(1, 1, 2, 2); // B2:C3
        assert_eq!(remap_block("=SUM(A1:C3)", &block, 0, 3), "=SUM(A1:C3)");
    }

    #[test]
    fn a_no_op_block_move_is_byte_identical() {
        let block = CellRect::new(0, 0, 4, 4);
        let src = "=SUM($A$1:A1)*LOG10(B2)+\"text\"";
        assert_eq!(remap_block(src, &block, 0, 0), src);
    }

    #[test]
    fn remap_block_never_treats_text_or_functions_as_references() {
        // The LOG10 / string-literal traps the whole family exists to avoid.
        let block = CellRect::new(0, 0, 9, 9);
        assert_eq!(remap_block("=LOG10(A1)", &block, 1, 0), "=LOG10(A2)");
        assert_eq!(remap_block("=\"A1\"&A1", &block, 1, 0), "=\"A1\"&A2");
        assert_eq!(remap_block("=Sheet1!A1", &block, 1, 0), "=Sheet1!A2");
    }

    #[test]
    fn cellrect_normalises_its_corners() {
        // Built from bottom-right + top-left, it still describes the rectangle.
        let r = CellRect::new(5, 5, 1, 1);
        assert!(r.contains(3, 3));
        assert!(r.contains(1, 1));
        assert!(r.contains(5, 5));
        assert!(!r.contains(0, 3));
        assert!(!r.contains(6, 3));
    }
}
