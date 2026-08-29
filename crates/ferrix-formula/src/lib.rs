//! # ferrix-formula
//!
//! Tokenizer, Pratt parser, evaluator, and dependency graph for spreadsheet
//! formulas.

pub mod criteria;
pub mod datetime;
pub mod depgraph;
pub mod eval;
pub mod fill;
pub mod lookup;
pub mod names;
pub mod parser;
pub mod refedit;
pub mod refscan;
pub mod remap;
pub mod stats;
pub mod text;

#[cfg(test)]
mod compose_tests;

pub use criteria::{Criterion, Pattern, Scalar};
pub use depgraph::{DepGraph, Precedent};
pub use eval::{eval, eval_view, CellSource};
pub use names::{DefinedName, NameError, NameScope, NameTable};
pub use parser::{parse, parse_with_names, quote_sheet_name, BinOp, Expr, ParseError, UnOp};
pub use remap::{paste_formula, remap_columns, remap_formula, remap_rows, AxisMap};
