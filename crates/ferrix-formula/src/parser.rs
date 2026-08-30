//! Formula tokenizer and Pratt parser.
//!
//! Grammar (precedence low -> high):
//!   comparison  := concat (("=" | "<>" | "<" | ">" | "<=" | ">=") concat)*
//!   concat      := additive ("&" additive)*
//!   additive    := multiplicative (("+" | "-") multiplicative)*
//!   multiplicative := power (("*" | "/") power)*
//!   power       := unary ("^" unary)*
//!   unary       := ("-" | "+") unary | postfix
//!   postfix     := primary "%"?
//!   primary     := number | string | bool | ref | range | func "(" args ")" | "(" expr ")"

use ferrix_core::CellRef;

#[derive(Clone, PartialEq, Debug)]
pub enum Token {
    Number(f64),
    Text(String),
    Bool(bool),
    /// A cell reference like A1 or $B$2, with absolute-ness flags.
    Ref {
        cell: CellRef,
        abs_col: bool,
        abs_row: bool,
    },
    /// A sheet-qualified reference: `Sheet2!A1` or `'My Sheet'!$B$2`.
    ///
    /// The sheet name is kept verbatim (unquoted, with `''` unescaped) so the
    /// workbook can resolve it case-insensitively at graph-build time.
    SheetRef {
        sheet: String,
        cell: CellRef,
        abs_col: bool,
        abs_row: bool,
    },
    /// A 3-D span across consecutive sheets: `Sheet1:Sheet3!A1`.
    ///
    /// Both endpoint names are kept verbatim, in the order written. Which
    /// sheets lie between them is a question about TAB ORDER, which only the
    /// workbook can answer — the parser deliberately does not guess.
    SheetSpan {
        first: String,
        last: String,
        cell: CellRef,
        abs_col: bool,
        abs_row: bool,
    },
    /// A literal error constant, currently only `#REF!`.
    ///
    /// A deleted column, row or SHEET rewrites the formula TEXT to put
    /// `#REF!` where the reference was (see [`crate::remap`] and
    /// [`crate::names::break_sheet_in_formula`]). Without a token for it, that
    /// rewritten text would fail to parse and the cell would show `#NAME?` —
    /// blaming an unknown name for what is really a broken reference.
    Error(ferrix_core::ErrorKind),
    Ident(String),
    LParen,
    RParen,
    Comma,
    Colon,
    Op(BinOp),
    Percent,
    /// Unary minus is disambiguated by the parser, not the lexer.
    Minus,
    Plus,
    /// The implicit-intersection prefix `@`, as in `@A1:A10`. Excel's
    /// `_xlfn.SINGLE`/`@` operator: it forces a range or array expression to
    /// collapse to a single value under implicit intersection, opting a cell
    /// OUT of the spill behaviour a bare reference would otherwise trigger.
    At,
    /// The spill-range suffix `#`, as in `A1#`. It follows a cell reference and
    /// stands for the WHOLE dynamic array that spilled out of the host at that
    /// cell — resolved against the live spill overlay (#27 P2), not the sheet.
    /// Lexed as its own token (never part of a ref word) so the `$` markers a
    /// following rewrite needs stay attached to the reference, exactly as the
    /// text-editing reference model in [`crate::refscan`] requires.
    Hash,
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum BinOp {
    Add,
    Sub,
    Mul,
    Div,
    Pow,
    Concat,
    Eq,
    Ne,
    Lt,
    Gt,
    Le,
    Ge,
}

impl BinOp {
    fn precedence(self) -> u8 {
        match self {
            BinOp::Eq | BinOp::Ne | BinOp::Lt | BinOp::Gt | BinOp::Le | BinOp::Ge => 1,
            BinOp::Concat => 2,
            BinOp::Add | BinOp::Sub => 3,
            BinOp::Mul | BinOp::Div => 4,
            BinOp::Pow => 5,
        }
    }

