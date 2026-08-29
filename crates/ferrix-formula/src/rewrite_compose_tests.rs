//! Cross-branch composition tests for formula TEXT rewriting.
//!
//! Two features that landed on separate branches both rewrite formula TEXT
//! through `refscan`, and neither branch could test the other:
//!
//! - **#43** added sheet qualifiers (`Sheet2!A1`, `'My Sheet'!A1:B10`, the 3-D
//!   span `Sheet1:Sheet3!A1`) and `names::rename_sheet_in_formula`.
//! - **#30** added `remap::paste_formula`, which shifts a pasted formula's
//!   references by a row/column delta via `fill::offset_formula`.
//!
//! The clipboard branch never saw a sheet qualifier — its fixtures are all
//! same-sheet — and the multi-sheet branch never pasted anything. So the
//! property that only exists after the merge is untested by construction:
//! **one scanner, two rewriters, and a token shape only one of them knew
//! about.**
//!
//! That seam is where a silent defect would live. `Sheet1:Sheet3!A1` contains
//! the substring `1:Sheet3`, which reads like a range to a scanner that does
//! not know about spans; `#REF!` contains `REF!`, which reads like a sheet
//! qualifier. If the paste rewriter's view of "what is a reference" disagreed
//! with the qualifier scanner's, a paste would corrupt the sheet name and the
//! formula would silently repoint at different data — the failure mode that is
//! plausible rather than loud, because the result still looks like a formula.
//!
//! What these assert if the composition were broken: a pasted cross-sheet
//! formula comes back naming the wrong sheet, or with its `$` anchors dropped,
//! or with a 3-D span shredded into a range. None of them can pass against a
//! rewriter that mangles the other feature's token shape.

use crate::{names::rename_sheet_in_formula, remap::paste_formula};

/// A paste must shift the CELL part of a cross-sheet reference and leave the
/// SHEET part alone. Shifting the sheet name is meaningless; dropping the
/// qualifier repoints the formula at the pasting sheet's own data, which is
/// the silent-wrong-answer case.
#[test]
fn pasting_a_cross_sheet_formula_moves_the_cell_not_the_sheet() {
    // Down two rows, right one column.
    assert_eq!(paste_formula("=Sheet2!A1", 2, 1), "=Sheet2!B3");
    assert_eq!(paste_formula("=Sheet2!A1:A9", 2, 1), "=Sheet2!B3:B11");
    // A quoted name survives its quotes. Losing them yields `=My Sheet!B3`,
    // which does not parse — but losing the SPACE would parse and be wrong.
    assert_eq!(
        paste_formula("='My Sheet'!A1", 2, 1),
        "='My Sheet'!B3",
        "a quoted sheet name must survive a paste intact"
    );
}

/// `$` anchors must survive a paste on the cell half of a qualified
/// reference, exactly as they do on an unqualified one. This is the reason
/// both features rewrite text rather than round-tripping the AST: the parser
/// discards `$`, so an AST trip silently unpins every anchor.
#[test]
fn a_pasted_cross_sheet_formula_keeps_its_absolute_markers() {
    assert_eq!(paste_formula("=Sheet2!$A$1", 5, 3), "=Sheet2!$A$1");
    assert_eq!(paste_formula("=Sheet2!$A1", 5, 3), "=Sheet2!$A6");
    assert_eq!(paste_formula("=Sheet2!A$1", 5, 3), "=Sheet2!D$1");
    // Mixed with an unqualified reference in one expression, so a rewriter
    // that handled only one shape cannot pass.
    assert_eq!(paste_formula("=Sheet2!$A$1*B2", 1, 0), "=Sheet2!$A$1*B3");
}

