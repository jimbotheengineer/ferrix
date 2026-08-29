//! Editing the references inside a formula's SOURCE TEXT.
//!
//! ## Why this is text and not the AST
//!
//! Same reason [`crate::refscan`] and [`crate::fill`] give at length:
//! `Expr::Ref` carries only a `CellRef` and throws away the `$` markers the
//! tokenizer recorded. Anything that round-trips a formula through
//! parse → render therefore silently unpins every absolute reference in it.
//!
//! F4 anchoring is the feature where that trap is easiest to fall into — it is
//! *about* the `$` markers — so it is built on top of the token-level scanner,
//! which reports each reference's span together with the anchoring the user
//! actually typed. Every rewrite here splices bytes into the original string:
//! spacing, capitalisation, string literals, sheet qualifiers and every
//! reference other than the one being edited come out byte-identical.
//!
//! Two operations live here:
//!
//! * [`cycle_at`] — F4: cycle the anchoring of the reference under the caret;
//! * [`shift_span`] — dragging a highlighted range outline onto another cell.

use crate::refscan::{parse_ref, range_follows, scan, ParsedRef, RefWord};

/// One reference in the source: a single cell, or a range with two endpoints.
///
/// A range is ONE span deliberately. `A1:B5` is a single thing to the user —
/// F4 anchors it as a unit, and dragging its outline moves both corners — and
/// treating the endpoints independently is how a range ends up inverted into
/// `B5:A1`.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct RefSpan {
    /// Byte offset of the first character of the whole reference.
    pub start: usize,
    /// Byte offset one past its last character.
    pub end: usize,
    /// Top-left endpoint, with the anchoring the user wrote.
    pub first: ParsedRef,
    /// Bottom-right endpoint; `None` for a single-cell reference.
    pub last: Option<ParsedRef>,
    first_word: RefWord,
    last_word: Option<RefWord>,
}

impl RefSpan {
    /// True when this reference is a range rather than a single cell.
    pub fn is_range(&self) -> bool {
        self.last.is_some()
    }

    /// Inclusive bounds as (top row, left col, bottom row, right col).
    ///
    /// Normalised, so a range written backwards still describes a rectangle a
    /// caller can outline without checking which corner came first.
    pub fn bounds(&self) -> (u32, u32, u32, u32) {
        let b = self.last.unwrap_or(self.first);
        (
            self.first.row.min(b.row),
            self.first.col.min(b.col),
            self.first.row.max(b.row),
            self.first.col.max(b.col),
        )
    }
}

/// Every reference in `src`, in source order, ranges folded into one span.
///
/// Built on [`scan`], so everything the tokenizer would not read as a
/// reference — text literals, quoted sheet names, function names — is already
/// gone, and a caller does not get to re-derive that judgement and drift from
/// it.
pub fn spans(src: &str) -> Vec<RefSpan> {
    let words = scan(src);
    let mut out = Vec::new();
    let mut i = 0usize;
    while i < words.len() {
        let w = words[i];
        let Some(first) = parse_ref(&src[w.start..w.end]) else {
            i += 1;
            continue;
        };
        // `A1:B5` — the colon binds the two words into one reference. The
        // second word has to parse as well; `A1:name` is not a range this can
        // rewrite, and pretending otherwise would corrupt the name.
        if range_follows(src, w.end) {
            if let Some(w2) = words.get(i + 1) {
                if let Some(last) = parse_ref(&src[w2.start..w2.end]) {
                    out.push(RefSpan {
                        start: w.start,
                        end: w2.end,
                        first,
                        last: Some(last),
                        first_word: w,
                        last_word: Some(*w2),
                    });
                    i += 2;
                    continue;
                }
            }
        }
        out.push(RefSpan {
            start: w.start,
            end: w.end,
            first,
            last: None,
            first_word: w,
            last_word: None,
        });
        i += 1;
    }
    out
}

/// The reference the caret (a BYTE offset) is sitting in or immediately after.
///
/// "Immediately after" matters: the caret is at the end of the text the moment
/// the user finishes typing `=A1`, which is exactly when they reach for F4.
pub fn span_at(src: &str, caret: usize) -> Option<RefSpan> {
    spans(src)
        .into_iter()
        .find(|s| caret >= s.start && caret <= s.end)
}

/// The four anchoring states, in Excel's F4 order.
///
/// `A1` → `$A$1` → `A$1` → `$A1` → back to `A1`.
fn next_anchor(abs_col: bool, abs_row: bool) -> (bool, bool) {
    match (abs_col, abs_row) {
        (false, false) => (true, true),
        (true, true) => (false, true),
        (false, true) => (true, false),
        (true, false) => (false, false),
    }
}