    /// Only `^` is right-associative, matching spreadsheet convention.
    fn right_assoc(self) -> bool {
        matches!(self, BinOp::Pow)
    }
}

/// Parsed expression tree.
#[derive(Clone, PartialEq, Debug)]
pub enum Expr {
    Number(f64),
    Text(String),
    Bool(bool),
    Ref(CellRef),
    /// An inclusive rectangular range, normalized so start <= end.
    Range(CellRef, CellRef),
    /// `Sheet2!A1` — a reference into another sheet, by name.
    ///
    /// Kept as a separate variant rather than adding a `sheet: Option<String>`
    /// field to [`Expr::Ref`] so that every existing consumer of a same-sheet
    /// reference keeps compiling and behaving identically; only code that
    /// genuinely wants to be workbook-aware has to grow a new arm.
    XRef(String, CellRef),
    /// `Sheet2!A1:B10` — a range inside another sheet, normalized.
    XRange(String, CellRef, CellRef),
    /// `Sheet1:Sheet3!A1` / `Sheet1:Sheet3!A1:B10` — a 3-D reference: the same
    /// rectangle taken from every sheet in a consecutive run of tabs.
    ///
    /// The rectangle is stored normalized, exactly as [`Expr::XRange`] stores
    /// it, and a single-cell 3-D reference is the degenerate 1x1 case. One
    /// variant rather than two keeps the number of match sites that have to
    /// learn about 3-D down to what is genuinely unavoidable.
    ///
    /// The two sheet NAMES are the endpoints of a tab-order run; which sheets
    /// lie between them is a question only the workbook can answer, so it is
    /// resolved at graph-build and evaluation time rather than at parse time.
    /// Resolving it in the parser would freeze the run against the tab order
    /// as it stood when the formula was typed, so inserting a sheet between
    /// the endpoints would silently fail to be included.
    X3D(String, String, CellRef, CellRef),
    /// A literal error constant, e.g. the `#REF!` a delete leaves behind.
    Error(ferrix_core::ErrorKind),
    /// `@expr` — the implicit-intersection prefix. Forces its operand to
    /// collapse to a single scalar value (Excel's `@`/`SINGLE`), so a cell
    /// holding `=@A1:A10` intersects rather than spilling. It is a no-op on an
    /// operand that is already scalar, which is why it can wrap any `Expr`.
    Intersect(Box<Expr>),
    /// `A1#` — the spill-range operator. Stands for the entire dynamic array
    /// that spilled from the host formula at this cell. It resolves against the
    /// live spill overlay (#27 P2) at evaluation time, NOT the stored sheet:
    /// `A1#` is `#REF!` when A1 is not a spilling host, and the whole
    /// `rows x cols` array when it is. The `CellRef` is the anchor (the host);
    /// the `$` anchoring the user wrote survives in the formula TEXT, which is
    /// all the rewrite paths in [`crate::refscan`] ever consult.
    SpillRange(CellRef),
    Unary(UnOp, Box<Expr>),
    Binary(BinOp, Box<Expr>, Box<Expr>),
    Call(String, Vec<Expr>),
    /// A lexical variable reference — a name bound by an enclosing `LET` or
    /// `LAMBDA`, resolved from the evaluator's scope stack rather than the
    /// workbook name table.
    ///
    /// The parser only ever emits this for an identifier that is in scope at
    /// that point in the text (see the `scope` stack in [`Parser`]); a bare
    /// word that is NOT a bound param or LET name still falls through to the
    /// workbook resolver and, failing that, `#NAME?`. So `Var` never escapes
    /// the LET/LAMBDA whose binding introduced it, and the reference-rewriting
    /// passes (`refscan`/`refedit`/`remap`) — which work on formula TEXT and
    /// leave bare identifiers untouched — need no new arm.
    Var(String),
    /// `LET(name1, value1, ..., body)` — lexical bindings. Each `(name, value)`
    /// pair binds `name` to `value` (evaluated in the scope built by the
    /// bindings BEFORE it, so later bindings can reference earlier ones), then
    /// `body` is evaluated in the fully-extended scope. A special form, not an
    /// `Expr::Call`, because its arguments are names, not values.
    Let(Vec<(String, Expr)>, Box<Expr>),
    /// `LAMBDA(param1, ..., body)` — a first-class function value. Evaluating it
    /// captures the enclosing scope; applying it (via [`Expr::Apply`]) binds the
    /// arguments to the parameters over that captured scope. A special form for
    /// the same reason as `Let`: its leading arguments are parameter *names*.
    Lambda(Vec<String>, Box<Expr>),
    /// `callee(arg1, ...)` — application of a lambda-valued expression. This is
    /// how `LAMBDA(x, x+1)(5)` and a LET-bound lambda called by name are
    /// invoked. The callee is any expression that evaluates to a lambda (an
    /// `Expr::Lambda`, or an `Expr::Var` bound to one); anything else is
    /// `#VALUE!`. Kept separate from `Expr::Call` (whose head is a static
    /// builtin NAME) because the head here is a runtime value.
    Apply(Box<Expr>, Vec<Expr>),
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum UnOp {
    Neg,
    /// Trailing `%` divides by 100.
    Percent,
}

#[derive(Debug, thiserror::Error, PartialEq)]
pub enum ParseError {
    #[error("unexpected character {0:?} at byte {1}")]
    BadChar(char, usize),
    #[error("unterminated string literal")]
    UnterminatedString,
    #[error("unexpected end of formula")]
    UnexpectedEnd,
    #[error("expected {0}, found {1:?}")]
    Expected(&'static str, String),
    #[error("invalid cell reference {0:?}")]
    BadRef(String),
    #[error("unterminated quoted sheet name")]
    UnterminatedSheetName,
    #[error("expected a cell reference after {0:?}!")]
    BadSheetRef(String),
    #[error("trailing input after expression: {0:?}")]
    Trailing(String),
    /// A bare word that is neither a function call, a cell reference, nor a
    /// name the resolver knows. This is what `#NAME?` means.
    #[error("unknown name {0:?}")]
    UnknownName(String),
    /// The expression nests deeper than [`MAX_PARSE_DEPTH`].
    ///
    /// The parser is recursive descent, so nesting depth is stack depth. A
    /// formula of 100,000 nested parentheses is a dozen kilobytes of text and
    /// would otherwise overflow the stack — which on a release build with
    /// `panic = "unwind"` is still an abort, taking the user's unsaved edits
    /// with it. A refusal is not a degradation here; it is the only outcome
    /// that keeps the process alive.
    #[error("formula nests {0} levels deep, over the {MAX_PARSE_DEPTH} limit")]
    TooDeep(usize),
    /// `LAMBDA` needs at least a body: `LAMBDA(body)` or
    /// `LAMBDA(p1, ..., body)`. Fewer than one argument is malformed.
    #[error("LAMBDA needs at least a body argument")]
    LambdaArity,
    /// Every argument of `LAMBDA` before the body must be a bare identifier
    /// (a parameter name), not an expression. `LAMBDA(1, x)` is malformed.
    #[error("LAMBDA parameter {0} must be a bare name")]
    LambdaParam(usize),
    /// `LET` needs an odd argument count of at least three:
    /// `LET(name, value, body)`. An even count leaves a name without a value.
    #[error("LET needs name/value pairs then a body (odd count >= 3)")]
    LetArity,
    /// Every LET binding target must be a bare identifier, not an expression.
    /// `LET(1, 2, body)` is malformed.
    #[error("LET binding name #{0} must be a bare name")]
    LetName(usize),
}

/// Deepest expression nesting the parser will build.
///
/// Twice Excel's own 64-level nested-function limit, so nothing a human or
/// another spreadsheet produces is refused.
///
/// **The number is measured, not picked.** Each level costs three frames
/// (`parse_expr` -> `parse_expr_inner` -> `parse_prefix`), and an unoptimized
/// debug build makes them far fatter than a release build. Binary-searching
/// the smallest thread stack that survives a parse at depth 127:
///
/// | build   | stack needed at depth 127 |
/// |---------|---------------------------|
/// | debug   | > 512 KB, <= 768 KB       |
/// | release | > 128 KB, <= 256 KB       |
///
/// Against Rust's 2 MB default for a spawned thread — where import runs —
/// that is roughly a 2.5x margin in the *worst* (debug) case, and much more
/// on the 8 MB main thread. A first attempt at 256 overflowed the small
/// stack the test below uses, which is exactly why that test spawns its own
/// thread instead of trusting whatever the harness provides.
pub const MAX_PARSE_DEPTH: usize = 128;

/// Render a sheet name as it must appear inside a formula.
///
/// Bare names are written as-is; anything containing a character that would
/// confuse the tokenizer (space, `!`, `'`, punctuation) is single-quoted with
/// interior quotes doubled — the same convention Excel uses.
pub fn quote_sheet_name(name: &str) -> String {
    let plain = !name.is_empty()
        && !name.as_bytes()[0].is_ascii_digit()
        && name
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b == b'_' || b == b'.');
    if plain {
        name.to_string()
    } else {
        format!("'{}'", name.replace('\'', "''"))
    }
}

/// Tokenize a formula body (without the leading `=`).
pub fn tokenize(input: &str) -> Result<Vec<Token>, ParseError> {
    let bytes = input.as_bytes();
    let mut out = Vec::new();
    let mut i = 0usize;

    while i < bytes.len() {
        let c = bytes[i];
        match c {
            b' ' | b'\t' | b'\n' | b'\r' => {
                i += 1;
            }
            b'(' => {
                out.push(Token::LParen);
                i += 1;
            }
            b')' => {
                out.push(Token::RParen);
                i += 1;
            }
            b',' => {
                out.push(Token::Comma);
                i += 1;
            }
            b':' => {
                out.push(Token::Colon);
                i += 1;
            }
            b'+' => {
                out.push(Token::Plus);
                i += 1;
            }
            b'-' => {
                out.push(Token::Minus);
                i += 1;
            }
            b'*' => {
                out.push(Token::Op(BinOp::Mul));
                i += 1;
            }
            b'/' => {
                out.push(Token::Op(BinOp::Div));
                i += 1;
            }
            b'^' => {
                out.push(Token::Op(BinOp::Pow));
                i += 1;
            }
            b'&' => {
                out.push(Token::Op(BinOp::Concat));
                i += 1;
            }
            b'%' => {
                out.push(Token::Percent);
                i += 1;
            }
            b'=' => {
                out.push(Token::Op(BinOp::Eq));
                i += 1;
            }
            b'<' => {
                if i + 1 < bytes.len() && bytes[i + 1] == b'>' {
                    out.push(Token::Op(BinOp::Ne));
                    i += 2;
                } else if i + 1 < bytes.len() && bytes[i + 1] == b'=' {
                    out.push(Token::Op(BinOp::Le));
                    i += 2;
                } else {
                    out.push(Token::Op(BinOp::Lt));
                    i += 1;
                }
            }
            b'>' => {
                if i + 1 < bytes.len() && bytes[i + 1] == b'=' {
                    out.push(Token::Op(BinOp::Ge));
                    i += 2;
                } else {
                    out.push(Token::Op(BinOp::Gt));
                    i += 1;
                }
            }
            b'"' => {
                // String literal with "" escaping.
                let mut s = String::new();
                i += 1;
                loop {
                    if i >= bytes.len() {
                        return Err(ParseError::UnterminatedString);
                    }
                    if bytes[i] == b'"' {
                        if i + 1 < bytes.len() && bytes[i + 1] == b'"' {
                            s.push('"');
                            i += 2;
                            continue;
                        }
                        i += 1;
                        break;
                    }
                    // Copy the full UTF-8 char, not just one byte.
                    let ch_len = utf8_len(bytes[i]);
                    s.push_str(&input[i..i + ch_len]);
                    i += ch_len;
                }
                out.push(Token::Text(s));
            }
            b'0'..=b'9' | b'.' => {
                let start = i;
                while i < bytes.len() && (bytes[i].is_ascii_digit() || bytes[i] == b'.') {
                    i += 1;
                }
                // Scientific notation: 1e5, 1.2E-3
                if i < bytes.len() && (bytes[i] == b'e' || bytes[i] == b'E') {
                    let save = i;
                    i += 1;
                    if i < bytes.len() && (bytes[i] == b'+' || bytes[i] == b'-') {
                        i += 1;
                    }
                    if i < bytes.len() && bytes[i].is_ascii_digit() {
                        while i < bytes.len() && bytes[i].is_ascii_digit() {
                            i += 1;
                        }
                    } else {
                        i = save; // not an exponent after all
                    }
                }
                let text = &input[start..i];
                let n = text
                    .parse::<f64>()
                    .map_err(|_| ParseError::BadChar('.', start))?;
                out.push(Token::Number(n));
            }
            b'#' => {
                // `#REF!` — the only error constant a Ferrix rewrite writes
                // into formula text. Every other error is stored as a cell
                // VALUE rather than as formula source, so none of them ever
                // reach the tokenizer. A bare `#` that is NOT `#REF!` is the
                // spill-range suffix (`A1#`); it is lexed as its own token so
                // the reference before it stays a plain, rewritable ref word.
                const REF: &str = "#REF!";
                if input.len() >= i + REF.len() && input[i..i + REF.len()].eq_ignore_ascii_case(REF)
                {
                    out.push(Token::Error(ferrix_core::ErrorKind::Ref));
                    i += REF.len();
                } else {
                    out.push(Token::Hash);
                    i += 1;
                }
            }
            b'@' => {
                // The implicit-intersection prefix. Only meaningful before a
                // reference/range/array expression; the parser decides.
                out.push(Token::At);
                i += 1;
            }
            b'\'' => {
                // A quoted sheet name: 'My Sheet'!A1, with '' as an escaped
                // quote. Only ever valid immediately before `!`.
                let mut name = String::new();
                i += 1;
                loop {
                    if i >= bytes.len() {
                        return Err(ParseError::UnterminatedSheetName);
                    }
                    if bytes[i] == b'\'' {
                        if bytes.get(i + 1) == Some(&b'\'') {
                            name.push('\'');
                            i += 2;
                            continue;
                        }
                        i += 1;
                        break;
                    }
                    let ch_len = utf8_len(bytes[i]);
                    name.push_str(&input[i..i + ch_len]);
                    i += ch_len;
                }
                let (tok, next) = lex_sheet_qualified(input, bytes, i, name)?;
                out.push(tok);
                i = next;
            }
            b'$' | b'A'..=b'Z' | b'a'..=b'z' | b'_' => {
                let start = i;
                while i < bytes.len()
                    && (bytes[i].is_ascii_alphanumeric()
                        || bytes[i] == b'$'
                        || bytes[i] == b'_'
                        || bytes[i] == b'.')
                {
                    i += 1;
                }
                let word = &input[start..i];
                let upper = word.to_ascii_uppercase();
                // `Name!` is a sheet qualifier, and it wins over every other
                // reading of the word — `TRUE!A1` names a sheet called TRUE.
                // `Name:Other!` is the 3-D form of the same thing, and only
                // counts when a `!` really follows the second name; a plain
                // `A1:B4` range never reaches here.
                if bytes.get(i) == Some(&b'!')
                    || (parse_ref_token(word).is_none()
                        && sheet_span_ahead(input, bytes, i).is_some())
                {
                    let (tok, next) = lex_sheet_qualified(input, bytes, i, word.to_string())?;
                    out.push(tok);
                    i = next;
                    continue;
                }
                // A word immediately followed by `(` is a function call, never
                // a cell reference. Without this, LOG10( lexes as ref LOG10.
                let mut j = i;
                while j < bytes.len() && (bytes[j] == b' ' || bytes[j] == b'\t') {
                    j += 1;
                }
                let is_call = bytes.get(j) == Some(&b'(');
                if upper == "TRUE" && !is_call {
                    out.push(Token::Bool(true));
                } else if upper == "FALSE" && !is_call {
                    out.push(Token::Bool(false));
                } else if !is_call {
                    if let Some(tok) = parse_ref_token(word) {
                        out.push(tok);
                    } else {
                        out.push(Token::Ident(upper));
                    }
                } else {
                    out.push(Token::Ident(upper));
                }
            }
            _ => {
                let ch = input[i..].chars().next().unwrap_or('?');
                return Err(ParseError::BadChar(ch, i));
            }
        }
    }
    Ok(out)
}

#[inline]
fn utf8_len(b: u8) -> usize {
    if b < 0x80 {
        1
    } else if b >> 5 == 0b110 {
        2
    } else if b >> 4 == 0b1110 {
        3
    } else {
        4
    }
}

/// Recognize `A1`, `$A$1`, `$A1`, `A$1`. Returns None for anything else so the
/// lexer can fall back to treating the word as a function name.
fn parse_ref_token(word: &str) -> Option<Token> {
    let bytes = word.as_bytes();
    let mut i = 0;
    let abs_col = bytes.get(i) == Some(&b'$');
    if abs_col {
        i += 1;
    }
    let letter_start = i;
    while i < bytes.len() && bytes[i].is_ascii_alphabetic() {
        i += 1;
    }
    if i == letter_start {
        return None;
    }
    let letters = &word[letter_start..i];
    // Excel's limit is XFD (3 letters); more than that is a name, not a ref.
    if letters.len() > 3 {
        return None;
    }
    let abs_row = bytes.get(i) == Some(&b'$');
    if abs_row {
        i += 1;
    }
    let digit_start = i;
    while i < bytes.len() && bytes[i].is_ascii_digit() {
        i += 1;
    }
    if i == digit_start || i != bytes.len() {
        return None;
    }
    let digits = &word[digit_start..i];
    let cell = CellRef::from_a1(&format!("{letters}{digits}"))?;
    Some(Token::Ref {
        cell,
        abs_col,
        abs_row,
    })
}

/// Finish lexing a sheet-qualified reference.
///
/// `at` points at the byte that should be `!` — or at the `:` of a 3-D span
/// like `Sheet1:Sheet3!A1`. `name` is the already-decoded first sheet name.
/// Returns the token plus the index just past the reference.
fn lex_sheet_qualified(
    input: &str,
    bytes: &[u8],
    at: usize,
    name: String,
) -> Result<(Token, usize), ParseError> {
    // `Sheet1:Sheet3!` — the second endpoint of a 3-D span.
    let (second, at) = match sheet_span_ahead(input, bytes, at) {
        Some((s, bang)) => (Some(s), bang),
        None => (None, at),
    };
    if bytes.get(at) != Some(&b'!') {
        return Err(ParseError::BadSheetRef(name));
    }
    let mut i = at + 1;
    let ref_start = i;
    while i < bytes.len() && (bytes[i].is_ascii_alphanumeric() || bytes[i] == b'$') {
        i += 1;
    }
    let word = &input[ref_start..i];
    match parse_ref_token(word) {
        Some(Token::Ref {
            cell,
            abs_col,
            abs_row,
        }) => {
            let tok = match second {
                Some(last) => Token::SheetSpan {
                    first: name,
                    last,
                    cell,
                    abs_col,
                    abs_row,
                },
                None => Token::SheetRef {
                    sheet: name,
                    cell,
                    abs_col,
                    abs_row,
                },
            };
            Ok((tok, i))
        }
        _ => Err(ParseError::BadSheetRef(name)),
    }
}

/// At `at`, read the `:Sheet3` half of a 3-D span, returning the decoded
/// second sheet name and the index of the `!` that must follow it.
///
/// `None` unless the text really is `:<name>!`, so `A1:B4` and
/// `Sheet1!A1:Sheet1!B4` are left entirely alone — the caller only reaches
/// here for a word that is not itself a cell reference.
fn sheet_span_ahead(input: &str, bytes: &[u8], at: usize) -> Option<(String, usize)> {
    if bytes.get(at) != Some(&b':') {
        return None;
    }
    let mut i = at + 1;
    let name = if bytes.get(i) == Some(&b'\'') {
        let mut name = String::new();
        i += 1;
        loop {
            if i >= bytes.len() {
                return None;
            }
            if bytes[i] == b'\'' {
                if bytes.get(i + 1) == Some(&b'\'') {
                    name.push('\'');
                    i += 2;
                    continue;
                }
                i += 1;
                break;
            }
            let ch_len = utf8_len(bytes[i]);
            name.push_str(&input[i..i + ch_len]);
            i += ch_len;
        }
        name
    } else {
        let start = i;
        while i < bytes.len()
            && (bytes[i].is_ascii_alphanumeric()
                || bytes[i] == b'$'
                || bytes[i] == b'_'
                || bytes[i] == b'.')
        {
            i += 1;
        }
        if i == start {
            return None;
        }
        input[start..i].to_string()
    };
    if bytes.get(i) != Some(&b'!') {
        return None;
    }
    Some((name, i))
}

/// Parse a formula. Accepts an optional leading `=`.
pub fn parse(input: &str) -> Result<Expr, ParseError> {
    parse_with_names(input, &|_| None)
}

/// Parse a formula, resolving bare identifiers through a name table.
///
/// `resolve` is handed the identifier (upper-cased by the tokenizer; name
/// lookup is case-insensitive) and returns the expression the name stands for.
/// Resolution happens HERE, at parse time, so `SUM(Sales)` and
/// `SUM(Sheet1!$B$2:$B$1000)` produce identical trees — which is what keeps
/// the columnar aggregation fast path in [`crate::eval`] firing, and keeps a
/// name over a 200M-row range exactly as cheap as the explicit range. Carrying
/// an `Expr::Name` through to evaluation instead would force the dependency
/// graph and every fast-path match to learn about names.
///
/// A word the resolver does not know is [`ParseError::UnknownName`], which
/// callers surface as `#NAME?`.
pub fn parse_with_names(
    input: &str,
    resolve: &dyn Fn(&str) -> Option<Expr>,
) -> Result<Expr, ParseError> {
    let body = input.strip_prefix('=').unwrap_or(input);
    let tokens = tokenize(body)?;
    let mut p = Parser {
        tokens,
        pos: 0,
        depth: 0,
        resolve,
        scope: Vec::new(),
    };
    let expr = p.parse_expr(0)?;
    if p.pos < p.tokens.len() {
        return Err(ParseError::Trailing(format!("{:?}", p.tokens[p.pos])));
    }
    Ok(expr)
}

struct Parser<'a> {
    tokens: Vec<Token>,
    pos: usize,
    /// Current recursion depth, bounded by [`MAX_PARSE_DEPTH`].
    ///
    /// Tracked on the struct rather than passed as an argument so that every
    /// mutually recursive entry point shares one counter — `parse_expr` and
    /// `parse_prefix` call each other, and two independent counters would
    /// each stay under the limit while the stack went twice as deep.
    depth: usize,
    /// Name table lookup. [`parse`] passes one that knows nothing, so a
    /// workbook with no names behaves exactly as it did before names existed.
    resolve: &'a dyn Fn(&str) -> Option<Expr>,
    /// Lexical names in scope at the current point, innermost last. A `LET`
    /// binding or `LAMBDA` parameter pushes its name here for the duration of
    /// the sub-expression it governs and pops it on the way out, so a bare
    /// identifier can be classified as an in-scope variable (`Expr::Var`)
    /// versus a workbook name at PARSE time. This is what lets `x` inside
    /// `LET(x, 1, x + 1)` become a variable reference instead of a `#NAME?`,
    /// without any name ever leaking out of the form that bound it.
    scope: Vec<String>,
}

impl Parser<'_> {
    fn peek(&self) -> Option<&Token> {
        self.tokens.get(self.pos)
    }

