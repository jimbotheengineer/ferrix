//! # ferrix-formula
//!
//! Tokenizer, Pratt parser, and evaluator for spreadsheet formulas.

pub mod eval;
pub mod parser;

pub use eval::eval;
pub use parser::{parse, BinOp, Expr, ParseError, UnOp};
