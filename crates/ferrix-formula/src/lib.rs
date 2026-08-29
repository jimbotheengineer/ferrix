//! # ferrix-formula
//!
//! Tokenizer, Pratt parser, evaluator, and dependency graph for spreadsheet
//! formulas.

pub mod criteria;
pub mod depgraph;
pub mod eval;
pub mod fill;
pub mod parser;
pub mod refscan;
pub mod remap;

pub use criteria::{Criterion, Pattern, Scalar};
pub use depgraph::{DepGraph, Precedent};
pub use eval::{eval, eval_view, CellSource};
pub use parser::{parse, quote_sheet_name, BinOp, Expr, ParseError, UnOp};
pub use remap::{remap_columns, remap_formula, remap_rows, AxisMap};