    fn next(&mut self) -> Option<Token> {
        let t = self.tokens.get(self.pos).cloned();
        if t.is_some() {
            self.pos += 1;
        }
        t
    }

    fn expect(&mut self, want: &Token, what: &'static str) -> Result<(), ParseError> {
        match self.next() {
            Some(ref t) if t == want => Ok(()),
            Some(t) => Err(ParseError::Expected(what, format!("{t:?}"))),
            None => Err(ParseError::UnexpectedEnd),
        }
    }

    /// Pratt loop: parse a prefix, then absorb operators of >= min_prec.
    ///
    /// Depth is counted HERE, around the whole recursive step, and restored
    /// on the way out — including on the error path, so a refusal deep in one
    /// argument does not leave the counter poisoned for its siblings.
    fn parse_expr(&mut self, min_prec: u8) -> Result<Expr, ParseError> {
        self.depth += 1;
        if self.depth > MAX_PARSE_DEPTH {
            self.depth -= 1;
            return Err(ParseError::TooDeep(MAX_PARSE_DEPTH + 1));
        }
        let out = self.parse_expr_inner(min_prec);
        self.depth -= 1;
        out
    }

    fn parse_expr_inner(&mut self, min_prec: u8) -> Result<Expr, ParseError> {
        let mut lhs = self.parse_prefix()?;
        // Operators this loop absorbs into a single left-nested chain. Each one
        // adds a level to the AST WITHOUT recursing, so `parse_expr`'s descent
        // guard never sees it. We therefore charge each absorbed operator
        // against the same depth budget and refund the whole run on the way
        // out — otherwise a flat chain like `=1+1+1+...+1` builds an
        // arbitrarily deep AST at recursion depth ~2, and a later unguarded
        // recursive walk (eval_view, or depgraph's collect_precedents on
        // workbook LOAD) overflows the stack and ABORTS the process, taking
        // unsaved edits with it. Refuse it here, at parse time, before any
        // such AST can escape the parser.
        let mut absorbed = 0usize;
        let result = loop {
            let op = match self.peek() {
                Some(Token::Op(op)) => *op,
                Some(Token::Plus) => BinOp::Add,
                Some(Token::Minus) => BinOp::Sub,
                _ => break Ok(lhs),
            };
            let prec = op.precedence();
            if prec < min_prec {
                break Ok(lhs);
            }
            self.pos += 1;
            self.depth += 1;
            absorbed += 1;
            if self.depth > MAX_PARSE_DEPTH {
                break Err(ParseError::TooDeep(MAX_PARSE_DEPTH + 1));
            }
            let next_min = if op.right_assoc() { prec } else { prec + 1 };
            match self.parse_expr(next_min) {
                Ok(rhs) => lhs = Expr::Binary(op, Box::new(lhs), Box::new(rhs)),
                Err(e) => break Err(e),
            }
        };
        // Refund the whole absorbed run, on success and on error alike, so a
        // sibling expression starts from the same depth this one did.
        self.depth -= absorbed;
        result
    }

