//! Excel text functions: LEFT, RIGHT, MID, LEN, UPPER, LOWER, PROPER, TRIM,
//! CLEAN, SUBSTITUTE, REPLACE, FIND, SEARCH, CONCAT/CONCATENATE, TEXTJOIN,
//! TEXT, VALUE, REPT.
//!
//! Everything lives here rather than in `eval.rs` so this whole feature is one
//! file plus a single delegating arm in the evaluator's `match`.
//!
//! ## Four rules this module holds to
//!
//! **1. One matcher, not two.** Case-insensitive comparison and wildcard
//! collation are defined exactly once, in [`crate::criteria`]. `SEARCH` uses
//! [`Pattern::compile`](crate::criteria::Pattern::compile) and
//! [`find_ignore_case`](crate::criteria::find_ignore_case); it does not reach
//! for `to_lowercase()`. `FIND` is case-*sensitive* and wildcard-free, so it is
//! a plain byte search — deliberately not routed through the matcher, because
//! routing it there would be routing it through case folding it must not do.
//!
//! **2. Characters, never bytes.** `LEN("café") == 4`, and `MID`/`LEFT`/`RIGHT`
//! cut on `char_indices` boundaries, so no result can split a multi-byte
//! character. Positions crossing the API are 1-based *character* positions;
//! byte offsets exist only inside a function body, and every one of them comes
//! from `char_indices` or from the matcher (which advances by whole chars).
//!
//! **3. One format engine.** `TEXT` calls
//! [`NumFmt::parse`](ferrix_core::numfmt::NumFmt::parse) and renders through
//! it. There is no second format parser here.
//!
//! **4. Results intern.** A text result is an arena [`StrId`], not a `String`,
//! and interning deduplicates — see the module docs on
//! [`ferrix_core::arena`] for why a process-wide interner is where formula
//! results have to land given that `CellSource` is `&self`-only, and for the
//! cost that choice carries.
//!
//! ## Scale invariant
//!
//! Nothing here materialises anything proportional to a row count.
//! Cell-at-a-time functions (`LEN`, `UPPER`, `MID`, …) are O(len of that one
//! cell) and touch exactly one cell. The two functions that accept ranges
//! (`CONCAT`, `TEXTJOIN`) stream them and stop at [`MAX_TEXT_LEN`], so their
//! peak is a 32k buffer whether the range spans a thousand rows or 200 million.

use std::borrow::Cow;

use ferrix_core::arena::intern_formula_text;
use ferrix_core::numfmt::NumFmt;
use ferrix_core::{format_number, CellRef, ErrorKind, Value};

use crate::criteria::{find_ignore_case, Pattern};
use crate::eval::{eval_view, CellSource};
use crate::parser::Expr;

#[cfg(test)]
mod tests;

/// Excel's per-cell text ceiling. Producing more than this is `#VALUE!`
/// rather than an unbounded allocation, which is what keeps `CONCAT` over a
/// 200M-row range bounded by the viewport instead of by the data.
pub const MAX_TEXT_LEN: usize = 32_767;

/// Is `name` handled by this module?
///
/// Used by the one delegating arm in `eval::eval_call`, which is the only
/// edit this feature makes to the evaluator.
pub fn is_text_fn(name: &str) -> bool {
    matches!(
        name,
        "LEFT"
            | "RIGHT"
            | "MID"
            | "LEN"
            | "UPPER"
            | "LOWER"
            | "PROPER"
            | "TRIM"
            | "CLEAN"
            | "SUBSTITUTE"
            | "REPLACE"
            | "FIND"
            | "SEARCH"
            | "CONCAT"
            | "CONCATENATE"
            | "TEXTJOIN"
            | "TEXT"
            | "VALUE"
            | "REPT"
    )
}

/// Evaluate a text function. Assumes [`is_text_fn`] said yes.
pub fn call<S: CellSource + ?Sized>(name: &str, args: &[Expr], src: &S) -> Value {
    match dispatch(name, args, src) {
        Ok(v) => v,
        Err(e) => Value::Error(e),
    }
}

