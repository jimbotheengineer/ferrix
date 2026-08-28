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
    #[error("trailing input after expression: {0:?}")]
    Trailing(String),
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

/// Parse a formula. Accepts an optional leading `=`.
pub fn parse(input: &str) -> Result<Expr, ParseError> {
    let body = input.strip_prefix('=').unwrap_or(input);
    let tokens = tokenize(body)?;
    let mut p = Parser { tokens, pos: 0 };
    let expr = p.parse_expr(0)?;
    if p.pos < p.tokens.len() {
        return Err(ParseError::Trailing(format!("{:?}", p.tokens[p.pos])));
    }
    Ok(expr)
}

struct Parser {
    tokens: Vec<Token>,
    pos: usize,
}

impl Parser {
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
    fn parse_expr(&mut self, min_prec: u8) -> Result<Expr, ParseError> {
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
            Token::Ident(name) => {
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
}