    fn parse_prefix(&mut self) -> Result<Expr, ParseError> {
        let tok = self.next().ok_or(ParseError::UnexpectedEnd)?;
        let expr = match tok {
            Token::Minus => {
                // Unary minus binds tighter than */ but looser than ^.
                let operand = self.parse_expr(5)?;
                Expr::Unary(UnOp::Neg, Box::new(operand))
            }
            Token::Plus => self.parse_expr(5)?,
            Token::At => {
                // Implicit-intersection prefix. It binds to the reference or
                // array expression that follows exactly as unary minus binds to
                // its operand — `@A1:A10` intersects the range, `@SUM(A1:A3)`
                // wraps the call — so it shares unary minus's precedence. It is
                // idempotent and harmless on an already-scalar operand.
                let operand = self.parse_expr(5)?;
                Expr::Intersect(Box::new(operand))
            }
            Token::Number(n) => Expr::Number(n),
            Token::Text(s) => Expr::Text(s),
            Token::Bool(b) => Expr::Bool(b),
            Token::Error(e) => {
                // A `#` right after an error constant is the spill-range suffix
                // on a BROKEN anchor: a structural delete rewrites `A1#`'s
                // reference to `#REF!` and leaves the `#` behind, so `#REF!#` is
                // the text that results. Absorb the `#` and stay `#REF!` — a
                // spill-range whose host reference no longer exists is itself
                // `#REF!`. Without this the rewritten text would fail to parse
                // and show `#NAME?`, blaming an unknown name for a broken ref.
                if matches!(self.peek(), Some(Token::Hash)) {
                    self.pos += 1;
                }
                Expr::Error(e)
            }
            Token::Ref { cell, .. } => {
                // A colon here means this is a range.
                if matches!(self.peek(), Some(Token::Colon)) {
                    self.pos += 1;
                    match self.next() {
                        Some(Token::Ref { cell: end, .. }) => {
                            let (a, b) = normalize_range(cell, end);
                            Expr::Range(a, b)
                        }
                        Some(t) => {
                            return Err(ParseError::Expected("cell reference", format!("{t:?}")))
                        }
                        None => return Err(ParseError::UnexpectedEnd),
                    }
                } else if matches!(self.peek(), Some(Token::Hash)) {
                    // `A1#` — the spill-range suffix. The `#` binds to the
                    // single reference it follows and turns it into the whole
                    // spilled array anchored at that host. A range endpoint
                    // (`A1:B2`) is handled above and never reaches here, so a
                    // spill-range is always rooted at exactly one cell.
                    self.pos += 1;
                    Expr::SpillRange(cell)
                } else {
                    Expr::Ref(cell)
                }
            }
            Token::SheetRef { sheet, cell, .. } => {
                // `Sheet2!A1:B4` — a colon continues the range. The right-hand
                // corner may be bare (`Sheet2!A1:B4`) or requalified with the
                // SAME sheet (`Sheet2!A1:Sheet2!B4`); a different sheet on the
                // far side is a 3-D reference, which Ferrix does not support.
                if matches!(self.peek(), Some(Token::Colon)) {
                    self.pos += 1;
                    match self.next() {
                        Some(Token::Ref { cell: end, .. }) => {
                            let (a, b) = normalize_range(cell, end);
                            Expr::XRange(sheet, a, b)
                        }
                        Some(Token::SheetRef {
                            sheet: s2,
                            cell: end,
                            ..
                        }) if s2.eq_ignore_ascii_case(&sheet) => {
                            let (a, b) = normalize_range(cell, end);
                            Expr::XRange(sheet, a, b)
                        }
                        Some(t) => {
                            return Err(ParseError::Expected("cell reference", format!("{t:?}")))
                        }
                        None => return Err(ParseError::UnexpectedEnd),
                    }
                } else {
                    Expr::XRef(sheet, cell)
                }
            }
            Token::SheetSpan {
                first, last, cell, ..
            } => {
                // `Sheet1:Sheet3!A1:B4` — the rectangle may be a range, the
                // same way `Sheet2!A1:B4` may. The far corner is bare: a
                // second qualifier inside a 3-D rectangle is not a thing.
                if matches!(self.peek(), Some(Token::Colon)) {
                    self.pos += 1;
                    match self.next() {
                        Some(Token::Ref { cell: end, .. }) => {
                            let (a, b) = normalize_range(cell, end);
                            Expr::X3D(first, last, a, b)
                        }
                        Some(t) => {
                            return Err(ParseError::Expected("cell reference", format!("{t:?}")))
                        }
                        None => return Err(ParseError::UnexpectedEnd),
                    }
                } else {
                    // A single-cell 3-D reference is the 1x1 rectangle.
                    Expr::X3D(first, last, cell, cell)
                }
            }
            Token::Ident(name) => self.parse_ident_prefix(name)?,
            Token::LParen => {
                let inner = self.parse_expr(0)?;
                self.expect(&Token::RParen, ")")?;
                inner
            }
            t => return Err(ParseError::Expected("a value", format!("{t:?}"))),
        };
        let expr = self.parse_postfix(expr)?;
        // Postfix percent.
        if matches!(self.peek(), Some(Token::Percent)) {
            self.pos += 1;
            return Ok(Expr::Unary(UnOp::Percent, Box::new(expr)));
        }
        Ok(expr)
    }

    /// Handle an identifier prefix: LET/LAMBDA special forms, application of an
    /// in-scope lambda variable, an ordinary builtin call, a bare lexical
    /// variable, or a workbook name. Split out of [`Self::parse_prefix`] so its
    /// locals do not inflate that function's stack frame — `parse_prefix` sits
    /// on the parser's recursion path, and a fat frame there is what turns a
    /// deeply-nested formula into a stack overflow rather than a `TooDeep`
    /// refusal (see `pathological_nesting_errors_instead_of_blowing_the_stack`).
    #[inline(never)]
    fn parse_ident_prefix(&mut self, name: String) -> Result<Expr, ParseError> {
        let has_paren = matches!(self.peek(), Some(Token::LParen));
        if has_paren && name.eq_ignore_ascii_case("LET") {
            // LET and LAMBDA are SPECIAL FORMS: their leading arguments are
            // binding/parameter NAMES, not values, so they cannot go through the
            // ordinary comma-separated argument loop (which would try to resolve
            // each name as an expression and fail). They are intercepted before
            // the generic call path, and parse their own bindings while pushing
            // names onto the lexical scope stack.
            self.parse_let()
        } else if has_paren && name.eq_ignore_ascii_case("LAMBDA") {
            self.parse_lambda()
        } else if has_paren {
            // A `(` makes this either an application of an in-scope lambda
            // variable, or an ordinary builtin call. An in-scope name is a
            // variable — invoking it is `Apply(Var, args)`; any other name is a
            // builtin `Call`.
            let args = self.parse_call_args()?;
            if self.in_scope(&name) {
                Ok(Expr::Apply(Box::new(Expr::Var(name)), args))
            } else {
                Ok(Expr::Call(name, args))
            }
        } else if self.in_scope(&name) {
            // A bare in-scope name is a lexical variable reference, resolved from
            // the evaluator's scope stack, never the workbook name table.
            Ok(Expr::Var(name))
        } else {
            // Not in scope and no `(`: a defined name, and the workbook resolver
            // gets the first look. Falling through to UnknownName (rendered
            // `#NAME?`) only AFTER the table has been consulted is what
            // "resolution happens in the parser" means.
            match (self.resolve)(&name) {
                Some(expr) => Ok(expr),
                None => Err(ParseError::UnknownName(name)),
            }
        }
    }

    /// Postfix application: an in-place `LAMBDA(...)(...)` invocation. The callee
    /// expression (a `LAMBDA(...)`, or a chained application) is immediately
    /// applied to a following argument list. Only expressions that can evaluate
    /// to a lambda are eligible — a `Ref`/`Range`/number followed by `(` is
    /// still the syntax error it always was — so this is gated on the prefix
    /// being a `Lambda` or `Apply` (chained application). Split out of
    /// [`Self::parse_prefix`] to keep that function's frame small on the
    /// recursion path.
    #[inline(never)]
    fn parse_postfix(&mut self, mut expr: Expr) -> Result<Expr, ParseError> {
        while matches!(expr, Expr::Lambda(..) | Expr::Apply(..))
            && matches!(self.peek(), Some(Token::LParen))
        {
            let args = self.parse_call_args()?;
            expr = Expr::Apply(Box::new(expr), args);
        }
        Ok(expr)
    }

    /// Is `name` a lexical variable in scope at the current point? Names are
    /// upper-cased by the tokenizer and matched case-insensitively, so scope
    /// membership is exact against what a later `Expr::Var` will look up.
    fn in_scope(&self, name: &str) -> bool {
        self.scope.iter().any(|n| n.eq_ignore_ascii_case(name))
    }

