//! Scanning formula source text for cell references.
//!
//! ## Why this is shared, and why it is text
//!
//! Two features need to rewrite the references in a formula: filling (offset
//! every relative reference) and structural reordering (send every reference
//! to wherever its column or row moved). Both must rewrite the SOURCE TEXT
//! rather than the AST, for the reason [`crate::fill`] documents at length:
//! `Expr::Ref` carries only a `CellRef` and discards the `$` markers the
//! tokenizer recorded, so an AST round-trip silently unpins every absolute
//! reference in the sheet.
//!
//! Getting "what is a reference" right is subtle — string literals, quoted
//! sheet names, function calls, and sheet qualifiers all contain things that
//! look exactly like `A1`. That logic lived once in `fill.rs` and is factored
//! out here so the reorder path cannot drift from the fill path. If they
//! disagreed, a formula would survive one operation and be corrupted by the
//! other.

use ferrix_core::column_name;

/// A word in the source that might be a cell reference, as a byte range.
///
/// Everything the scanner has already ruled out — text inside quotes, sheet
/// qualifiers, function names — is absent, so a caller can treat every word it
/// receives as a reference candidate and only has to decide whether it parses.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct RefWord {
    pub start: usize,
    pub end: usize,
}

/// A parsed `A1`-style reference, with its `$` anchoring preserved.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct ParsedRef {
    /// 0-based column.
    pub col: u32,
    /// 0-based row.
    pub row: u32,
    /// `$` was written before the column letters.
    pub abs_col: bool,
    /// `$` was written before the row digits.
    pub abs_row: bool,
}

impl ParsedRef {
    /// Render back to source text, restoring the `$` markers exactly as the
    /// user wrote them.
    ///
    /// This is the half of the round-trip that an AST rewrite cannot do,
    /// because by then the flags are gone.
    pub fn render(&self) -> String {
        let mut out = String::new();
        if self.abs_col {
            out.push('$');
        }
        out.push_str(&column_name(self.col));
        if self.abs_row {
            out.push('$');
        }
        out.push_str(&(self.row + 1).to_string());
        out
    }
}

/// Parse one word as an `A1` reference. `None` when it is not one.
///
/// Mirrors the tokenizer's rules, including the `XFD` width limit that stops a
/// long name like `TOTAL1` being read as a column.
pub fn parse_ref(word: &str) -> Option<ParsedRef> {
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
    Some(ParsedRef {
        col: col - 1,
        row: row_1based - 1,
        abs_col,
        abs_row,
    })
}

/// A sheet qualifier in formula TEXT: `Sheet1!`, `'My Sheet'!`, or the 3-D
/// form `Sheet1:Sheet3!`.
///
/// Byte spans are reported for each name **as written**, quotes included, so a
/// caller can splice a new (re-quoted) name over exactly those bytes and leave
/// the rest of the formula byte-identical. That is what a sheet rename needs
/// and what an AST round trip cannot give it.
#[derive(Clone, PartialEq, Eq, Debug)]
pub struct Qualifier {
    /// Byte offset of the first character of the qualifier.
    pub start: usize,
    /// Byte offset one past the `!`.
    pub end: usize,
    /// First (or only) sheet name, unquoted, with `''` unescaped.
    pub first: String,
    /// Byte range of the first name as written.
    pub first_span: (usize, usize),
    /// The second endpoint of a 3-D span, if this is one.
    pub last: Option<String>,
    /// Byte range of the second name as written.
    pub last_span: Option<(usize, usize)>,
}

/// Index one past the closing `'` of a quoted run starting at `at`.
fn skip_quoted(b: &[u8], at: usize) -> Option<usize> {
    let mut i = at + 1;
    while i < b.len() {
        if b[i] == b'\'' {
            // A doubled quote is an escaped quote, not the end.
            if b.get(i + 1) == Some(&b'\'') {
                i += 2;
                continue;
            }
            return Some(i + 1);
        }
        i += 1;
    }
    None
}

/// Read one sheet name at `at`: a quoted run, or a bare word.
///
/// Returns the decoded name and the index just past it (past the closing
/// quote, for a quoted name).
fn read_name(src: &str, at: usize) -> Option<(String, usize)> {
    let b = src.as_bytes();
    if b.get(at) == Some(&b'\'') {
        let end = skip_quoted(b, at)?;
        let inner = &src[at + 1..end - 1];
        return Some((inner.replace("''", "'"), end));
    }
    let mut i = at;
    while i < b.len()
        && (b[i].is_ascii_alphanumeric() || b[i] == b'_' || b[i] == b'$' || b[i] == b'.')
    {
        i += 1;
    }
    if i == at {
        return None;
    }
    Some((src[at..i].to_string(), i))
}