fn dispatch<S: CellSource + ?Sized>(
    name: &str,
    args: &[Expr],
    src: &S,
) -> Result<Value, ErrorKind> {
    match name {
        "LEN" => {
            let s = one_text(args, src)?;
            // O(chars in THIS cell). Nothing about the column's height is
            // read, which is what makes LEN on a 200M-row column O(1) per
            // cell rather than O(rows).
            Ok(Value::Number(s.chars().count() as f64))
        }
        "LEFT" | "RIGHT" => {
            let s = text_at(args, 0, src)?;
            let n = opt_count(args, 1, src, 1.0)?;
            let len = s.chars().count();
            let n = n.min(len);
            let cut = if name == "LEFT" {
                sub_chars(&s, 0, n)
            } else {
                sub_chars(&s, len - n, n)
            };
            Ok(text_value(cut))
        }
        "MID" => {
            if args.len() != 3 {
                return Err(ErrorKind::Value);
            }
            let s = text_at(args, 0, src)?;
            let start = number_at(args, 1, src)?;
            let count = opt_count(args, 2, src, 0.0)?;
            // Excel: start_num < 1 is #VALUE!; a start PAST the end is an
            // empty string, not an error. (FIND/SEARCH are the functions
            // where a past-the-end position is #VALUE! — see below.)
            if start < 1.0 {
                return Err(ErrorKind::Value);
            }
            let start = (start.trunc() as usize) - 1;
            Ok(text_value(sub_chars(&s, start, count)))
        }
        "UPPER" | "LOWER" => {
            let s = one_text(args, src)?;
            // Full Unicode case mapping, not ASCII: `to_uppercase` on a char
            // can yield several chars (ß -> SS), and truncating that to one
            // would silently corrupt non-English text.
            let out: String = if name == "UPPER" {
                s.chars().flat_map(char::to_uppercase).collect()
            } else {
                s.chars().flat_map(char::to_lowercase).collect()
            };
            Ok(text_value(&out))
        }
        "PROPER" => {
            let s = one_text(args, src)?;
            Ok(text_value(&proper(&s)))
        }
        "TRIM" => {
            let s = one_text(args, src)?;
            Ok(text_value(&trim_spaces(&s)))
        }
        "CLEAN" => {
            let s = one_text(args, src)?;
            // Excel CLEAN strips the 32 ASCII control codes and nothing else;
            // it is not a general "printable" filter.
            let out: String = s.chars().filter(|c| (*c as u32) >= 32).collect();
            Ok(text_value(&out))
        }
        "REPT" => {
            if args.len() != 2 {
                return Err(ErrorKind::Value);
            }
            let s = text_at(args, 0, src)?;
            let n = opt_count(args, 1, src, 0.0)?;
            // Check the product BEFORE building anything: `REPT("x", 1e9)`
            // must be an error, not a gigabyte.
            if s.len().saturating_mul(n) > MAX_TEXT_LEN {
                return Err(ErrorKind::Value);
            }
            Ok(text_value(&s.repeat(n)))
        }
        "SUBSTITUTE" => substitute(args, src),
        "REPLACE" => replace(args, src),
        "FIND" | "SEARCH" => find_or_search(name == "SEARCH", args, src),
        "CONCAT" | "CONCATENATE" => concat(name, args, src),
        "TEXTJOIN" => textjoin(args, src),
        "TEXT" => {
            if args.len() != 2 {
                return Err(ErrorKind::Value);
            }
            let fmt = text_at(args, 1, src)?;
            // ONE format engine: the same NumFmt that renders cells on screen
            // and round-trips to .xlsx. No second parser lives here.
            let nf = NumFmt::parse(&fmt);
            let v = value_at(args, 0, src)?;
            let out = match v {
                Value::Text(id) => nf.render_text(src.resolve(id)),
                Value::Empty => nf.render(0.0),
                other => match other.as_number() {
                    Some(n) => nf.render(n),
                    None => return Err(ErrorKind::Value),
                },
            };
            Ok(text_value(&out))
        }
        "VALUE" => {
            let s = one_text(args, src)?;
            parse_value(&s).map(Value::Number).ok_or(ErrorKind::Value)
        }
        _ => Err(ErrorKind::Name),
    }
}

