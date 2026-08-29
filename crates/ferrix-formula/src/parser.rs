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
    Unary(UnOp, Box<Expr>),
    Binary(BinOp, Box<Expr>, Box<Expr>),
    Call(String, Vec<Expr>),
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
}

/// Deepest expression nesting the parser will build.
///
/// Excel's own limit is 64 levels of nested functions; this is set far above
/// anything a human writes so that no real formula is refused, while staying
/// two orders of magnitude below the depth that would exhaust the default
/// 8 MB main-thread stack (each level costs on the order of a hundred bytes
/// of frame across `parse_expr`/`parse_prefix`).
pub const MAX_PARSE_DEPTH: usize = 256;

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
                if bytes.get(i) == Some(&b'!') {
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
/// `at` points at the byte that should be `!`; `name` is the already-decoded
/// sheet name. Returns the token plus the index just past the reference.
fn lex_sheet_qualified(
    input: &str,
    bytes: &[u8],
    at: usize,
    name: String,
) -> Result<(Token, usize), ParseError> {
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
        }) => Ok((
            Token::SheetRef {
                sheet: name,
                cell,
                abs_col,
                abs_row,
            },
            i,
        )),
        _ => Err(ParseError::BadSheetRef(name)),
    }
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
        loop {
            let op = match self.peek() {
                Some(Token::Op(op)) => *op,
                Some(Token::Plus) => BinOp::Add,
                Some(Token::Minus) => BinOp::Sub,
                _ => break,
            };
            let prec = op.precedence();
            if prec < min_prec {
                break;
            }
            self.pos += 1;
            let next_min = if op.right_assoc() { prec } else { prec + 1 };
            let rhs = self.parse_expr(next_min)?;
            lhs = Expr::Binary(op, Box::new(lhs), Box::new(rhs));
        }
        Ok(lhs)
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
            Token::Number(n) => Expr::Number(n),
            Token::Text(s) => Expr::Text(s),
            Token::Bool(b) => Expr::Bool(b),
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
            Token::Ident(name) => {
                // A following `(` makes it a function call; otherwise it is a
                // defined name, and the name table gets the first look.
                // Falling through to UnknownName (which the workbook renders
                // as #NAME?) only AFTER the table has been consulted is what
                // "resolution happens in the parser" means.
                if !matches!(self.peek(), Some(Token::LParen)) {
                    match (self.resolve)(&name) {
                        Some(expr) => expr,
                        None => return Err(ParseError::UnknownName(name)),
                    }
                } else {
                    self.expect(&Token::LParen, "( after function name")?;
                    let mut args = Vec::new();
                    if matches!(self.peek(), Some(Token::RParen)) {
                        self.pos += 1;
                    } else {
                        loop {
                            args.push(self.parse_expr(0)?);
                            match self.next() {
                                Some(Token::Comma) => continue,
                                Some(Token::RParen) => break,
                                Some(t) => {
                                    return Err(ParseError::Expected(", or )", format!("{t:?}")))
                                }
                                None => return Err(ParseError::UnexpectedEnd),
                            }
                        }
                    }
                    Expr::Call(name, args)
                }
            }
            Token::LParen => {
                let inner = self.parse_expr(0)?;
                self.expect(&Token::RParen, ")")?;
                inner
            }
            t => return Err(ParseError::Expected("a value", format!("{t:?}"))),
        };
        // Postfix percent.
        if matches!(self.peek(), Some(Token::Percent)) {
            self.pos += 1;
            return Ok(Expr::Unary(UnOp::Percent, Box::new(expr)));
        }
        Ok(expr)
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
}