/// Parse a sheet qualifier starting exactly at `at`. `None` when there is not
/// one there.
pub fn qualifier_at(src: &str, at: usize) -> Option<(Qualifier, usize)> {
    let b = src.as_bytes();
    let (first, mut j) = read_name(src, at)?;
    let first_span = (at, j);
    let mut last = None;
    let mut last_span = None;

    // `Sheet1:Sheet3!` — but ONLY when the left name is not itself a cell
    // reference. `Sheet1!A1:Sheet1!B4` is a requalified 2-D range, and
    // reading `A1:Sheet1!` out of it as a 3-D span would hide the `A1` from
    // every reference rewriter. A sheet whose name is spelled exactly like a
    // cell reference cannot open a 3-D span here; Excel refuses such sheet
    // names outright, so nothing valid is lost.
    if b.get(j) == Some(&b':') && parse_ref(&first).is_none() {
        if let Some((second, k)) = read_name(src, j + 1) {
            if b.get(k) == Some(&b'!') {
                last = Some(second);
                last_span = Some((j + 1, k));
                j = k;
            }
        }
    }

    if b.get(j) != Some(&b'!') {
        return None;
    }
    Some((
        Qualifier {
            start: at,
            end: j + 1,
            first,
            first_span,
            last,
            last_span,
        },
        j + 1,
    ))
}

/// Every sheet qualifier in `src`, in order.
///
/// String literals are skipped, which is the asymmetry a sheet rename lives
/// or dies by: a sheet name may legitimately appear inside `"Sheet2 total"`,
/// and rewriting there would corrupt the user's text rather than repoint a
/// reference.
pub fn qualifiers(src: &str) -> Vec<Qualifier> {
    let b = src.as_bytes();
    let mut out = Vec::new();
    let mut i = 0usize;
    while i < b.len() {
        if b[i] == b'"' {
            i += 1;
            while i < b.len() {
                if b[i] == b'"' {
                    if b.get(i + 1) == Some(&b'"') {
                        i += 2;
                        continue;
                    }
                    i += 1;
                    break;
                }
                i += 1;
            }
            continue;
        }
        if let Some((q, past)) = qualifier_at(src, i) {
            out.push(q);
            i = past;
            continue;
        }
        if b[i] == b'\'' {
            i = skip_quoted(b, i).unwrap_or(b.len());
            continue;
        }
        // Consume a whole word before moving on. Advancing one byte would
        // restart the qualifier parse in the MIDDLE of a word, and a suffix
        // can look like a qualifier its whole word is not: in
        // `Sheet1!A1:Sheet1!B4` the tail `1:Sheet1!` reads as the 3-D span
        // `1`..`Sheet1`, which would hide the `A1` reference entirely.
        if b[i].is_ascii_alphanumeric() || b[i] == b'_' || b[i] == b'$' {
            while i < b.len()
                && (b[i].is_ascii_alphanumeric() || b[i] == b'_' || b[i] == b'$' || b[i] == b'.')
            {
                i += 1;
            }
            continue;
        }
        i += 1;
    }
    out
}

/// Find every reference-candidate word in `src`, in order.
///
/// Skips, in the same way and for the same reasons the tokenizer does:
///
/// - **string literals** — `"A1"` is text the user typed, not a reference;
/// - **quoted sheet names** — `'Q1 2024'!A1` contains `Q1`, a perfectly valid
///   reference, which would otherwise be rewritten and silently rename the
///   sheet the formula points at;
/// - **sheet qualifiers** — in `Sheet1!A1` only `A1` is a reference;
/// - **3-D sheet spans** — in `Sheet1:Sheet3!A1` neither endpoint name is a
///   reference, even though the `:` looks like a range operator;
/// - **function names** — `SUM(` is a call, never cell `SUM`. Note that a bare
///   `LOG10` with no paren IS a valid reference (column LOG, row 10), and
///   Excel agrees; only the paren disambiguates.
pub fn scan(src: &str) -> Vec<RefWord> {
    let b = src.as_bytes();
    let mut out = Vec::new();
    let mut i = 0usize;

    while i < b.len() {
        let ch = b[i];

        // Skip string literals wholesale.
        if ch == b'"' {
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
            continue;
        }

        // Skip a sheet qualifier — `Sheet1!`, `'Q1 2024'!`, or the two names
        // of a 3-D span `Sheet1:Sheet3!` — wholesale, INCLUDING the `!`. The
        // cell reference after it is left for the loop to pick up normally.
        if let Some((_, past)) = qualifier_at(src, i) {
            i = past;
            continue;
        }

        // A lone quoted run that is not a sheet qualifier is still not
        // reference text; skip it rather than reading `Q1` out of it.
        if ch == b'\'' {
            i = skip_quoted(b, i).unwrap_or(b.len());
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

            // A word immediately followed by '(' is a function call.
            let mut j = i;
            while j < b.len() && (b[j] == b' ' || b[j] == b'\t') {
                j += 1;
            }
            if b.get(j) == Some(&b'(') {
                continue;
            }

            out.push(RefWord { start, end: i });
            continue;
        }

        i += 1;
    }
    out
}