// --- argument plumbing ----------------------------------------------------

/// A cell or literal seen as text.
///
/// Text borrows — from the AST for a literal, from the sheet's arena for a
/// cell — so the common case costs nothing. Only numeric coercion, which has
/// to render digits, owns.
fn text_of<'a, S: CellSource + ?Sized>(
    arg: &'a Expr,
    src: &'a S,
) -> Result<Cow<'a, str>, ErrorKind> {
    if let Expr::Text(s) = arg {
        return Ok(Cow::Borrowed(s.as_str()));
    }
    value_text(eval_view(arg, src), src)
}

fn value_text<'a, S: CellSource + ?Sized>(v: Value, src: &'a S) -> Result<Cow<'a, str>, ErrorKind> {
    match v {
        Value::Text(id) => Ok(Cow::Borrowed(src.resolve(id))),
        Value::Number(n) => Ok(Cow::Owned(format_number(n))),
        Value::Bool(b) => Ok(Cow::Borrowed(if b { "TRUE" } else { "FALSE" })),
        Value::Empty => Ok(Cow::Borrowed("")),
        Value::Error(e) => Err(e),
    }
}

fn text_at<'a, S: CellSource + ?Sized>(
    args: &'a [Expr],
    i: usize,
    src: &'a S,
) -> Result<Cow<'a, str>, ErrorKind> {
    text_of(args.get(i).ok_or(ErrorKind::Value)?, src)
}

/// The single-argument shape shared by LEN/UPPER/LOWER/PROPER/TRIM/CLEAN/VALUE.
fn one_text<'a, S: CellSource + ?Sized>(
    args: &'a [Expr],
    src: &'a S,
) -> Result<Cow<'a, str>, ErrorKind> {
    if args.len() != 1 {
        return Err(ErrorKind::Value);
    }
    text_of(&args[0], src)
}

fn value_at<S: CellSource + ?Sized>(args: &[Expr], i: usize, src: &S) -> Result<Value, ErrorKind> {
    match args.get(i) {
        // A bare string literal has no `Value` in this engine; TEXT still has
        // to see it as text rather than as #VALUE!.
        Some(Expr::Text(s)) => match intern_formula_text(s) {
            Some(id) => Ok(Value::Text(id)),
            None => Err(ErrorKind::Value),
        },
        Some(other) => Ok(eval_view(other, src)),
        None => Err(ErrorKind::Value),
    }
}

fn number_at<S: CellSource + ?Sized>(args: &[Expr], i: usize, src: &S) -> Result<f64, ErrorKind> {
    let arg = args.get(i).ok_or(ErrorKind::Value)?;
    let v = eval_view(arg, src);
    if let Some(e) = v.error() {
        return Err(e);
    }
    match v {
        // A numeric string argument coerces, as Excel does for `MID(a,"2",1)`.
        Value::Text(id) => parse_value(src.resolve(id)).ok_or(ErrorKind::Value),
        other => other.as_number().ok_or(ErrorKind::Value),
    }
}

/// A non-negative count argument, defaulting when absent. Negative counts are
/// `#VALUE!`, matching Excel, rather than silently clamping to zero.
fn opt_count<S: CellSource + ?Sized>(
    args: &[Expr],
    i: usize,
    src: &S,
    default: f64,
) -> Result<usize, ErrorKind> {
    let n = if i < args.len() {
        number_at(args, i, src)?
    } else {
        default
    };
    if n < 0.0 || !n.is_finite() {
        return Err(ErrorKind::Value);
    }
    Ok(n.trunc() as usize)
}

// --- char-oriented slicing ------------------------------------------------

/// Byte offset of character `n`, clamped to the end of the string.
///
/// This is the only place a char index becomes a byte index, so it is the only
/// place a multi-byte split could ever happen — and it cannot, because every
/// offset comes from `char_indices`.
#[inline]
fn byte_of_char(s: &str, n: usize) -> usize {
    match s.char_indices().nth(n) {
        Some((b, _)) => b,
        None => s.len(),
    }
}