/// Splice replacement text over a set of scanned words, right to left.
///
/// Right to left so an earlier word's offsets are still valid after a later
/// one has been replaced. Everything outside the listed words is copied
/// verbatim — that is the whole point of rewriting text rather than the AST.
fn splice(src: &str, edits: &mut [(RefWord, String)]) -> String {
    edits.sort_by_key(|(w, _)| std::cmp::Reverse(w.start));
    let mut out = src.to_string();
    for (w, text) in edits.iter() {
        out.replace_range(w.start..w.end, text);
    }
    out
}

/// F4: cycle the anchoring of the reference under `caret`.
///
/// Returns the rewritten formula and the byte range the reference now occupies
/// in it, so the caller can leave the caret on the reference it just changed.
/// `None` when the caret is not on a reference, which must leave the text
/// alone rather than guessing at a nearby one.
///
/// A RANGE cycles as a unit, both endpoints taking the first endpoint's next
/// state — `A1:B5` → `$A$1:$B$5`, matching Excel. Anchoring the corners
/// independently would let a user produce `$A$1:B5` by accident and never be
/// able to say what F4 would do next.
pub fn cycle_at(src: &str, caret: usize) -> Option<(String, std::ops::Range<usize>)> {
    let sp = span_at(src, caret)?;
    let (abs_col, abs_row) = next_anchor(sp.first.abs_col, sp.first.abs_row);

    let mut a = sp.first;
    a.abs_col = abs_col;
    a.abs_row = abs_row;
    let first_text = a.render();
    let mut edits = vec![(sp.first_word, first_text.clone())];

    let mut grew = first_text.len() as isize - (sp.first_word.end - sp.first_word.start) as isize;
    if let (Some(mut b), Some(w2)) = (sp.last, sp.last_word) {
        b.abs_col = abs_col;
        b.abs_row = abs_row;
        let t = b.render();
        grew += t.len() as isize - (w2.end - w2.start) as isize;
        edits.push((w2, t));
    }

    let out = splice(src, &mut edits);
    let end = (sp.end as isize + grew) as usize;
    Some((out, sp.start..end))
}

