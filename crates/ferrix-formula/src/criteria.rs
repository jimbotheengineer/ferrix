//! Excel-compatible criteria matching, in one place.
//!
//! Everything that has to answer "does this cell match `">100"` / `"<>foo"` /
//! `"a?c*"`?" goes through this module. That is deliberate: collation
//! (case-folding), wildcard syntax (`*`, `?`, `~` escapes) and the
//! cross-type ordering rules are the kind of thing that silently drifts apart
//! when each function grows its own copy. `SUMIF`/`COUNTIFS` use it today;
//! `SEARCH`/`SUBSTITUTE` will use the same [`fold`], [`eq_ignore_case`] and
//! [`Pattern`] when they land.
//!
//! ## Scale invariant
//!
//! A criterion is *compiled once* ([`Criterion::parse`]) and then matched
//! against a stream of [`Scalar`]s that borrow from the sheet. Matching a row
//! allocates nothing: no `String`, no `Vec`, no formatting. That is what lets
//! a `COUNTIFS` over a 200M-row column stay bounded by the viewport rather
//! than by the row count. Any change here that allocates inside
//! [`Criterion::matches`] breaks that invariant, and
//! `tests/criteria_alloc.rs` will fail.

use std::cmp::Ordering;

use ferrix_core::ErrorKind;

/// A cell as the matcher sees it: text borrows from the sheet's arena, so
/// building one is free.
#[derive(Clone, Copy, PartialEq, Debug)]
pub enum Scalar<'a> {
    Blank,
    Number(f64),
    Bool(bool),
    Text(&'a str),
    Error(ErrorKind),
}

/// The comparison a criteria string leads with.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum CompareOp {
    Eq,
    Ne,
    Lt,
    Le,
    Gt,
    Ge,
}

impl CompareOp {
    /// Does this operator accept `ord` as a match?
    #[inline]
    fn accepts(self, ord: Ordering) -> bool {
        match self {
            CompareOp::Eq => ord == Ordering::Equal,
            CompareOp::Ne => ord != Ordering::Equal,
            CompareOp::Lt => ord == Ordering::Less,
            CompareOp::Le => ord != Ordering::Greater,
            CompareOp::Gt => ord == Ordering::Greater,
            CompareOp::Ge => ord != Ordering::Less,
        }
    }

    /// Result for a pair of values that are simply not comparable (a number
    /// against a boolean, say). Excel treats those as "different but not
    /// ordered": `<>` is true, everything else is false.
    #[inline]
    fn incomparable(self) -> bool {
        self == CompareOp::Ne
    }
}

// --- collation ------------------------------------------------------------

/// The single definition of case folding used by every text comparison in the
/// formula engine.
///
/// ASCII is folded branchlessly; non-ASCII uses the first char of
/// `to_lowercase`, which is exact for the overwhelming majority of scripts and
/// — critically — allocation-free. Full Unicode special-casing (ß → ss) would
/// need a `String` per comparison, which the scale invariant forbids.
#[inline]
pub fn fold(c: char) -> char {
    if c.is_ascii() {
        c.to_ascii_lowercase()
    } else {
        c.to_lowercase().next().unwrap_or(c)
    }
}

/// Case-insensitive string equality under [`fold`]. Allocation-free.
#[inline]
pub fn eq_ignore_case(a: &str, b: &str) -> bool {
    let mut bi = b.chars();
    for ca in a.chars() {
        match bi.next() {
            Some(cb) if fold(ca) == fold(cb) => {}
            _ => return false,
        }
    }
    bi.next().is_none()
}

/// Case-insensitive lexicographic ordering under [`fold`]. Allocation-free.
pub fn cmp_ignore_case(a: &str, b: &str) -> Ordering {
    let mut bi = b.chars();
    for ca in a.chars() {
        match bi.next() {
            None => return Ordering::Greater,
            Some(cb) => match fold(ca).cmp(&fold(cb)) {
                Ordering::Equal => {}
                other => return other,
            },
        }
    }
    if bi.next().is_some() {
        Ordering::Less
    } else {
        Ordering::Equal
    }
}

/// Case-insensitive substring search returning a *byte* offset into `hay`.
///
/// Not used by the aggregates; it is here so `SEARCH` inherits exactly the
/// collation the criteria matcher uses instead of reaching for
/// `to_lowercase()` and a second, subtly different, definition.
pub fn find_ignore_case(hay: &str, needle: &str) -> Option<usize> {
    if needle.is_empty() {
        return Some(0);
    }
    for (off, _) in hay.char_indices() {
        if lit_prefix_len(&hay[off..], needle, true).is_some() {
            return Some(off);
        }
    }
    None
}