/// `count` characters starting at character `start`. Never splits a char.
#[inline]
fn sub_chars(s: &str, start: usize, count: usize) -> &str {
    let b0 = byte_of_char(s, start);
    let b1 = byte_of_char(s, start.saturating_add(count));
    &s[b0..b1]
}

// --- individual functions -------------------------------------------------

/// Excel PROPER: uppercase every letter that follows a non-letter.
fn proper(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let mut start_of_word = true;
    for c in s.chars() {
        if start_of_word {
            out.extend(c.to_uppercase());
        } else {
            out.extend(c.to_lowercase());
        }
        start_of_word = !c.is_alphabetic();
    }
    out
}

/// Excel TRIM: drop leading/trailing spaces and collapse internal runs to one.
///
/// Only U+0020, deliberately — Excel does not touch tabs or newlines here, and
/// a `trim()`-based implementation would quietly eat them.
fn trim_spaces(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let mut pending_space = false;
    for c in s.chars() {
        if c == ' ' {
            pending_space = !out.is_empty();
            continue;
        }
        if pending_space {
            out.push(' ');
            pending_space = false;
        }
        out.push(c);
    }
    out
}

/// `SUBSTITUTE(text, old, new, [instance_num])`. Case-SENSITIVE, like Excel.
fn substitute<S: CellSource + ?Sized>(args: &[Expr], src: &S) -> Result<Value, ErrorKind> {
    if args.len() < 3 || args.len() > 4 {
        return Err(ErrorKind::Value);
    }
    let text = text_at(args, 0, src)?;
    let old = text_at(args, 1, src)?;
    let new = text_at(args, 2, src)?;
    let instance = if args.len() == 4 {
        let n = number_at(args, 3, src)?;
        if n < 1.0 {
            return Err(ErrorKind::Value);
        }
        Some(n.trunc() as usize)
    } else {
        None
    };
    // Excel returns the text untouched when there is nothing to look for.
    if old.is_empty() {
        return Ok(text_value(&text));
    }

    let mut out = String::with_capacity(text.len());
    let mut rest: &str = &text;
    let mut seen = 0usize;
    while let Some(pos) = rest.find(old.as_ref()) {
        seen += 1;
        out.push_str(&rest[..pos]);
        let replace_this = instance.is_none_or(|want| want == seen);
        if replace_this {
            out.push_str(&new);
        } else {
            out.push_str(&rest[pos..pos + old.len()]);
        }
        rest = &rest[pos + old.len()..];
        if out.len() > MAX_TEXT_LEN {
            return Err(ErrorKind::Value);
        }
        if instance == Some(seen) {
            break;
        }
    }
    out.push_str(rest);
    if out.chars().count() > MAX_TEXT_LEN {
        return Err(ErrorKind::Value);
    }
    Ok(text_value(&out))
}

/// `REPLACE(old_text, start_num, num_chars, new_text)`, positions 1-based.
fn replace<S: CellSource + ?Sized>(args: &[Expr], src: &S) -> Result<Value, ErrorKind> {
    if args.len() != 4 {
        return Err(ErrorKind::Value);
    }
    let old = text_at(args, 0, src)?;
    let start = number_at(args, 1, src)?;
    let count = opt_count(args, 2, src, 0.0)?;
    let new = text_at(args, 3, src)?;
    if start < 1.0 {
        return Err(ErrorKind::Value);
    }
    let start = (start.trunc() as usize) - 1;
    let head_end = byte_of_char(&old, start);
    let tail_start = byte_of_char(&old, start.saturating_add(count));
    let mut out = String::with_capacity(old.len() + new.len());
    out.push_str(&old[..head_end]);
    out.push_str(&new);
    out.push_str(&old[tail_start..]);
    if out.chars().count() > MAX_TEXT_LEN {
        return Err(ErrorKind::Value);
    }
    Ok(text_value(&out))
}

