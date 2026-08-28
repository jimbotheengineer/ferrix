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

/// Find every reference-candidate word in `src`, in order.
///
/// Skips, in the same way and for the same reasons the tokenizer does:
///
/// - **string literals** — `"A1"` is text the user typed, not a reference;
/// - **quoted sheet names** — `'Q1 2024'!A1` contains `Q1`, a perfectly valid
///   reference, which would otherwise be rewritten and silently rename the
///   sheet the formula points at;
/// - **sheet qualifiers** — in `Sheet1!A1` only `A1` is a reference;
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

        // Skip quoted sheet names wholesale.
        if ch == b'\'' {
            i += 1;
            while i < b.len() {
                if b[i] == b'\'' {
                    if b.get(i + 1) == Some(&b'\'') {
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

        // A word starts at $ or a letter.
        if ch == b'$' || ch.is_ascii_alphabetic() || ch == b'_' {
            let start = i;
            while i < b.len()
                && (b[i].is_ascii_alphanumeric() || b[i] == b'_' || b[i] == b'$' || b[i] == b'.')
            {
                i += 1;
            }

            // A bare word followed by `!` is a sheet qualifier, not a cell.
            if b.get(i) == Some(&b'!') {
                continue;
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
}