    /// Parse a `(`-led, comma-separated argument list into expressions. Shared
    /// by ordinary calls and lambda applications; the leading `(` is consumed
    /// here and the matching `)` closes the list. An empty list is `()`.
    fn parse_call_args(&mut self) -> Result<Vec<Expr>, ParseError> {
        self.expect(&Token::LParen, "( after function name")?;
        let mut args = Vec::new();
        if matches!(self.peek(), Some(Token::RParen)) {
            self.pos += 1;
            return Ok(args);
        }
        loop {
            args.push(self.parse_expr(0)?);
            match self.next() {
                Some(Token::Comma) => continue,
                Some(Token::RParen) => break,
                Some(t) => return Err(ParseError::Expected(", or )", format!("{t:?}"))),
                None => return Err(ParseError::UnexpectedEnd),
            }
        }
        Ok(args)
    }

    /// Consume a bare identifier used as a binding/parameter NAME. Returns the
    /// upper-cased name (as the tokenizer produced it) so scope membership and
    /// evaluation lookup agree. Any non-identifier token is the caller's error.
    fn expect_name(&mut self) -> Option<String> {
        match self.peek() {
            Some(Token::Ident(n)) => {
                let n = n.clone();
                self.pos += 1;
                Some(n)
            }
            _ => None,
        }
    }

    /// Parse `LET(name1, value1, ..., body)`. The opening `(` has NOT yet been
    /// consumed. Each value is parsed in the scope built by the bindings before
    /// it — so `LET(x, 1, y, x + 1, body)` sees `x` while binding `y` — and the
    /// body sees them all. Every name is pushed onto the lexical scope stack for
    /// the duration of this form and popped on exit, so nothing leaks out.
    fn parse_let(&mut self) -> Result<Expr, ParseError> {
        self.expect(&Token::LParen, "( after LET")?;
        let scope_base = self.scope.len();
        let mut bindings: Vec<(String, Expr)> = Vec::new();
        let result = (|| {
            loop {
                // A name starts either another binding pair or — if the NEXT
                // token after it is `)` rather than `,` — would be malformed;
                // the body is a full expression, never a lone trailing name.
                // We peek: if the current arg parses as a name AND is followed
                // by a comma AND there is more after it, it is a binding target.
                // Simpler and unambiguous: LET always alternates name, value,
                // ..., body. So the body is the LAST argument; every argument at
                // an even index (0,2,...) before the last is a name.
                //
                // Detect the body by lookahead: parse a name; if the token
                // after it is `)`, the "name" was actually the body position
                // and LET is malformed (even arg count). Otherwise expect a
                // value expression.
                let idx = bindings.len();
                let Some(name) = self.expect_name() else {
                    return Err(ParseError::LetName(idx + 1));
                };
                // After a binding name must come a comma then its value.
                match self.next() {
                    Some(Token::Comma) => {}
                    _ => return Err(ParseError::LetArity),
                }
                // Bind the name for the value expressions that follow (later
                // bindings and the body can reference it). Excel evaluates LET
                // bindings top-to-bottom with earlier names visible to later
                // values.
                let value = self.parse_expr(0)?;
                self.scope.push(name.clone());
                bindings.push((name, value));
                // What follows the value is either `,` (more bindings or the
                // body) or `)` — but `)` here means no body, which is malformed.
                match self.peek() {
                    Some(Token::Comma) => {
                        self.pos += 1;
                        // The argument after this comma is the body IFF the one
                        // after THAT is `)`. Rather than deep lookahead, parse
                        // it as the body speculatively only when we can tell it
                        // is last. We detect "last" structurally: try to read a
                        // name followed by a comma (=> another binding);
                        // otherwise it is the body.
                        if self.looks_like_binding_name() {
                            continue;
                        }
                        let body = self.parse_expr(0)?;
                        self.expect(&Token::RParen, ") to close LET")?;
                        return Ok(Expr::Let(std::mem::take(&mut bindings), Box::new(body)));
                    }
                    _ => return Err(ParseError::LetArity),
                }
            }
        })();
        // Pop this form's names no matter how it exited, so a sibling
        // expression never sees a binding that belonged to this LET.
        self.scope.truncate(scope_base);
        result
    }

    /// Lookahead: does the upcoming token stream start a `name ,` binding pair
    /// (as opposed to the body expression)? A binding target is a bare
    /// identifier immediately followed by a comma. Anything else — a number, a
    /// reference, an identifier followed by `(` or an operator — is the body.
    fn looks_like_binding_name(&self) -> bool {
        matches!(self.tokens.get(self.pos), Some(Token::Ident(_)))
            && matches!(self.tokens.get(self.pos + 1), Some(Token::Comma))
    }

    /// Parse `LAMBDA(param1, ..., body)`. The opening `(` has NOT yet been
    /// consumed. Every argument except the last is a parameter name; the last
    /// is the body, parsed with all parameters in scope. Parameters are pushed
    /// onto the lexical scope stack for the body and popped on exit.
    fn parse_lambda(&mut self) -> Result<Expr, ParseError> {
        self.expect(&Token::LParen, "( after LAMBDA")?;
        let scope_base = self.scope.len();
        let mut params: Vec<String> = Vec::new();
        let result = (|| {
            // Consume `name ,` pairs while the next tokens look like a
            // parameter (identifier followed by a comma). The final argument —
            // an identifier NOT followed by a comma, or any non-identifier — is
            // the body.
            while self.looks_like_binding_name() {
                let name = self
                    .expect_name()
                    .ok_or(ParseError::LambdaParam(params.len() + 1))?;
                self.pos += 1; // the comma
                self.scope.push(name.clone());
                params.push(name);
            }
            // Whatever remains is the body. `LAMBDA()` with nothing is arity 0.
            if matches!(self.peek(), Some(Token::RParen)) {
                return Err(ParseError::LambdaArity);
            }
            let body = self.parse_expr(0)?;
            self.expect(&Token::RParen, ") to close LAMBDA")?;
            Ok(Expr::Lambda(std::mem::take(&mut params), Box::new(body)))
        })();
        self.scope.truncate(scope_base);
        result
    }
}