/// `FIND(find, within, [start])` and `SEARCH(find, within, [start])`.
///
/// The two differ in exactly two ways and share everything else:
///   * FIND is case-sensitive and has no wildcards.
///   * SEARCH is case-insensitive and accepts `*` / `?` (with `~` escaping),
///     both inherited from [`crate::criteria`] rather than reimplemented.
///
/// Both return a 1-based CHARACTER position, and both report `#VALUE!` when
/// `start` is past the end of `within` or the needle is not present — which is
/// the "position past the end is #VALUE!" rule.
fn find_or_search<S: CellSource + ?Sized>(
    wildcards: bool,
    args: &[Expr],
    src: &S,
) -> Result<Value, ErrorKind> {
    if args.len() < 2 || args.len() > 3 {
        return Err(ErrorKind::Value);
    }
    let needle = text_at(args, 0, src)?;
    let hay = text_at(args, 1, src)?;
    let start = if args.len() == 3 {
        let n = number_at(args, 2, src)?;
        if n < 1.0 {
            return Err(ErrorKind::Value);
        }
        (n.trunc() as usize) - 1
    } else {
        0
    };
    let hay_len = hay.chars().count();
    // Past the end of the haystack: #VALUE!, matching Excel. Note `>` not
    // `>=`: start == len is the position just after the last character, which
    // Excel accepts (and where only an empty needle can match).
    if start > hay_len {
        return Err(ErrorKind::Value);
    }
    let from = byte_of_char(&hay, start);
    let tail = &hay[from..];

    let rel_byte = if wildcards {
        // ONE matcher. `Pattern` is a whole-string glob, so anchoring it to a
        // start position and letting it run to the end of the string is spelt
        // by appending `*` — no second, subtly different, wildcard engine.
        let probe = Pattern::compile(&needle);
        if probe.has_wildcard() {
            let anchored = Pattern::compile(&format!("{needle}*"));
            let mut hit = None;
            for (off, _) in tail.char_indices() {
                if anchored.matches(&tail[off..]) {
                    hit = Some(off);
                    break;
                }
            }
            // An empty tail still has to be probed: `SEARCH("*","")` is 1.
            match hit {
                Some(off) => Some(off),
                None if anchored.matches(tail) => Some(0),
                None => None,
            }
        } else {
            // `literal()` is the needle with `~` escapes resolved, so
            // SEARCH("a~*b", ...) looks for a literal asterisk.
            find_ignore_case(tail, probe.literal())
        }
    } else {
        // FIND: case-sensitive, wildcard-free. Byte search is exact here
        // precisely because no folding happens.
        tail.find(needle.as_ref())
    };

    match rel_byte {
        Some(b) => {
            let rel_chars = tail[..b].chars().count();
            Ok(Value::Number((start + rel_chars + 1) as f64))
        }
        None => Err(ErrorKind::Value),
    }
}

/// `CONCAT(...)` / `CONCATENATE(...)`.
///
/// CONCAT accepts ranges (streamed, capped); CONCATENATE takes scalars only,
/// which is the Excel distinction.
fn concat<S: CellSource + ?Sized>(name: &str, args: &[Expr], src: &S) -> Result<Value, ErrorKind> {
    let allow_ranges = name == "CONCAT";
    let mut out = String::new();
    for a in args {
        if allow_ranges {
            for_each_text(a, src, &mut |s| {
                out.push_str(s);
                cap(&out)
            })?;
        } else {
            if matches!(a, Expr::Range(_, _) | Expr::XRange(_, _, _)) {
                return Err(ErrorKind::Value);
            }
            out.push_str(&text_of(a, src)?);
            cap(&out)?;
        }
    }
    Ok(text_value(&out))
}

