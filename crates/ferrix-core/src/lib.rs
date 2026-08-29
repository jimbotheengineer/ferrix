//! # ferrix-core
//!
//! Storage engine for Ferrix. Contains no UI and no I/O so it stays fast to
//! compile and trivial to fuzz/benchmark in isolation.
//!
//! Design constraints that drive everything here:
//! 1. 10M+ rows must fit in consumer RAM -> columnar, typed, bit-packed.
//! 2. Scrolling must never touch more than a viewport of cells -> O(1) reads.
//! 3. Aggregations run over typed slices, not boxed values -> vectorizable.

pub mod annotation;
pub mod arena;
pub mod bitmap;
pub mod budget;
pub mod cancel;
pub mod chart;
pub mod column;
pub mod comment;
pub mod filter;
pub mod format;
pub mod merge;
pub mod numfmt;
pub mod order;
pub mod overlay;
pub mod scene;
pub mod search;
pub mod selection;
pub mod sheet;
pub mod sizing;
pub mod sort;
pub mod table;
pub mod tsv;
pub mod value;

pub use arena::{StrId, StringArena};
pub use bitmap::Bitmap;
pub use budget::Budget;
pub use cancel::CancelToken;
pub use column::Column;
pub use comment::{Comment, CommentMap};
pub use filter::RowFilter;
pub use format::{
    CellOverride, ColumnFormat, ManualStyle, PlanEntry, RangeFormat, RuleEval, SheetFormat,
};
pub use order::{AxisOrder, OrderError, SheetOrder};
pub use overlay::{CellInput, EditOverlay, OverlayChange};
pub use search::{
    replace_stream, IdSet, LookIn, Query, ReplaceOutcome, ReplaceReport, ReplaceSpec, SearchResults,
};
pub use selection::Selection;
pub use sheet::{column_name, CellRef, Sheet, SheetCell, SheetId};
pub use sizing::{
    ColSizes, HiddenRows, Outline, OutlineError, OutlineGroup, RowSizes, SheetSizing,
    MAX_OUTLINE_LEVEL,
};
pub use sort::{cycle_click, CellKeys, SortCell, SortDir, SortKey, SortOrder};
pub use table::{
    CellStyle, CmpOp, ColumnType, CompiledPredicate, ConditionalRule, DateStyle, NumberFormat,
    Predicate, Rgb, RowMask, Table, TableColumn, TableRange, UniquenessIndex, Validation,
    ValidationReport, ValidationRule, Violation,
};
pub use value::{format_number, ErrorKind, Value, ValueTag};