/// Move a reference by a whole-cell offset, keeping its anchoring.
///
/// This is what dragging a highlighted range outline onto another cell does.
/// Absolute endpoints move too: the user grabbed the outline and dropped it
/// somewhere, which is an instruction about *where* the reference points, not
/// about whether it is pinned. The `$` markers survive the move because this
/// re-renders from the flags the scanner read rather than from a parse.
///
/// `None` when the move would push an endpoint off the top or left edge, so a
/// drag past the boundary does nothing instead of silently clamping the
/// reference somewhere the user did not aim at.
pub fn shift_span(src: &str, span: &RefSpan, d_row: i64, d_col: i64) -> Option<String> {
    let moved = |p: ParsedRef| -> Option<ParsedRef> {
        let row = i64::from(p.row).checked_add(d_row)?;
        let col = i64::from(p.col).checked_add(d_col)?;
        if row < 0 || col < 0 || row > i64::from(u32::MAX) || col > i64::from(u32::MAX) {
            return None;
        }
        Some(ParsedRef {
            row: row as u32,
            col: col as u32,
            ..p
        })
    };
    let a = moved(span.first)?;
    let mut edits = vec![(span.first_word, a.render())];
    if let (Some(b), Some(w2)) = (span.last, span.last_word) {
        edits.push((w2, moved(b)?.render()));
    }
    Some(splice(src, &mut edits))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The whole point of the feature, and the whole point of doing it in
    /// text: four presses return to exactly what the user typed.
    #[test]
    fn f4_cycles_through_four_states_and_returns() {
        let mut src = "=A1".to_string();
        let mut seen = Vec::new();
        for _ in 0..4 {
            let (next, _) = cycle_at(&src, src.len()).expect("caret is on A1");
            seen.push(next.clone());
            src = next;
        }
        assert_eq!(
            seen,
            vec![
                "=$A$1".to_string(),
                "=A$1".to_string(),
                "=$A1".to_string(),
                "=A1".to_string(),
            ],
            "F4 must walk A1 -> $A$1 -> A$1 -> $A1 -> A1"
        );
        assert_eq!(src, "=A1", "the cycle did not close");
    }

    /// THE regression this design exists to prevent. Rewriting through the
    /// AST would re-render every reference in the formula and drop the `$`
    /// markers on the ones the user did not touch.
    #[test]
    fn f4_leaves_every_other_reference_byte_identical() {
        let src = "=SUM($C$3:$C$9)+A1*'Q1 2024'!$B$2-\"$D$4\"+LOG10(  $E$5 )";
        // Caret sits just after A1.
        let caret = src.find("A1").unwrap() + 2;
        let (out, span) = cycle_at(src, caret).expect("caret is on A1");
        assert_eq!(
            out, "=SUM($C$3:$C$9)+$A$1*'Q1 2024'!$B$2-\"$D$4\"+LOG10(  $E$5 )",
            "only A1 may change"
        );
        assert_eq!(&out[span], "$A$1", "reported span must cover the new text");

        // And the untouched markers really are still there after a full cycle.
        let mut cur = src.to_string();
        let mut caret = caret;
        for _ in 0..4 {
            let (next, sp) = cycle_at(&cur, caret).unwrap();
            caret = sp.end;
            cur = next;
        }
        assert_eq!(cur, src, "a full cycle must be byte-identical to the input");
    }

    #[test]
    fn f4_anchors_a_range_as_one_unit() {
        let (out, _) = cycle_at("=SUM(A1:B5)", 7).expect("caret inside the range");
        assert_eq!(out, "=SUM($A$1:$B$5)");
        // Spacing around the colon survives, because each endpoint word is
        // spliced rather than the span being re-rendered.
        let (out, _) = cycle_at("=SUM(A1 : B5)", 7).unwrap();
        assert_eq!(out, "=SUM($A$1 : $B$5)");
    }

    #[test]
    fn f4_off_a_reference_changes_nothing() {
        assert!(
            cycle_at("=1+2", 2).is_none(),
            "no reference under the caret"
        );
        assert!(
            cycle_at("=\"A1\"", 3).is_none(),
            "a reference inside a string literal is text, not a reference"
        );
        assert!(
            cycle_at("=Sheet1!A1", 5).is_none(),
            "the caret is in the sheet qualifier, not the cell"
        );
    }

    #[test]
    fn f4_picks_the_reference_the_caret_is_in_not_the_first_one() {
        let src = "=A1+B2+C3";
        let caret = src.find("B2").unwrap() + 1;
        let (out, _) = cycle_at(src, caret).unwrap();
        assert_eq!(out, "=A1+$B$2+C3");
    }

    #[test]
    fn spans_fold_ranges_and_report_bounds() {
        let src = "=SUM(A1:B5)+C10";
        let sp = spans(src);
        assert_eq!(sp.len(), 2, "a range is one span, not two: {sp:?}");
        assert!(sp[0].is_range());
        assert_eq!(sp[0].bounds(), (0, 0, 4, 1), "A1:B5 in 0-based rows/cols");
        assert!(!sp[1].is_range());
        assert_eq!(sp[1].bounds(), (9, 2, 9, 2), "C10");
        assert_eq!(&src[sp[0].start..sp[0].end], "A1:B5");
    }

    #[test]
    fn spans_ignore_what_the_scanner_ignores() {
        let src = "=IF(A1>0,\"B2\",Sheet1!C3)+SUM(D4:D9)";
        let got: Vec<&str> = spans(src)
            .iter()
            .map(|s| &src[s.start..s.end])
            .collect::<Vec<_>>();
        assert_eq!(got, vec!["A1", "C3", "D4:D9"]);
    }

    #[test]
    fn spans_do_not_swallow_a_name_after_a_colon() {
        // `A1:total` is not a range this may rewrite; folding it would eat the
        // name and produce a formula the user never wrote.
        let src = "=A1:total";
        let sp = spans(src);
        assert_eq!(&src[sp[0].start..sp[0].end], "A1");
        assert!(!sp[0].is_range());
    }

    #[test]
    fn dragging_a_reference_moves_it_and_keeps_its_anchoring() {
        let src = "=SUM($A$1:B5)+C3";
        let sp = spans(src);
        let out = shift_span(src, &sp[0], 2, 1).expect("in bounds");
        assert_eq!(
            out, "=SUM($B$3:C7)+C3",
            "both endpoints move; the $ markers stay exactly where they were"
        );
    }

    #[test]
    fn dragging_off_the_sheet_is_refused_rather_than_clamped() {
        let src = "=A1+B2";
        let sp = spans(src);
        assert!(
            shift_span(src, &sp[0], -1, 0).is_none(),
            "A1 cannot move up; the formula must be left alone"
        );
        assert!(shift_span(src, &sp[0], 0, -1).is_none(), "nor left");
    }

    #[test]
    fn span_at_finds_nothing_outside_a_reference() {
        let src = "=A1  +  B2";
        assert!(span_at(src, 4).is_none(), "caret in the whitespace");
        assert_eq!(span_at(src, 3).map(|s| s.start), Some(1), "just after A1");
    }
}