/// Order range corners so start is top-left. `B4:A1` and `A1:B4` are the same
/// range in every spreadsheet.
fn normalize_range(a: CellRef, b: CellRef) -> (CellRef, CellRef) {
    (
        CellRef::new(a.row.min(b.row), a.col.min(b.col)),
        CellRef::new(a.row.max(b.row), a.col.max(b.col)),
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    fn num(n: f64) -> Expr {
        Expr::Number(n)
    }

    // --- pathological nesting depth -------------------------------------

    #[test]
    fn pathological_nesting_errors_instead_of_blowing_the_stack() {
        // THE acceptance criterion. 100,000 nested parens is a 200 KB string
        // -- trivial to put in a cell or an imported formula -- and against a
        // recursive-descent parser with no depth cap it is a stack overflow,
        // which is an ABORT even under `panic = "unwind"`, taking the user's
        // unsaved edits with it.
        //
        // Run on a thread with a deliberately SMALL stack -- 1 MB, half of
        // Rust's default for a spawned thread and an eighth of the main
        // thread's. If MAX_PARSE_DEPTH were ever raised past what the
        // recursion can afford, this overflows here rather than passing by
        // luck on whatever stack the harness happened to provide. That is
        // not hypothetical: the constant started at 256 and this test is
        // what caught it.
        let handle = std::thread::Builder::new()
            .stack_size(1024 * 1024)
            .spawn(|| {
                let deep = format!("{}1{}", "(".repeat(100_000), ")".repeat(100_000));
                parse(&deep)
            })
            .expect("spawn");
        let err = handle
            .join()
            .expect("parsing deeply nested input must not abort the thread")
            .expect_err("100,000 levels of nesting must be refused");
        assert!(
            matches!(err, ParseError::TooDeep(_)),
            "expected TooDeep, got {err:?}"
        );
        // The message has to say what happened, not just that it failed.
        assert!(err.to_string().contains("nests"), "{err}");
    }

    #[test]
    fn deep_function_nesting_is_also_capped() {
        // Parenthesis nesting is not the only route to the same stack: a
        // chain of calls recurses through `parse_prefix` -> `parse_expr` just
        // as hard. Both entry points share one counter, which is why this
        // cannot slip through at twice the depth.
        let deep = format!("{}1{}", "ABS(".repeat(5_000), ")".repeat(5_000));
        let err = parse(&deep).expect_err("5,000 nested calls must be refused");
        assert!(
            matches!(err, ParseError::TooDeep(_)),
            "expected TooDeep, got {err:?}"
        );

        // And unary operators, which recurse without any bracket at all.
        let deep = format!("{}1", "-".repeat(5_000));
        assert!(
            matches!(parse(&deep), Err(ParseError::TooDeep(_))),
            "a chain of unary minus must be capped too"
        );
    }

    #[test]
    fn formulas_a_human_would_actually_write_still_parse() {
        // The control. Without it, the tests above pass against a parser that
        // refuses everything. Excel's own nesting limit is 64 levels, so
        // anything a real spreadsheet contains must be well clear of the cap.
        let realistic = format!("{}A1{}", "IF(A1>0,".repeat(60), ",0)".repeat(60));
        assert!(
            parse(&realistic).is_ok(),
            "60 levels of nesting is under Excel's own 64-level limit and must parse"
        );
        // Right at the boundary, from both sides, so the cap is exactly where
        // it claims to be rather than approximately there.
        let at_limit = format!(
            "{}1{}",
            "(".repeat(MAX_PARSE_DEPTH - 1),
            ")".repeat(MAX_PARSE_DEPTH - 1)
        );
        assert!(
            parse(&at_limit).is_ok(),
            "{} levels must still parse; the cap is {MAX_PARSE_DEPTH}",
            MAX_PARSE_DEPTH - 1
        );
        let over_limit = format!(
            "{}1{}",
            "(".repeat(MAX_PARSE_DEPTH + 1),
            ")".repeat(MAX_PARSE_DEPTH + 1)
        );
        assert!(
            matches!(parse(&over_limit), Err(ParseError::TooDeep(_))),
            "{} levels must be refused",
            MAX_PARSE_DEPTH + 1
        );
    }

    #[test]
    fn the_depth_counter_is_restored_after_an_error() {
        // A counter that leaked on the error path would make each successive
        // argument of a long call cheaper to refuse, so a formula with many
        // sibling arguments would start failing for the wrong reason. Many
        // arguments, each modestly nested, must all parse.
        let arg = format!("{}1{}", "(".repeat(50), ")".repeat(50));
        let wide: Vec<String> = (0..100).map(|_| arg.clone()).collect();
        let formula = format!("=SUM({})", wide.join(","));
        assert!(
            parse(&formula).is_ok(),
            "100 siblings at depth 50 must parse; depth is per-path, not cumulative"
        );
    }

    #[test]
    fn a_flat_operator_chain_is_capped_like_nested_depth() {
        // A left-associative operator chain (`=1+1+1+...`) is absorbed by
        // parse_expr_inner's LOOP, not by recursion, so the descent-only depth
        // guard used to miss it entirely: a ~200k-term chain parsed fine and
        // then overflowed the stack in eval_view (and in depgraph's precedent
        // walk on workbook LOAD) — an uncatchable process abort that takes
        // unsaved edits down with it. The loop now charges each absorbed
        // operator against MAX_PARSE_DEPTH, so the chain is refused at parse
        // time before any pathological AST can escape.
        let chain = format!("={}", vec!["1"; MAX_PARSE_DEPTH + 50].join("+"));
        assert!(
            matches!(parse(&chain), Err(ParseError::TooDeep(_))),
            "a flat + chain past the depth limit must be refused, not left to \
             overflow the eval/depgraph stack"
        );

        // The deepest still-allowed chain must build an AST whose left-nesting
        // stays within the cap, so a downstream recursive walk cannot overflow.
        let ok_chain = format!("={}", vec!["1"; MAX_PARSE_DEPTH - 2].join("+"));
        let e = parse(&ok_chain).expect("a chain under the limit must parse");
        let mut d = 0usize;
        let mut cur = &e;
        while let Expr::Binary(_, l, _) = cur {
            d += 1;
            cur = l;
        }
        assert!(
            d < MAX_PARSE_DEPTH,
            "the allowed AST's left-nesting ({d}) must stay under the cap"
        );
    }

    #[test]
    fn a_realistic_length_sum_chain_still_parses() {
        // The control for the cap above: a chain a human might plausibly write
        // (adding a couple dozen cells) must NOT be refused. Excel-style flat
        // sums of this length are ordinary; only pathological lengths attack.
        let realistic = format!(
            "={}",
            (1..=30)
                .map(|i| format!("A{i}"))
                .collect::<Vec<_>>()
                .join("+")
        );
        assert!(
            parse(&realistic).is_ok(),
            "a 30-term additive chain is an ordinary formula and must parse"
        );
    }

    #[test]
    fn tokenizes_operators() {
        let t = tokenize("1+2*3").unwrap();
        assert_eq!(
            t,
            vec![
                Token::Number(1.0),
                Token::Plus,
                Token::Number(2.0),
                Token::Op(BinOp::Mul),
                Token::Number(3.0)
            ]
        );
    }

    #[test]
    fn tokenizes_comparison_operators() {
        assert_eq!(tokenize("<=").unwrap(), vec![Token::Op(BinOp::Le)]);
        assert_eq!(tokenize(">=").unwrap(), vec![Token::Op(BinOp::Ge)]);
        assert_eq!(tokenize("<>").unwrap(), vec![Token::Op(BinOp::Ne)]);
        assert_eq!(tokenize("<").unwrap(), vec![Token::Op(BinOp::Lt)]);
    }

    #[test]
    fn tokenizes_refs_and_absolutes() {
        let t = tokenize("$A$1").unwrap();
        assert_eq!(
            t,
            vec![Token::Ref {
                cell: CellRef::new(0, 0),
                abs_col: true,
                abs_row: true
            }]
        );
        let t = tokenize("B12").unwrap();
        assert_eq!(
            t,
            vec![Token::Ref {
                cell: CellRef::new(11, 1),
                abs_col: false,
                abs_row: false
            }]
        );
    }

    #[test]
    fn long_names_are_idents_not_refs() {
        // SUMIF has 5 letters and no digits, so it can never be a ref.
        assert_eq!(
            tokenize("SUMIF").unwrap(),
            vec![Token::Ident("SUMIF".into())]
        );
    }

    #[test]
    fn paren_disambiguates_function_from_ref() {
        // Bare LOG10 is genuinely ambiguous and resolves as a cell reference
        // (column LOG, row 10) — Excel does the same. A following `(` makes it
        // unambiguously a function call.
        assert!(matches!(
            tokenize("LOG10").unwrap().as_slice(),
            [Token::Ref { .. }]
        ));
        assert_eq!(
            tokenize("LOG10(").unwrap(),
            vec![Token::Ident("LOG10".into()), Token::LParen]
        );
        assert_eq!(tokenize("TRUE").unwrap(), vec![Token::Bool(true)]);
    }

    #[test]
    fn tokenizes_strings_with_escapes() {
        let t = tokenize(r#""he said ""hi""""#).unwrap();
        assert_eq!(t, vec![Token::Text(r#"he said "hi""#.into())]);
    }

    #[test]
    fn unterminated_string_errors() {
        assert_eq!(tokenize(r#""abc"#), Err(ParseError::UnterminatedString));
    }

    #[test]
    fn scientific_notation() {
        assert_eq!(tokenize("1e5").unwrap(), vec![Token::Number(100000.0)]);
        assert_eq!(tokenize("1.5E-3").unwrap(), vec![Token::Number(0.0015)]);
    }

    #[test]
    fn precedence_multiplication_over_addition() {
        // 1+2*3 must parse as 1+(2*3)
        let e = parse("=1+2*3").unwrap();
        assert_eq!(
            e,
            Expr::Binary(
                BinOp::Add,
                Box::new(num(1.0)),
                Box::new(Expr::Binary(
                    BinOp::Mul,
                    Box::new(num(2.0)),
                    Box::new(num(3.0))
                ))
            )
        );
    }

    #[test]
    fn power_is_right_associative() {
        // 2^3^2 == 2^(3^2) == 512, not (2^3)^2 == 64
        let e = parse("=2^3^2").unwrap();
        assert_eq!(
            e,
            Expr::Binary(
                BinOp::Pow,
                Box::new(num(2.0)),
                Box::new(Expr::Binary(
                    BinOp::Pow,
                    Box::new(num(3.0)),
                    Box::new(num(2.0))
                ))
            )
        );
    }

    #[test]
    fn subtraction_is_left_associative() {
        // 10-3-2 must be (10-3)-2 == 5
        let e = parse("=10-3-2").unwrap();
        assert_eq!(
            e,
            Expr::Binary(
                BinOp::Sub,
                Box::new(Expr::Binary(
                    BinOp::Sub,
                    Box::new(num(10.0)),
                    Box::new(num(3.0))
                )),
                Box::new(num(2.0))
            )
        );
    }

    #[test]
    fn unary_minus() {
        let e = parse("=-5").unwrap();
        assert_eq!(e, Expr::Unary(UnOp::Neg, Box::new(num(5.0))));
        // -2^2 is -(2^2) in Excel semantics for this parser's precedence.
        let e = parse("=-2^2").unwrap();
        assert_eq!(
            e,
            Expr::Unary(
                UnOp::Neg,
                Box::new(Expr::Binary(
                    BinOp::Pow,
                    Box::new(num(2.0)),
                    Box::new(num(2.0))
                ))
            )
        );
    }

    #[test]
    fn parens_override_precedence() {
        let e = parse("=(1+2)*3").unwrap();
        assert_eq!(
            e,
            Expr::Binary(
                BinOp::Mul,
                Box::new(Expr::Binary(
                    BinOp::Add,
                    Box::new(num(1.0)),
                    Box::new(num(2.0))
                )),
                Box::new(num(3.0))
            )
        );
    }

    #[test]
    fn function_calls() {
        let e = parse("=SUM(A1:A10)").unwrap();
        assert_eq!(
            e,
            Expr::Call(
                "SUM".into(),
                vec![Expr::Range(CellRef::new(0, 0), CellRef::new(9, 0))]
            )
        );
        let e = parse("=IF(A1>0,1,2)").unwrap();
        match e {
            Expr::Call(name, args) => {
                assert_eq!(name, "IF");
                assert_eq!(args.len(), 3);
            }
            other => panic!("expected call, got {other:?}"),
        }
    }

    #[test]
    fn zero_arg_function() {
        assert_eq!(parse("=NOW()").unwrap(), Expr::Call("NOW".into(), vec![]));
    }

    #[test]
    fn nested_calls() {
        let e = parse("=SUM(A1:A3,MAX(B1,B2))").unwrap();
        match e {
            Expr::Call(ref n, ref args) if n == "SUM" => {
                assert_eq!(args.len(), 2);
                assert!(matches!(args[1], Expr::Call(_, _)));
            }
            other => panic!("unexpected {other:?}"),
        }
    }

    #[test]
    fn range_normalizes_corners() {
        // Reversed corners must produce the same range.
        let a = parse("=SUM(A1:B4)").unwrap();
        let b = parse("=SUM(B4:A1)").unwrap();
        assert_eq!(a, b);
    }

    #[test]
    fn percent_postfix() {
        assert_eq!(
            parse("=50%").unwrap(),
            Expr::Unary(UnOp::Percent, Box::new(num(50.0)))
        );
    }

    #[test]
    fn concat_operator() {
        let e = parse(r#"="a"&"b""#).unwrap();
        assert_eq!(
            e,
            Expr::Binary(
                BinOp::Concat,
                Box::new(Expr::Text("a".into())),
                Box::new(Expr::Text("b".into()))
            )
        );
    }

    #[test]
    fn comparison_lowest_precedence() {
        // 1+2=3 parses as (1+2)=3
        let e = parse("=1+2=3").unwrap();
        match e {
            Expr::Binary(BinOp::Eq, lhs, _) => {
                assert!(matches!(*lhs, Expr::Binary(BinOp::Add, _, _)));
            }
            other => panic!("unexpected {other:?}"),
        }
    }

    #[test]
    fn rejects_malformed() {
        assert!(parse("=1+").is_err());
        assert!(parse("=(1+2").is_err());
        assert!(parse("=SUM(").is_err());
        assert!(parse("=1 2").is_err());
        assert!(parse("=*5").is_err());
    }

    #[test]
    fn works_without_leading_equals() {
        assert_eq!(parse("1+1").unwrap(), parse("=1+1").unwrap());
    }

    // --- cross-sheet references (issue #15) ---

    #[test]
    fn tokenizes_a_bare_sheet_qualified_ref() {
        assert_eq!(
            tokenize("Sheet2!A1").unwrap(),
            vec![Token::SheetRef {
                sheet: "Sheet2".into(),
                cell: CellRef::new(0, 0),
                abs_col: false,
                abs_row: false
            }]
        );
    }

    #[test]
    fn sheet_qualified_refs_keep_absolute_markers() {
        assert_eq!(
            tokenize("Data!$B$2").unwrap(),
            vec![Token::SheetRef {
                sheet: "Data".into(),
                cell: CellRef::new(1, 1),
                abs_col: true,
                abs_row: true
            }]
        );
    }

    #[test]
    fn parses_a_cross_sheet_reference() {
        assert_eq!(
            parse("=Sheet2!A1").unwrap(),
            Expr::XRef("Sheet2".into(), CellRef::new(0, 0))
        );
        // And it composes like any other value.
        assert_eq!(
            parse("=Sheet2!A1*2").unwrap(),
            Expr::Binary(
                BinOp::Mul,
                Box::new(Expr::XRef("Sheet2".into(), CellRef::new(0, 0))),
                Box::new(num(2.0))
            )
        );
    }

    #[test]
    fn parses_a_cross_sheet_range() {
        assert_eq!(
            parse("=SUM(Sheet2!A1:B4)").unwrap(),
            Expr::Call(
                "SUM".into(),
                vec![Expr::XRange(
                    "Sheet2".into(),
                    CellRef::new(0, 0),
                    CellRef::new(3, 1)
                )]
            )
        );
        // Reversed corners normalize, exactly like a same-sheet range.
        assert_eq!(
            parse("=SUM(Sheet2!B4:A1)").unwrap(),
            parse("=SUM(Sheet2!A1:B4)").unwrap()
        );
    }

    #[test]
    fn a_range_may_requalify_the_same_sheet_on_both_ends() {
        assert_eq!(
            parse("=SUM(Sheet2!A1:Sheet2!B4)").unwrap(),
            parse("=SUM(Sheet2!A1:B4)").unwrap()
        );
    }

    #[test]
    fn a_three_d_range_across_two_sheets_is_rejected() {
        // Sheet1!A1:Sheet2!B4 is a 3-D reference. Ferrix does not support one,
        // and silently treating it as a Sheet1 range would be a wrong answer.
        assert!(parse("=SUM(Sheet1!A1:Sheet2!B4)").is_err());
    }

    #[test]
    fn quoted_sheet_names_may_contain_spaces() {
        assert_eq!(
            parse("='My Sheet'!A1").unwrap(),
            Expr::XRef("My Sheet".into(), CellRef::new(0, 0))
        );
        // A doubled '' is an escaped quote inside the name.
        assert_eq!(
            parse("='Bob''s Data'!B2").unwrap(),
            Expr::XRef("Bob's Data".into(), CellRef::new(1, 1))
        );
    }

    #[test]
    fn an_unterminated_quoted_sheet_name_errors() {
        assert_eq!(
            tokenize("'My Sheet!A1"),
            Err(ParseError::UnterminatedSheetName)
        );
    }

    #[test]
    fn a_sheet_qualifier_with_no_valid_ref_errors() {
        assert!(parse("=Sheet2!").is_err());
        assert!(parse("=Sheet2!ZZZZ").is_err());
        assert!(parse("='My Sheet'!nonsense").is_err());
    }

    #[test]
    fn a_sheet_name_wins_over_bool_and_function_readings() {
        // `TRUE!A1` names a sheet called TRUE, not the boolean.
        assert_eq!(
            parse("=TRUE!A1").unwrap(),
            Expr::XRef("TRUE".into(), CellRef::new(0, 0))
        );
        // A sheet may share a name with a function, too.
        assert_eq!(
            parse("=SUM!A1").unwrap(),
            Expr::XRef("SUM".into(), CellRef::new(0, 0))
        );
    }

    #[test]
    fn sheet_name_quoting_round_trips() {
        // Plain names stay bare; anything the tokenizer could misread is
        // quoted, and re-parsing recovers the original name exactly.
        assert_eq!(quote_sheet_name("Sheet2"), "Sheet2");
        assert_eq!(quote_sheet_name("My Sheet"), "'My Sheet'");
        assert_eq!(quote_sheet_name("Bob's"), "'Bob''s'");
        assert_eq!(quote_sheet_name("2024"), "'2024'");
        for name in ["Sheet2", "My Sheet", "Bob's", "2024", "Q1-Report"] {
            let src = format!("={}!A1", quote_sheet_name(name));
            assert_eq!(
                parse(&src).unwrap(),
                Expr::XRef(name.into(), CellRef::new(0, 0)),
                "round trip failed for {name:?} (as {src})"
            );
        }
    }

    #[test]
    fn plain_references_are_untouched_by_sheet_support() {
        // The regression guard: nothing about ordinary formulas changed.
        assert_eq!(parse("=A1").unwrap(), Expr::Ref(CellRef::new(0, 0)));
        assert_eq!(parse("=TRUE").unwrap(), Expr::Bool(true));
        assert_eq!(
            parse("=SUM(A1:A10)").unwrap(),
            Expr::Call(
                "SUM".into(),
                vec![Expr::Range(CellRef::new(0, 0), CellRef::new(9, 0))]
            )
        );
    }

    // --- 3-D references across a sheet run (issue #43) ---

    fn cr(row: u32, col: u32) -> CellRef {
        CellRef::new(row, col)
    }

    #[test]
    fn parses_a_three_d_single_cell_reference() {
        // THE acceptance criterion at the parser level.
        assert_eq!(
            parse("=Sheet1:Sheet3!A1").unwrap(),
            Expr::X3D("Sheet1".into(), "Sheet3".into(), cr(0, 0), cr(0, 0)),
            "a single-cell 3-D reference is the degenerate 1x1 rectangle"
        );
        assert_eq!(
            parse("=SUM(Sheet1:Sheet3!A1)").unwrap(),
            Expr::Call(
                "SUM".into(),
                vec![Expr::X3D(
                    "Sheet1".into(),
                    "Sheet3".into(),
                    cr(0, 0),
                    cr(0, 0)
                )]
            )
        );
    }

    #[test]
    fn parses_a_three_d_range() {
        assert_eq!(
            parse("=SUM(Sheet1:Sheet3!A1:B10)").unwrap(),
            Expr::Call(
                "SUM".into(),
                vec![Expr::X3D(
                    "Sheet1".into(),
                    "Sheet3".into(),
                    cr(0, 0),
                    cr(9, 1)
                )]
            )
        );
        // Corners are normalised, exactly as a 2-D range's are.
        assert_eq!(
            parse("=SUM(Sheet1:Sheet3!B10:A1)").unwrap(),
            parse("=SUM(Sheet1:Sheet3!A1:B10)").unwrap()
        );
    }

    #[test]
    fn three_d_endpoints_may_be_quoted() {
        assert_eq!(
            parse("=SUM('Q1 2024':'Q4 2024'!A1)").unwrap(),
            Expr::Call(
                "SUM".into(),
                vec![Expr::X3D(
                    "Q1 2024".into(),
                    "Q4 2024".into(),
                    cr(0, 0),
                    cr(0, 0)
                )]
            )
        );
        // Mixed quoting, and `''` as an escaped quote in the second name.
        assert_eq!(
            parse("=SUM(Start:'Bob''s Data'!A1)").unwrap(),
            Expr::Call(
                "SUM".into(),
                vec![Expr::X3D(
                    "Start".into(),
                    "Bob's Data".into(),
                    cr(0, 0),
                    cr(0, 0)
                )]
            )
        );
    }

    #[test]
    fn a_three_d_run_written_backwards_keeps_the_names_as_written() {
        // The parser does NOT reorder the endpoints: which way round the run
        // goes is a tab-order question, and only the workbook knows the tab
        // order. Reordering here would be a guess.
        assert_eq!(
            parse("=SUM(Sheet3:Sheet1!A1)").unwrap(),
            Expr::Call(
                "SUM".into(),
                vec![Expr::X3D(
                    "Sheet3".into(),
                    "Sheet1".into(),
                    cr(0, 0),
                    cr(0, 0)
                )]
            )
        );
    }

    #[test]
    fn a_plain_range_is_never_mistaken_for_a_three_d_span() {
        // THE regression the `:` lookahead could easily cause. If `A1:B4`
        // were read as the span `A1`..`B4`, every ordinary range in the
        // workbook would stop being a range.
        assert_eq!(
            parse("=SUM(A1:B4)").unwrap(),
            Expr::Call("SUM".into(), vec![Expr::Range(cr(0, 0), cr(3, 1))])
        );
        // A requalified same-sheet range stays a 2-D XRange, not a span.
        assert_eq!(
            parse("=SUM(Sheet2!A1:Sheet2!B4)").unwrap(),
            Expr::Call(
                "SUM".into(),
                vec![Expr::XRange("Sheet2".into(), cr(0, 0), cr(3, 1))]
            )
        );
        // And a bare cross-sheet range is unaffected.
        assert_eq!(
            parse("=SUM(Sheet2!A1:B4)").unwrap(),
            Expr::Call(
                "SUM".into(),
                vec![Expr::XRange("Sheet2".into(), cr(0, 0), cr(3, 1))]
            )
        );
    }

    #[test]
    fn a_colon_without_a_following_bang_is_not_a_span() {
        // `Total:B4` is a name-to-cell range the parser must not silently
        // reinterpret as a sheet run. Without the `!` requirement the
        // qualifier scanner would eat `Total:B4` whole.
        assert!(
            !matches!(parse("=SUM(Total:B4)"), Ok(Expr::Call(_, _))),
            "a bare name is not a sheet qualifier"
        );
        // The tokenizer must not have produced a SheetSpan for it either.
        assert!(!tokenize("Total:B4")
            .unwrap()
            .iter()
            .any(|t| matches!(t, Token::SheetSpan { .. })));
    }

    #[test]
    fn tokenizes_a_three_d_span_with_absolute_markers() {
        assert_eq!(
            tokenize("Sheet1:Sheet3!$B$2").unwrap(),
            vec![Token::SheetSpan {
                first: "Sheet1".into(),
                last: "Sheet3".into(),
                cell: cr(1, 1),
                abs_col: true,
                abs_row: true
            }],
            "the tokenizer still records the $ markers a rewrite needs"
        );
    }

    // --- defined names ----------------------------------------------------

    /// A resolver that knows exactly one name.
    fn one_name(ident: &'static str, target: &'static str) -> impl Fn(&str) -> Option<Expr> {
        move |w: &str| {
            w.eq_ignore_ascii_case(ident)
                .then(|| parse(target).unwrap())
        }
    }

    #[test]
    fn a_name_parses_to_the_very_same_tree_as_the_range_it_stands_for() {
        // THE acceptance criterion, at the parser level: =SUM(Sales) must be
        // indistinguishable from =SUM(Sheet1!B2:B1000) after parsing, so the
        // columnar fast path fires identically for both.
        let r = one_name("SALES", "=Sheet1!B2:B1000");
        assert_eq!(
            parse_with_names("=SUM(Sales)", &r).unwrap(),
            parse("=SUM(Sheet1!B2:B1000)").unwrap()
        );
    }

    #[test]
    fn an_unknown_name_is_reported_rather_than_guessed_at() {
        let r = one_name("SALES", "=Sheet1!B2:B1000");
        assert_eq!(
            parse_with_names("=SUM(Revenue)", &r).unwrap_err(),
            ParseError::UnknownName("REVENUE".into())
        );
        // And with no name table at all, every bare word is unknown.
        assert_eq!(
            parse("=Sales").unwrap_err(),
            ParseError::UnknownName("SALES".into())
        );
    }

    #[test]
    fn name_lookup_is_case_insensitive() {
        let r = one_name("SALES", "=Sheet1!B2:B4");
        for spelling in ["Sales", "SALES", "sales", "sAlEs"] {
            assert!(
                parse_with_names(&format!("={spelling}"), &r).is_ok(),
                "{spelling} should resolve"
            );
        }
    }

    #[test]
    fn a_name_may_stand_for_a_constant_or_a_single_cell() {
        let rate = one_name("RATE", "=0.96");
        assert_eq!(parse_with_names("=A1*Rate", &rate).unwrap(), {
            Expr::Binary(
                BinOp::Mul,
                Box::new(Expr::Ref(CellRef::new(0, 0))),
                Box::new(Expr::Number(0.96)),
            )
        });
        let anchor = one_name("ANCHOR", "=Sheet1!$C$3");
        assert_eq!(
            parse_with_names("=Anchor", &anchor).unwrap(),
            Expr::XRef("Sheet1".into(), CellRef::new(2, 2))
        );
    }

    #[test]
    fn function_names_still_win_over_the_name_table() {
        // A resolver that would happily claim SUM must never be consulted for
        // `SUM(`, or every builtin could be shadowed by a stray name.
        let greedy = |_: &str| Some(Expr::Number(1.0));
        assert_eq!(
            parse_with_names("=SUM(A1:A2)", &greedy).unwrap(),
            Expr::Call(
                "SUM".into(),
                vec![Expr::Range(CellRef::new(0, 0), CellRef::new(1, 0))]
            )
        );
    }

    #[test]
    fn cell_references_still_win_over_the_name_table() {
        // The tokenizer resolves A1 as a reference before the parser ever sees
        // an Ident, so a resolver cannot hijack it.
        let greedy = |_: &str| Some(Expr::Number(1.0));
        assert_eq!(
            parse_with_names("=A1", &greedy).unwrap(),
            Expr::Ref(CellRef::new(0, 0))
        );
        assert_eq!(
            parse_with_names("=TRUE", &greedy).unwrap(),
            Expr::Bool(true)
        );
    }

    #[test]
    fn a_resolved_name_participates_in_surrounding_expressions() {
        let r = one_name("SALES", "=Sheet1!B2:B4");
        // Postfix and infix operators must apply to the resolved tree.
        assert_eq!(
            parse_with_names("=SUM(Sales)+1", &r).unwrap(),
            parse("=SUM(Sheet1!B2:B4)+1").unwrap()
        );
    }

    // --- @ implicit intersection and A1# spill-range (#27 P4) --------------

    #[test]
    fn at_prefix_parses_as_implicit_intersection() {
        // `@A1:A10` forces the range to collapse to a single value.
        assert_eq!(
            parse("=@A1:A10").unwrap(),
            Expr::Intersect(Box::new(Expr::Range(
                CellRef::new(0, 0),
                CellRef::new(9, 0)
            )))
        );
        // `@` before a single reference is legal too (a no-op intersection).
        assert_eq!(
            parse("=@A1").unwrap(),
            Expr::Intersect(Box::new(Expr::Ref(CellRef::new(0, 0))))
        );
        // `@` before a function call wraps the whole call.
        assert_eq!(
            parse("=@SUM(A1:A3)").unwrap(),
            Expr::Intersect(Box::new(Expr::Call(
                "SUM".into(),
                vec![Expr::Range(CellRef::new(0, 0), CellRef::new(2, 0))]
            )))
        );
    }

    #[test]
    fn at_prefix_binds_like_unary_minus() {
        // `@A1:A3 + 1` intersects the range, then adds — the `@` binds to the
        // reference, not to the whole sum, exactly as unary minus would.
        assert_eq!(
            parse("=@A1:A3+1").unwrap(),
            Expr::Binary(
                BinOp::Add,
                Box::new(Expr::Intersect(Box::new(Expr::Range(
                    CellRef::new(0, 0),
                    CellRef::new(2, 0)
                )))),
                Box::new(num(1.0)),
            )
        );
    }

    #[test]
    fn hash_suffix_parses_as_spill_range() {
        // `A1#` is the whole spill anchored at A1.
        assert_eq!(parse("=A1#").unwrap(), Expr::SpillRange(CellRef::new(0, 0)));
        // Inside a call: `SUM(A1#)` sums the spill.
        assert_eq!(
            parse("=SUM(A1#)").unwrap(),
            Expr::Call("SUM".into(), vec![Expr::SpillRange(CellRef::new(0, 0))])
        );
        // In an arithmetic context the `#` binds only to the reference it
        // follows, so `A1#+B2` is (spill-range A1) + (ref B2).
        assert_eq!(
            parse("=A1#+B2").unwrap(),
            Expr::Binary(
                BinOp::Add,
                Box::new(Expr::SpillRange(CellRef::new(0, 0))),
                Box::new(Expr::Ref(CellRef::new(1, 1))),
            )
        );
    }

    #[test]
    fn hash_ref_constant_is_still_ref_not_a_spill_suffix() {
        // A bare `#` is the spill suffix, but `#REF!` is the error constant and
        // must keep parsing as one — the two share the `#` lead byte.
        assert_eq!(
            parse("=#REF!").unwrap(),
            Expr::Error(ferrix_core::ErrorKind::Ref)
        );
        // `A1#` next to nothing is a spill range; `#REF!` alone is the error.
        assert_eq!(parse("=A1#").unwrap(), Expr::SpillRange(CellRef::new(0, 0)));
    }

    #[test]
    fn a_range_endpoint_is_never_a_spill_range() {
        // `A1:B2` is a range; the `#` suffix only applies to a lone reference,
        // so a range endpoint can never be silently turned into a spill anchor.
        assert_eq!(
            parse("=A1:B2").unwrap(),
            Expr::Range(CellRef::new(0, 0), CellRef::new(1, 1))
        );
    }

    #[test]
    fn at_and_hash_compose() {
        // `@A1#` — force the spilled array to collapse to one value.
        assert_eq!(
            parse("=@A1#").unwrap(),
            Expr::Intersect(Box::new(Expr::SpillRange(CellRef::new(0, 0))))
        );
    }

    #[test]
    fn a_broken_spill_anchor_reparses_as_ref_error() {
        // A structural delete rewrites `A1#`'s anchor to `#REF!` and leaves the
        // `#` behind, so the reference-rewrite path emits `#REF!#`. That text
        // must re-parse (to `#REF!`), or the cell would show `#NAME?` on reload
        // — blaming an unknown name for what is a broken reference.
        assert_eq!(
            parse("=#REF!#").unwrap(),
            Expr::Error(ferrix_core::ErrorKind::Ref)
        );
        // The `#REF!` constant on its own is unchanged.
        assert_eq!(
            parse("=#REF!").unwrap(),
            Expr::Error(ferrix_core::ErrorKind::Ref)
        );
    }
}