/// `TEXTJOIN(delimiter, ignore_empty, text...)`.
fn textjoin<S: CellSource + ?Sized>(args: &[Expr], src: &S) -> Result<Value, ErrorKind> {
    if args.len() < 3 {
        return Err(ErrorKind::Value);
    }
    let delim = text_at(args, 0, src)?;
    let ignore_empty = match eval_view(&args[1], src) {
        Value::Error(e) => return Err(e),
        v => v.as_bool().ok_or(ErrorKind::Value)?,
    };
    let mut out = String::new();
    let mut any = false;
    for a in &args[2..] {
        for_each_text(a, src, &mut |s| {
            // The whole point of `ignore_empty`: when false, an empty item
            // still produces a delimiter, so blanks are visible as gaps.
            if s.is_empty() && ignore_empty {
                return Ok(());
            }
            if any {
                out.push_str(&delim);
            }
            out.push_str(s);
            any = true;
            cap(&out)
        })?;
    }
    Ok(text_value(&out))
}

/// Stop before a result can grow without bound.
#[inline]
fn cap(out: &str) -> Result<(), ErrorKind> {
    if out.len() > MAX_TEXT_LEN {
        Err(ErrorKind::Value)
    } else {
        Ok(())
    }
}

/// Feed every text item an argument contributes to `f`.
///
/// A range STREAMS: one cell is read, handed over, and dropped before the next
/// is touched. Combined with [`cap`], the peak for `CONCAT(A:A)` over a
/// 200M-row column is the 32k output buffer, not the column.
fn for_each_text<S, F>(arg: &Expr, src: &S, f: &mut F) -> Result<(), ErrorKind>
where
    S: CellSource + ?Sized,
    F: FnMut(&str) -> Result<(), ErrorKind>,
{
    match arg {
        Expr::Range(start, end) => {
            let r1 = (end.row as usize + 1).min(src.row_count().max(1));
            for c in start.col..=end.col {
                for r in start.row as usize..r1 {
                    let v = src.get(CellRef::new(r as u32, c));
                    let s = value_text(v, src)?;
                    f(&s)?;
                }
            }
            Ok(())
        }
        Expr::XRange(sheet, start, end) => {
            let rows = src.row_count_in(sheet).ok_or(ErrorKind::Ref)?;
            let r1 = (end.row as usize + 1).min(rows.max(1));
            for c in start.col..=end.col {
                for r in start.row as usize..r1 {
                    let v = src.get_in(sheet, CellRef::new(r as u32, c));
                    let s = value_text(v, src)?;
                    f(&s)?;
                }
            }
            Ok(())
        }
        other => {
            let s = text_of(other, src)?;
            f(&s)
        }
    }
}

/// Excel VALUE: parse a number out of text, tolerating the decorations a
/// spreadsheet puts on numbers (currency sign, thousands separators, a
/// trailing percent, parenthesised negatives).
fn parse_value(s: &str) -> Option<f64> {
    let t = s.trim();
    if t.is_empty() {
        return None;
    }
    let (t, paren_negative) = match t.strip_prefix('(').and_then(|x| x.strip_suffix(')')) {
        Some(inner) => (inner.trim(), true),
        None => (t, false),
    };
    let (t, percent) = match t.strip_suffix('%') {
        Some(x) => (x.trim_end(), true),
        None => (t, false),
    };
    let mut cleaned = String::with_capacity(t.len());
    for (i, c) in t.chars().enumerate() {
        match c {
            ',' | '$' | '\u{a0}' => {}
            '+' | '-' if i == 0 => cleaned.push(c),
            _ => cleaned.push(c),
        }
    }
    let n: f64 = cleaned.parse().ok()?;
    if !n.is_finite() {
        return None;
    }
    let n = if percent { n / 100.0 } else { n };
    Some(if paren_negative { -n } else { n })
}

/// Intern a text result and wrap it as a [`Value`].
///
/// This is where the "no fresh String per cell" property is bought: interning
/// DEDUPLICATES, so a million-row column of `=UPPER(region)` over three
/// regions retains three strings. When the interner's budget is exhausted the
/// answer is `#VALUE!` — bounded and visible, never an unbounded leak.
#[inline]
fn text_value(s: &str) -> Value {
    match intern_formula_text(s) {
        Some(id) => Value::Text(id),
        None => Value::Error(ErrorKind::Value),
    }
}