/// A 3-D span is one token, and a paste must not shred it.
///
/// `Sheet1:Sheet3!A1` contains `1:Sheet3`. A scanner that treats `:` as a
/// range separator without knowing about spans sees a reference where there is
/// none and rewrites inside the sheet names. The result still looks like a
/// formula, which is what makes this worth pinning.
#[test]
fn pasting_a_three_d_span_shifts_only_its_cell_part() {
    let out = paste_formula("=SUM(Sheet1:Sheet3!A1)", 3, 0);
    assert_eq!(
        out, "=SUM(Sheet1:Sheet3!A4)",
        "the span endpoints must be untouched and only the cell shifted"
    );
    let out = paste_formula("=SUM(Sheet1:Sheet3!A1:B2)", 1, 1);
    assert_eq!(out, "=SUM(Sheet1:Sheet3!B2:C3)");
}

/// `#REF!` must survive a paste as `#REF!`.
///
/// Its interior spells `REF!`, which is the shape of a sheet qualifier. A
/// paste that rewrote inside it would turn a visibly dead reference into
/// something that looks alive — strictly worse than the error it replaced.
#[test]
fn pasting_a_broken_reference_leaves_it_broken() {
    assert_eq!(paste_formula("=#REF!*2", 4, 4), "=#REF!*2");
    assert_eq!(paste_formula("=#REF!+A1", 1, 0), "=#REF!+A2");
}

/// The two rewriters compose in BOTH orders and agree.
///
/// Renaming a sheet then pasting the formula must give the same text as
/// pasting then renaming. If either rewriter disturbed the other's token
/// shape, the two orders would diverge — and in a real workbook the order is
/// whatever the user happened to do, so a divergence is a bug you cannot
/// reproduce on demand.
#[test]
fn a_rename_and_a_paste_commute() {
    let src = "=Sheet2!$A$1+Sheet2!B2";

    let rename_then_paste = paste_formula(&rename_sheet_in_formula(src, "Sheet2", "Q1"), 1, 0);
    let paste_then_rename = rename_sheet_in_formula(&paste_formula(src, 1, 0), "Sheet2", "Q1");

    assert_eq!(
        rename_then_paste, paste_then_rename,
        "rewriting order must not change the result"
    );
    assert_eq!(
        rename_then_paste, "=Q1!$A$1+Q1!B3",
        "the anchored reference stays pinned, the relative one follows the paste"
    );
}

/// Renaming into a name that needs quotes, then pasting, keeps both properties.
///
/// The rename adds quotes; the paste must then treat the quoted name as one
/// token. A paste that re-scanned the quoted name as ordinary text would find
/// `Sheet` and a number in `'Q1 2024'` and shift something inside the name.
#[test]
fn a_paste_after_a_rename_into_a_quoted_name_keeps_the_quotes() {
    let renamed = rename_sheet_in_formula("=Sheet2!A1*2", "Sheet2", "Q1 2024");
    assert_eq!(renamed, "='Q1 2024'!A1*2");
    assert_eq!(
        paste_formula(&renamed, 4, 0),
        "='Q1 2024'!A5*2",
        "the quoted name is one token to the paste rewriter too"
    );
}

/// A rename must not touch a sheet name inside a string literal — INCLUDING
/// after a paste has rewritten the references around it.
///
/// This is #43's asymmetry (a sheet name has another meaning; a defined name
/// does not) crossed with #30's rewriter. The formula names `Sheet2` twice:
/// once as a reference, once as prose the user typed.
#[test]
fn a_paste_does_not_expose_a_string_literal_to_a_later_rename() {
    let src = "=Sheet2!A1&\" (from Sheet2)\"";
    let pasted = paste_formula(src, 2, 0);
    assert_eq!(
        pasted, "=Sheet2!A3&\" (from Sheet2)\"",
        "a paste must not shift anything inside a string literal"
    );
    let renamed = rename_sheet_in_formula(&pasted, "Sheet2", "Q1");
    assert_eq!(
        renamed, "=Q1!A3&\" (from Sheet2)\"",
        "the reference is repointed; the prose is the user's text and stays"
    );
}
