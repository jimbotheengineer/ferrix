//! Fill: extending a selection by copying, continuing a series, or offsetting
//! formulas.
//!
//! ## Why formulas are rewritten as text, not as an AST
//!
//! Filling `=A1*2` down one row must produce `=A2*2`, and filling `=$A$1*2`
//! must produce `=$A$1*2` unchanged. The parser already recognises `$` and
//! records `abs_col`/`abs_row` on its tokens, but `Expr::Ref` carries only a
//! `CellRef` — the flags are discarded once parsed. Rewriting the AST would
//! therefore either lose absolute markers or require threading anchor state
//! through every expression node and every match site in the evaluator.
//!
//! Rewriting the source text instead keeps `$` semantics exactly, preserves
//! the user's own spacing and capitalisation, and cannot alter the meaning of
//! anything it does not touch. The scanner below mirrors the tokenizer's own
//! rules for what counts as a reference.

use ferrix_core::CellRef;

use crate::refscan;

/// How a fill should populate the cells it extends into.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum FillKind {
    /// Repeat the source block, tiling to cover the target.
    Copy,
    /// Continue an arithmetic progression detected in the source.
    Series,
}

/// Offset every *relative* reference in a formula by `(drow, dcol)`.
///
/// Absolute parts (`$A`, `A$1`, `$A$1`) are left alone, matching Excel.
/// References that would move above row 1 or left of column A are clamped at
/// the edge rather than wrapping — Excel emits `#REF!` there, but clamping
/// keeps a filled block usable, and the alternative would silently poison
/// cells the user can see.
///
/// The scanning rules (what counts as a reference, and what is a string, a
/// sheet name, or a function call) live in [`crate::refscan`], shared with the
/// structural-reorder rewriter so the two can never disagree about which
/// bytes of a formula are a reference.
pub fn offset_formula(src: &str, drow: i64, dcol: i64) -> String {
    if drow == 0 && dcol == 0 {
        return src.to_string();
    }
    let words = refscan::scan(src);
    refscan::rewrite(src, &words, |_, w| {
        let p = refscan::parse_ref(&src[w.start..w.end])?;
        // `$` pins against a FILL — this is the case where honouring it is
        // correct. A structural reorder is the opposite; see `crate::remap`.
        let new_row = if p.abs_row {
            p.row
        } else {
            (p.row as i64 + drow).max(0) as u32
        };
        let new_col = if p.abs_col {
            p.col
        } else {
            (p.col as i64 + dcol).max(0) as u32
        };
        Some(
            refscan::ParsedRef {
                col: new_col,
                row: new_row,
                abs_col: p.abs_col,
                abs_row: p.abs_row,
            }
            .render(),
        )
    })
}

/// Detect a constant arithmetic step in a column of numbers.
///
/// Returns `None` unless there are at least two values and every consecutive
/// difference matches. One value is ambiguous — Excel treats a lone number as
/// a copy, not a series starting at step 1 — so it is deliberately not a
/// series here either.
pub fn detect_step(values: &[Option<f64>]) -> Option<f64> {
    let nums: Vec<f64> = values.iter().copied().collect::<Option<Vec<f64>>>()?;
    if nums.len() < 2 {
        return None;
    }
    let step = nums[1] - nums[0];
    for w in nums.windows(2) {
        // Floating point: compare with a relative tolerance so 0.1 increments
        // are recognised despite not being exactly representable.
        let d = w[1] - w[0];
        let scale = step.abs().max(1.0);
        if (d - step).abs() > scale * 1e-9 {
            return None;
        }
    }
    Some(step)
}

/// Where a fill is heading, derived from how far the handle was dragged.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum FillDir {
    Down,
    Up,
    Right,
    Left,
}

/// Choose an axis from a drag delta. Fills are single-axis, like Excel: the
/// dominant direction wins, so a slightly diagonal drag still does something
/// predictable.
pub fn fill_direction(drow: i64, dcol: i64) -> Option<FillDir> {
    if drow == 0 && dcol == 0 {
        return None;
    }
    if drow.abs() >= dcol.abs() {
        Some(if drow > 0 { FillDir::Down } else { FillDir::Up })
    } else {
        Some(if dcol > 0 {
            FillDir::Right
        } else {
            FillDir::Left
        })
    }
}

/// The cell a value is sourced from when tiling `len` source cells over an
/// offset of `n` positions.
#[inline]
pub fn tile_index(n: usize, len: usize) -> usize {
    n % len
}