// --- wildcard patterns ----------------------------------------------------

#[derive(Clone, Debug, PartialEq, Eq)]
enum Seg {
    /// `*` — any run of characters, including none.
    Star,
    /// `?` — exactly one character.
    AnyOne,
    /// A literal run, stored pre-folded so matching never folds the pattern.
    Lit(String),
}

/// A compiled Excel wildcard pattern.
///
/// Compilation happens once per criterion; [`Pattern::matches`] then runs over
/// borrowed text with no allocation. `~` escapes the next character, so
/// `~*` matches a literal asterisk.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Pattern {
    segs: Vec<Seg>,
    /// The pattern with escapes resolved and wildcards taken literally. Used
    /// for ordering comparisons, where Excel ignores wildcard-ness.
    literal: String,
    has_wildcard: bool,
}

impl Pattern {
    /// Compile Excel wildcard syntax. Never fails: anything unrecognised is a
    /// literal, which is what a spreadsheet user expects.
    pub fn compile(src: &str) -> Pattern {
        let mut segs: Vec<Seg> = Vec::new();
        let mut literal = String::with_capacity(src.len());
        let mut lit = String::new();
        let mut has_wildcard = false;
        let mut chars = src.chars();

        while let Some(c) = chars.next() {
            match c {
                '~' => {
                    // Escape: the next char is literal. A trailing lone `~` is
                    // itself literal, matching Excel's forgiving lexer.
                    let e = chars.next().unwrap_or('~');
                    lit.push(fold(e));
                    literal.push(e);
                }
                '*' | '?' => {
                    has_wildcard = true;
                    literal.push(c);
                    if !lit.is_empty() {
                        segs.push(Seg::Lit(std::mem::take(&mut lit)));
                    }
                    if c == '*' {
                        // Collapse `**` so backtracking stays linear-ish.
                        if segs.last() != Some(&Seg::Star) {
                            segs.push(Seg::Star);
                        }
                    } else {
                        segs.push(Seg::AnyOne);
                    }
                }
                other => {
                    lit.push(fold(other));
                    literal.push(other);
                }
            }
        }
        if !lit.is_empty() {
            segs.push(Seg::Lit(lit));
        }
        Pattern {
            segs,
            literal,
            has_wildcard,
        }
    }

    /// The pattern text with escapes resolved — what ordering comparisons use.
    #[inline]
    pub fn literal(&self) -> &str {
        &self.literal
    }

    #[inline]
    pub fn has_wildcard(&self) -> bool {
        self.has_wildcard
    }

    /// Whole-string match, case-insensitive. Allocation-free.
    pub fn matches(&self, text: &str) -> bool {
        if !self.has_wildcard {
            return eq_ignore_case(text, &self.literal);
        }
        // Classic greedy glob with single-star backtracking. Positions are
        // byte offsets into `text`; every advance is by a whole char.
        let segs = &self.segs;
        let mut si = 0usize;
        let mut ti = 0usize;
        let mut star: Option<(usize, usize)> = None;

        loop {
            if si < segs.len() {
                let advanced = match &segs[si] {
                    Seg::Star => {
                        star = Some((si, ti));
                        si += 1;
                        true
                    }
                    Seg::AnyOne => match text[ti..].chars().next() {
                        Some(c) => {
                            ti += c.len_utf8();
                            si += 1;
                            true
                        }
                        None => false,
                    },
                    Seg::Lit(lit) => match lit_prefix_len(&text[ti..], lit, false) {
                        Some(n) => {
                            ti += n;
                            si += 1;
                            true
                        }
                        None => false,
                    },
                };
                if advanced {
                    continue;
                }
            } else if ti == text.len() {
                return true;
            }
            // Mismatch (or trailing text with no segments left): give the most
            // recent `*` one more character and retry.
            match star {
                Some((ssi, sti)) => {
                    let Some(c) = text[sti..].chars().next() else {
                        return false;
                    };
                    let next = sti + c.len_utf8();
                    star = Some((ssi, next));
                    ti = next;
                    si = ssi + 1;
                }
                None => return false,
            }
        }
    }
}

