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

use ferrix_core::{column_name, CellRef};

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
pub fn offset_formula(src: &str, drow: i64, dcol: i64) -> String {
    if drow == 0 && dcol == 0 {
        return src.to_string();
    }
    let b = src.as_bytes();
    let mut out = String::with_capacity(src.len());
    let mut i = 0usize;

    while i < b.len() {
        let ch = b[i];

        // Skip string literals wholesale: "A1" is text, not a reference.
        if ch == b'"' {
            let start = i;
            i += 1;
            while i < b.len() {
                if b[i] == b'"' {
                    // A doubled quote is an escaped quote, not the end.
                    if b.get(i + 1) == Some(&b'"') {
                        i += 2;
                        continue;
                    }
                    i += 1;
                    break;
                }
                i += 1;
            }
            out.push_str(&src[start..i]);
            continue;
        }

        // A word starts at $ or a letter.
        if ch == b'$' || ch.is_ascii_alphabetic() || ch == b'_' {
            let start = i;
            while i < b.len()
                && (b[i].is_ascii_alphanumeric() || b[i] == b'_' || b[i] == b'$' || b[i] == b'.')
            {
                i += 1;
            }
            let word = &src[start..i];

            // A word immediately followed by '(' is a function call, never a
            // reference — the same disambiguation the tokenizer uses, which is
            // what stops LOG10( being treated as cell LOG10.
            let mut j = i;
            while j < b.len() && (b[j] == b' ' || b[j] == b'\t') {
                j += 1;
            }
            let is_call = b.get(j) == Some(&b'(');

            if is_call {
                out.push_str(word);
                continue;
            }
            match shift_ref(word, drow, dcol) {
                Some(shifted) => out.push_str(&shifted),
                None => out.push_str(word),
            }
            continue;
        }

        out.push(ch as char);
        i += 1;
    }
    out
}

/// Shift one `A1`-style token, honouring `$`. Returns `None` when the word is
/// not a reference at all.
fn shift_ref(word: &str, drow: i64, dcol: i64) -> Option<String> {
    let b = word.as_bytes();
    let mut i = 0;

    let abs_col = b.first() == Some(&b'$');
    if abs_col {
        i += 1;
    }
    let letter_start = i;
    while i < b.len() && b[i].is_ascii_alphabetic() {
        i += 1;
    }
    if i == letter_start {
        return None;
    }
    let letters = &word[letter_start..i];
    // Excel's widest column is XFD; more letters means it is a name.
    if letters.len() > 3 {
        return None;
    }

    let abs_row = b.get(i) == Some(&b'$');
    if abs_row {
        i += 1;
    }
    let digit_start = i;
    while i < b.len() && b[i].is_ascii_digit() {
        i += 1;
    }
    if i == digit_start || i != b.len() {
        // No digits, or trailing junk — not a plain reference.
        return None;
    }
    let row_1based: u32 = word[digit_start..i].parse().ok()?;
    if row_1based == 0 {
        return None;
    }

    let mut col: u32 = 0;
    for byte in letters.bytes() {
        let v = (byte.to_ascii_uppercase() - b'A') as u32 + 1;
        col = col.checked_mul(26)?.checked_add(v)?;
    }
    let col0 = col - 1;
    let row0 = row_1based - 1;

    let new_row = if abs_row {
        row0
    } else {
        (row0 as i64 + drow).max(0) as u32
    };
    let new_col = if abs_col {
        col0
    } else {
        (col0 as i64 + dcol).max(0) as u32
    };

    let mut out = String::new();
    if abs_col {
        out.push('$');
    }
    out.push_str(&column_name(new_col));
    if abs_row {
        out.push('$');
    }
    out.push_str(&(new_row + 1).to_string());
    Some(out)
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
}