/// Offset from a source cell to a destination cell.
#[inline]
pub fn delta(from: CellRef, to: CellRef) -> (i64, i64) {
    (
        to.row as i64 - from.row as i64,
        to.col as i64 - from.col as i64,
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn relative_refs_shift() {
        assert_eq!(offset_formula("=A1*2", 1, 0), "=A2*2");
        assert_eq!(offset_formula("=A1*2", 5, 0), "=A6*2");
        assert_eq!(offset_formula("=A1", 0, 1), "=B1");
        assert_eq!(offset_formula("=A1+B2", 1, 1), "=B2+C3");
    }

    #[test]
    fn absolute_refs_are_pinned() {
        // The whole point of `$`: filling must not move it.
        assert_eq!(offset_formula("=$A$1*2", 5, 5), "=$A$1*2");
        assert_eq!(offset_formula("=$A1*2", 1, 3), "=$A2*2");
        assert_eq!(offset_formula("=A$1*2", 3, 1), "=B$1*2");
    }

    #[test]
    fn function_names_are_not_references() {
        // SUM( must never be read as a cell. This is the same trap that made
        // LOG10 mis-lex historically.
        assert_eq!(offset_formula("=SUM(A1:A3)", 1, 0), "=SUM(A2:A4)");
        assert_eq!(offset_formula("=LOG10(A1)", 1, 0), "=LOG10(A2)");
        // A bare LOG10 IS a valid reference (column LOG, row 10) — Excel
        // agrees. Only the paren disambiguates.
        assert_eq!(offset_formula("=LOG10", 1, 0), "=LOG11");
    }

    #[test]
    fn ranges_shift_both_ends() {
        assert_eq!(offset_formula("=SUM(A1:B5)", 2, 0), "=SUM(A3:B7)");
        assert_eq!(offset_formula("=SUM($A$1:$B$5)", 2, 0), "=SUM($A$1:$B$5)");
        // Mixed anchoring: the classic running-total idiom.
        assert_eq!(offset_formula("=SUM($A$1:A1)", 1, 0), "=SUM($A$1:A2)");
    }

    #[test]
    fn text_literals_are_untouched() {
        // "A1" inside quotes is text, not a reference.
        assert_eq!(offset_formula("=\"A1\"&A1", 1, 0), "=\"A1\"&A2");
        assert_eq!(
            offset_formula("=IF(A1>0,\"B2 ok\",\"no\")", 1, 0),
            "=IF(A2>0,\"B2 ok\",\"no\")"
        );
    }

    #[test]
    fn refs_clamp_at_the_edges() {
        // Moving above row 1 clamps rather than wrapping to u32::MAX.
        assert_eq!(offset_formula("=A1", -5, 0), "=A1");
        assert_eq!(offset_formula("=A5", -2, 0), "=A3");
        assert_eq!(offset_formula("=B1", 0, -5), "=A1");
    }

    #[test]
    fn zero_offset_is_identity() {
        let src = "=SUM($A$1:A1)*LOG10(B2)+\"text\"";
        assert_eq!(offset_formula(src, 0, 0), src);
    }

    #[test]
    fn spacing_and_case_are_preserved() {
        // Text rewriting must not reformat what the user typed.
        assert_eq!(offset_formula("= A1 + B2 ", 1, 0), "= A2 + B3 ");
    }

    #[test]
    fn wide_column_names_survive() {
        assert_eq!(offset_formula("=AA1", 1, 0), "=AA2");
        assert_eq!(offset_formula("=Z1", 0, 1), "=AA1");
        assert_eq!(offset_formula("=AA1", 0, -1), "=Z1");
    }

    #[test]
    fn series_detection_requires_a_constant_step() {
        assert_eq!(detect_step(&[Some(1.0), Some(2.0)]), Some(1.0));
        assert_eq!(detect_step(&[Some(0.0), Some(5.0), Some(10.0)]), Some(5.0));
        assert_eq!(detect_step(&[Some(10.0), Some(8.0)]), Some(-2.0));
        // Not a progression.
        assert_eq!(detect_step(&[Some(1.0), Some(2.0), Some(4.0)]), None);
        // A single value is a copy, not a series.
        assert_eq!(detect_step(&[Some(1.0)]), None);
        // Non-numeric cells cannot form a series.
        assert_eq!(detect_step(&[Some(1.0), None]), None);
    }

    #[test]
    fn fractional_steps_are_detected() {
        // 0.1 is not exactly representable; the tolerance must absorb that.
        let vals = [Some(0.1), Some(0.2), Some(0.3), Some(0.4)];
        let step = detect_step(&vals).expect("0.1 step should be detected");
        assert!((step - 0.1).abs() < 1e-9);
    }

    #[test]
    fn direction_picks_the_dominant_axis() {
        assert_eq!(fill_direction(5, 1), Some(FillDir::Down));
        assert_eq!(fill_direction(-5, 1), Some(FillDir::Up));
        assert_eq!(fill_direction(1, 5), Some(FillDir::Right));
        assert_eq!(fill_direction(1, -5), Some(FillDir::Left));
        assert_eq!(fill_direction(0, 0), None);
        // A tie favours vertical, which is the common case.
        assert_eq!(fill_direction(3, 3), Some(FillDir::Down));
    }

    #[test]
    fn tiling_repeats_the_source() {
        assert_eq!(tile_index(0, 3), 0);
        assert_eq!(tile_index(3, 3), 0);
        assert_eq!(tile_index(4, 3), 1);
    }

    // --- A1# spill-range references survive a fill (#27 P4) ----------------

    #[test]
    fn a_spill_range_reference_moves_its_anchor_and_keeps_the_hash() {
        // The `#` is not part of the reference WORD the scanner rewrites — it
        // is a trailing byte copied verbatim — so filling `=A1#` moves the A1
        // anchor like any relative reference and leaves the `#` attached. This
        // is the whole reason the tokenizer lexes `#` as its own token: the
        // text-editing rewrite model in `refscan` never has to learn about it.
        assert_eq!(offset_formula("=A1#", 1, 1), "=B2#");
        // Inside a call, and in a compound expression, the `#` still rides
        // along with the reference it followed.
        assert_eq!(offset_formula("=SUM(A1#)", 0, 2), "=SUM(C1#)");
        assert_eq!(offset_formula("=A1#+B2", 1, 0), "=A2#+B3");
    }

    #[test]
    fn an_absolute_spill_anchor_is_pinned_through_a_fill() {
        // `$A$1#` — the `$` markers survive because the rewrite is textual (an
        // AST round trip would drop them, per the module docs), so an absolute
        // spill anchor stays put while the rest of the formula shifts.
        assert_eq!(offset_formula("=$A$1#", 3, 3), "=$A$1#");
        assert_eq!(offset_formula("=$A$1#+B2", 1, 0), "=$A$1#+B3");
    }
}