/// If `rest` starts with `lit` (case-insensitively), the number of bytes of
/// `rest` consumed.
///
/// `fold_lit` says whether `lit` still needs folding: patterns pre-fold their
/// literals, [`find_ignore_case`] does not.
#[inline]
fn lit_prefix_len(rest: &str, lit: &str, fold_lit: bool) -> Option<usize> {
    let mut n = 0usize;
    let mut li = lit.chars();
    for c in rest.chars() {
        let Some(l) = li.next() else { break };
        let l = if fold_lit { fold(l) } else { l };
        if fold(c) != l {
            return None;
        }
        n += c.len_utf8();
    }
    if li.next().is_some() {
        None
    } else {
        Some(n)
    }
}

// --- criteria -------------------------------------------------------------

#[derive(Clone, Debug, PartialEq)]
enum Operand {
    Number(f64),
    Bool(bool),
    Text(Pattern),
    /// The empty criteria string, which is how Excel spells "blank".
    Empty,
}

/// A compiled criterion: an operator plus a typed right-hand side.
///
/// Build once, match many. See the module docs on why that ordering matters.
#[derive(Clone, Debug, PartialEq)]
pub struct Criterion {
    op: CompareOp,
    operand: Operand,
}

impl Criterion {
    /// Parse Excel criteria syntax: an optional leading `<>`, `>=`, `<=`,
    /// `>`, `<` or `=`, then a value. A bare value means equality.
    ///
    /// The right-hand side is typed by *what it looks like*: `">100"` is a
    /// numeric comparison, `">apple"` a textual one. That is Excel's rule and
    /// it is why `COUNTIF(A:A,"5")` and `COUNTIF(A:A,5)` agree.
    pub fn parse(src: &str) -> Criterion {
        let (op, rest) = if let Some(r) = src.strip_prefix("<>") {
            (CompareOp::Ne, r)
        } else if let Some(r) = src.strip_prefix(">=") {
            (CompareOp::Ge, r)
        } else if let Some(r) = src.strip_prefix("<=") {
            (CompareOp::Le, r)
        } else if let Some(r) = src.strip_prefix('>') {
            (CompareOp::Gt, r)
        } else if let Some(r) = src.strip_prefix('<') {
            (CompareOp::Lt, r)
        } else if let Some(r) = src.strip_prefix('=') {
            (CompareOp::Eq, r)
        } else {
            (CompareOp::Eq, src)
        };
        Criterion {
            op,
            operand: parse_operand(rest),
        }
    }

    /// Equality against a number, for `COUNTIF(A:A, 5)` where the criterion
    /// arrives already typed.
    pub fn eq_number(n: f64) -> Criterion {
        Criterion {
            op: CompareOp::Eq,
            operand: Operand::Number(n),
        }
    }

    /// Equality against a boolean.
    pub fn eq_bool(b: bool) -> Criterion {
        Criterion {
            op: CompareOp::Eq,
            operand: Operand::Bool(b),
        }
    }

    #[inline]
    pub fn op(&self) -> CompareOp {
        self.op
    }

    /// Does `cell` satisfy this criterion? Allocation-free.
    pub fn matches(&self, cell: Scalar<'_>) -> bool {
        let op = self.op;
        match (cell, &self.operand) {
            // Error cells never satisfy a criterion, not even `<>x`. They are
            // not "a different value", they are the absence of one; counting
            // them under `<>` would make COUNTIF disagree with COUNTIFS the
            // moment one range holds a #DIV/0!.
            (Scalar::Error(_), _) => false,

            (Scalar::Blank, Operand::Empty) => op == CompareOp::Eq,
            // Excel matches blanks against `<>something`: a blank cell is
            // indeed not that something.
            (Scalar::Blank, _) => op.incomparable(),

            (Scalar::Number(a), Operand::Number(b)) => match a.partial_cmp(b) {
                Some(ord) => op.accepts(ord),
                // NaN: unordered, so only `<>` holds.
                None => op.incomparable(),
            },
            // Numbers sort before text, matching the evaluator's `compare`.
            (Scalar::Number(_), Operand::Text(_)) => op.accepts(Ordering::Less),
            (Scalar::Number(_), Operand::Bool(_) | Operand::Empty) => op.incomparable(),

            (Scalar::Text(t), Operand::Text(p)) => match op {
                // Wildcards are an equality-only feature; ordering uses the
                // literal spelling, exactly as Excel does.
                CompareOp::Eq => p.matches(t),
                CompareOp::Ne => !p.matches(t),
                _ => op.accepts(cmp_ignore_case(t, p.literal())),
            },
            (Scalar::Text(_), Operand::Number(_)) => op.accepts(Ordering::Greater),
            (Scalar::Text(t), Operand::Empty) => match op {
                CompareOp::Eq => t.is_empty(),
                CompareOp::Ne => !t.is_empty(),
                _ => false,
            },
            (Scalar::Text(_), Operand::Bool(_)) => op.incomparable(),

            (Scalar::Bool(a), Operand::Bool(b)) => op.accepts(a.cmp(b)),
            // Booleans are their own type in criteria: TRUE is not 1 here,
            // even though `as_number` coerces it in arithmetic.
            (Scalar::Bool(_), _) => op.incomparable(),
        }
    }
}