/// True when the next non-space byte after `at` is a `:` — i.e. this word is
/// the left endpoint of a range like `A1:B5`.
///
/// Reordering has to treat a range as a unit: if its endpoints move
/// independently the range can invert, and `B5:A1` is not what the user meant.
pub fn range_follows(src: &str, at: usize) -> bool {
    src.as_bytes()[at..]
        .iter()
        .find(|b| **b != b' ' && **b != b'\t')
        == Some(&b':')
}

/// Rebuild `src`, replacing the scanned words for which `f` returns `Some`.
///
/// Untouched bytes are copied verbatim, so the user's spacing, capitalisation,
/// and anything the scanner declined to look at survive exactly.
pub fn rewrite<F>(src: &str, words: &[RefWord], mut f: F) -> String
where
    F: FnMut(usize, &RefWord) -> Option<String>,
{
    let mut out = String::with_capacity(src.len());
    let mut cursor = 0usize;
    for (i, w) in words.iter().enumerate() {
        if let Some(text) = f(i, w) {
            out.push_str(&src[cursor..w.start]);
            out.push_str(&text);
            cursor = w.end;
        }
    }
    out.push_str(&src[cursor..]);
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn words(src: &str) -> Vec<&str> {
        scan(src).iter().map(|w| &src[w.start..w.end]).collect()
    }

    #[test]
    fn finds_plain_references() {
        assert_eq!(words("=A1+B2"), vec!["A1", "B2"]);
        assert_eq!(words("=SUM(A1:A10)"), vec!["A1", "A10"]);
    }

    #[test]
    fn skips_function_names_but_not_bare_words() {
        assert_eq!(words("=LOG10(A1)"), vec!["A1"]);
        // No paren: LOG10 is column LOG, row 10 — a real reference.
        assert_eq!(words("=LOG10"), vec!["LOG10"]);
        assert_eq!(words("=SUM (A1)"), vec!["A1"], "space before paren");
    }

    #[test]
    fn skips_text_literals() {
        assert_eq!(words("=\"A1\"&B2"), vec!["B2"]);
        assert_eq!(words("=IF(A1>0,\"B2 ok\",\"no\")"), vec!["A1"]);
        // A doubled quote is an escape, not the end of the literal.
        assert_eq!(words("=\"say \"\"A1\"\" now\"&C3"), vec!["C3"]);
    }

    #[test]
    fn skips_sheet_qualifiers_and_quoted_sheet_names() {
        assert_eq!(words("=Sheet1!A1"), vec!["A1"]);
        // Q1 inside the quotes is a valid reference and must NOT be scanned.
        assert_eq!(words("='Q1 2024'!A1"), vec!["A1"]);
    }

    #[test]
    fn parses_and_renders_preserving_dollars() {
        // The round-trip the AST cannot do.
        for src in ["A1", "$A1", "A$1", "$A$1", "AA10", "XFD1048576"] {
            let p = parse_ref(src).expect("should parse");
            assert_eq!(p.render(), src, "round-trip lost information");
        }
    }

    #[test]
    fn parse_rejects_non_references() {
        assert_eq!(parse_ref("TOTAL"), None, "no digits");
        assert_eq!(parse_ref("A0"), None, "row 0 does not exist");
        assert_eq!(parse_ref("ABCD1"), None, "wider than XFD");
        assert_eq!(parse_ref("A1B"), None, "trailing junk");
        assert_eq!(parse_ref("123"), None, "no letters");
    }

    #[test]
    fn parse_is_zero_based_internally() {
        let p = parse_ref("B3").unwrap();
        assert_eq!((p.col, p.row), (1, 2));
        assert!(!p.abs_col && !p.abs_row);
        let p = parse_ref("$B$3").unwrap();
        assert!(p.abs_col && p.abs_row);
    }

    #[test]
    fn detects_range_endpoints() {
        let src = "=SUM(A1:B5)+C1";
        let ws = scan(src);
        assert!(range_follows(src, ws[0].end), "A1 opens a range");
        assert!(!range_follows(src, ws[1].end), "B5 closes it");
        assert!(!range_follows(src, ws[2].end), "C1 is standalone");
        // Whitespace before the colon must not hide the range.
        let src = "=SUM(A1 : B5)";
        let ws = scan(src);
        assert!(range_follows(src, ws[0].end));
    }

    #[test]
    fn rewrite_replaces_only_selected_words() {
        let src = "= A1 + B2 ";
        let ws = scan(src);
        let out = rewrite(src, &ws, |i, _| (i == 0).then(|| "Z9".to_string()));
        assert_eq!(out, "= Z9 + B2 ", "spacing must survive untouched");
    }

    #[test]
    fn rewrite_with_no_replacements_is_the_identity() {
        let src = "=SUM($A$1:A1)*LOG10(B2)+\"text\"";
        let ws = scan(src);
        assert_eq!(rewrite(src, &ws, |_, _| None), src);
    }

    // --- sheet qualifiers (issue #43) ---

    fn quals(src: &str) -> Vec<(String, Option<String>)> {
        qualifiers(src)
            .into_iter()
            .map(|q| (q.first, q.last))
            .collect()
    }

    #[test]
    fn finds_bare_and_quoted_sheet_qualifiers() {
        assert_eq!(quals("=Sheet1!A1"), vec![("Sheet1".into(), None)]);
        assert_eq!(quals("='Q1 2024'!A1"), vec![("Q1 2024".into(), None)]);
        // `''` is an escaped quote inside the name.
        assert_eq!(
            quals("='Bob''s Data'!A1"),
            vec![("Bob's Data".into(), None)]
        );
        assert_eq!(
            quals("=Sheet1!A1+Sheet2!B2"),
            vec![("Sheet1".into(), None), ("Sheet2".into(), None)]
        );
    }

    #[test]
    fn finds_both_endpoints_of_a_three_d_span() {
        assert_eq!(
            quals("=SUM(Sheet1:Sheet3!A1)"),
            vec![("Sheet1".into(), Some("Sheet3".into()))]
        );
        assert_eq!(
            quals("=SUM('Q1 2024':'Q4 2024'!A1)"),
            vec![("Q1 2024".into(), Some("Q4 2024".into()))]
        );
    }

    #[test]
    fn a_sheet_name_inside_a_string_literal_is_not_a_qualifier() {
        // THE asymmetry a sheet rename lives or dies by. `"Sheet2!"` is the
        // user's text; rewriting it would corrupt data rather than repoint a
        // reference.
        assert_eq!(
            quals("=Sheet2!A1&\" from Sheet2!\""),
            vec![("Sheet2".into(), None)],
            "only the real qualifier may be reported"
        );
        assert!(quals("=\"Sheet2!A1\"").is_empty());
    }

    #[test]
    fn a_two_d_range_is_not_read_as_a_three_d_span() {
        // `Sheet1!A1:B4` — the `:` belongs to the RANGE, not to a sheet run.
        // If it were misread, `A1` would disappear from the scanner's output
        // and no reference rewrite could ever see it again.
        assert_eq!(quals("=Sheet1!A1:B4"), vec![("Sheet1".into(), None)]);
        assert_eq!(words("=Sheet1!A1:B4"), vec!["A1", "B4"]);
        // Requalified far corner: two qualifiers, both endpoints scanned.
        assert_eq!(
            quals("=Sheet1!A1:Sheet1!B4"),
            vec![("Sheet1".into(), None), ("Sheet1".into(), None)]
        );
        assert_eq!(words("=Sheet1!A1:Sheet1!B4"), vec!["A1", "B4"]);
    }

    #[test]
    fn a_three_d_spans_endpoint_names_are_not_scanned_as_references() {
        // `Q1` in `'Q1 2024':'Q4 2024'!A1` is a perfectly valid reference
        // spelling. Scanning it would let a column remap silently rename the
        // sheet the formula points at.
        assert_eq!(words("=SUM(Sheet1:Sheet3!A1)"), vec!["A1"]);
        assert_eq!(words("=SUM('Q1 2024':'Q4 2024'!A1:B9)"), vec!["A1", "B9"]);
    }

    #[test]
    fn qualifier_spans_cover_the_names_as_written() {
        let src = "=SUM('Q1 2024':Sheet3!A1)";
        let q = &qualifiers(src)[0];
        assert_eq!(
            &src[q.first_span.0..q.first_span.1],
            "'Q1 2024'",
            "the span must include the quotes so a rename can re-quote"
        );
        let last = q.last_span.expect("3-D span has a second endpoint");
        assert_eq!(&src[last.0..last.1], "Sheet3");
        assert_eq!(&src[q.start..q.end], "'Q1 2024':Sheet3!");
    }
}