/// Type the right-hand side of a criterion the way Excel does.
fn parse_operand(rest: &str) -> Operand {
    if rest.is_empty() {
        return Operand::Empty;
    }
    if let Ok(n) = rest.trim().parse::<f64>() {
        // Reject the textual infinities/NaN that Rust accepts but a
        // spreadsheet does not: "inf" is a word, not a number.
        if n.is_finite() {
            return Operand::Number(n);
        }
    }
    if rest.eq_ignore_ascii_case("TRUE") {
        return Operand::Bool(true);
    }
    if rest.eq_ignore_ascii_case("FALSE") {
        return Operand::Bool(false);
    }
    Operand::Text(Pattern::compile(rest))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn m(crit: &str, cell: Scalar<'_>) -> bool {
        Criterion::parse(crit).matches(cell)
    }

    #[test]
    fn bare_value_is_equality() {
        assert!(m("100", Scalar::Number(100.0)));
        assert!(!m("100", Scalar::Number(101.0)));
        assert!(m("apple", Scalar::Text("apple")));
        assert!(!m("apple", Scalar::Text("apricot")));
    }

    #[test]
    fn equality_is_case_insensitive() {
        assert!(m("APPLE", Scalar::Text("apple")));
        assert!(m("aPpLe", Scalar::Text("ApPlE")));
        assert!(m("=Apple", Scalar::Text("APPLE")));
    }

    #[test]
    fn numeric_comparisons() {
        assert!(m(">100", Scalar::Number(101.0)));
        assert!(!m(">100", Scalar::Number(100.0)));
        assert!(m(">=100", Scalar::Number(100.0)));
        assert!(m("<=5", Scalar::Number(5.0)));
        assert!(m("<5", Scalar::Number(4.9)));
        assert!(!m("<5", Scalar::Number(5.0)));
        assert!(m("<>7", Scalar::Number(8.0)));
        assert!(!m("<>7", Scalar::Number(7.0)));
    }

    #[test]
    fn quoted_number_and_bare_number_agree() {
        // COUNTIF(A:A,"5") must match COUNTIF(A:A,5).
        assert_eq!(Criterion::parse("5"), Criterion::eq_number(5.0));
        assert!(m("5", Scalar::Number(5.0)));
        assert!(Criterion::eq_number(5.0).matches(Scalar::Number(5.0)));
    }

    #[test]
    fn text_comparison_uses_collation_not_bytes() {
        // 'B' (0x42) < 'a' (0x61) as bytes; folded, apple < banana.
        assert!(m(">apple", Scalar::Text("Banana")));
        assert!(!m(">banana", Scalar::Text("Apple")));
        assert_eq!(cmp_ignore_case("Apple", "apple"), Ordering::Equal);
    }

    #[test]
    fn not_equal_text() {
        assert!(m("<>foo", Scalar::Text("bar")));
        assert!(!m("<>foo", Scalar::Text("FOO")));
    }

    #[test]
    fn wildcards_star_and_question() {
        assert!(m("a*", Scalar::Text("apple")));
        assert!(m("*e", Scalar::Text("apple")));
        assert!(m("a*e", Scalar::Text("apple")));
        assert!(m("*", Scalar::Text("")));
        assert!(m("a?ple", Scalar::Text("apple")));
        assert!(!m("a?ple", Scalar::Text("aple")));
        assert!(!m("a?ple", Scalar::Text("appple")));
        assert!(m("?????", Scalar::Text("apple")));
        assert!(!m("?????", Scalar::Text("apples")));
    }

    #[test]
    fn wildcards_are_case_insensitive() {
        assert!(m("A*E", Scalar::Text("apple")));
        assert!(m("*PP*", Scalar::Text("Apple")));
    }

    #[test]
    fn wildcard_backtracking() {
        // The naive greedy matcher fails these without backtracking.
        assert!(Pattern::compile("*ab*ab*c").matches("xxabyyabzzc"));
        assert!(!Pattern::compile("*ab*ab*c").matches("xxabyyzzc"));
        assert!(Pattern::compile("*a*a*a").matches("aaa"));
        assert!(!Pattern::compile("*a*a*a*a").matches("aaa"));
        assert!(Pattern::compile("a*").matches("a"));
    }

    #[test]
    fn tilde_escapes_wildcards() {
        assert!(m("~*", Scalar::Text("*")));
        assert!(!m("~*", Scalar::Text("anything")));
        assert!(m("a~?c", Scalar::Text("a?c")));
        assert!(!m("a~?c", Scalar::Text("abc")));
        assert!(m("~~", Scalar::Text("~")));
        assert!(Pattern::compile("100~%").matches("100%"));
    }

    #[test]
    fn wildcards_ignored_for_ordering() {
        // ">a*" compares against the literal "a*", it does not glob.
        let c = Criterion::parse(">a*");
        assert_eq!(c.op(), CompareOp::Gt);
        assert!(c.matches(Scalar::Text("b")));
    }

    #[test]
    fn blanks() {
        assert!(m("", Scalar::Blank));
        assert!(m("=", Scalar::Blank));
        assert!(!m("<>", Scalar::Blank));
        // "<>" with no operand means "not blank".
        assert!(m("<>", Scalar::Text("x")));
        assert!(m("<>", Scalar::Number(1.0)));
        // A blank is genuinely not "foo", so <>foo counts it.
        assert!(m("<>foo", Scalar::Blank));
        assert!(!m("foo", Scalar::Blank));
        assert!(!m(">0", Scalar::Blank));
    }

    #[test]
    fn empty_text_is_not_blank_but_matches_empty_criteria() {
        assert!(m("", Scalar::Text("")));
        assert!(!m("", Scalar::Text("x")));
    }

    #[test]
    fn cross_type_never_orders() {
        // Numbers sort before text; neither is ever "equal" to the other.
        assert!(!m("apple", Scalar::Number(5.0)));
        assert!(m("<>apple", Scalar::Number(5.0)));
        assert!(m("<apple", Scalar::Number(5.0)));
        assert!(!m(">apple", Scalar::Number(5.0)));
        assert!(m(">5", Scalar::Text("apple")));
        assert!(!m("<5", Scalar::Text("apple")));
    }

    #[test]
    fn booleans_are_their_own_type() {
        assert!(m("TRUE", Scalar::Bool(true)));
        assert!(m("false", Scalar::Bool(false)));
        assert!(!m("TRUE", Scalar::Bool(false)));
        // TRUE is not 1 in criteria, unlike in arithmetic.
        assert!(!m("1", Scalar::Bool(true)));
        assert!(!m("TRUE", Scalar::Number(1.0)));
    }

    #[test]
    fn errors_never_match() {
        for c in ["", "<>", "<>foo", ">0", "5", "*"] {
            assert!(
                !m(c, Scalar::Error(ErrorKind::DivZero)),
                "criterion {c:?} matched an error cell"
            );
        }
    }

    #[test]
    fn nan_is_unordered() {
        assert!(!m(">0", Scalar::Number(f64::NAN)));
        assert!(!m("<0", Scalar::Number(f64::NAN)));
        assert!(m("<>0", Scalar::Number(f64::NAN)));
    }

    #[test]
    fn infinity_words_are_text_not_numbers() {
        // Rust parses "inf"/"NaN" as floats; a spreadsheet does not.
        assert!(m("inf", Scalar::Text("INF")));
        assert!(!m("inf", Scalar::Number(f64::INFINITY)));
    }

    #[test]
    fn non_ascii_folding() {
        assert!(eq_ignore_case("ÉCOLE", "école"));
        assert!(Pattern::compile("é*").matches("École"));
        assert!(Pattern::compile("?cole").matches("école"));
    }

    #[test]
    fn find_ignore_case_shares_collation() {
        assert_eq!(find_ignore_case("Hello World", "world"), Some(6));
        assert_eq!(find_ignore_case("Hello", "xyz"), None);
        assert_eq!(find_ignore_case("Hello", ""), Some(0));
        // Byte offset, not char offset — callers converting to a 1-based
        // SEARCH() index must go through char_indices.
        assert_eq!(find_ignore_case("école", "COLE"), Some(2));
    }
}
