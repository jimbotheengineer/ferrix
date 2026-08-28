//! # ferrix-core
//!
//! Storage engine for Ferrix. Contains no UI and no I/O so it stays fast to
//! compile and trivial to fuzz/benchmark in isolation.
//!
//! Design constraints that drive everything here:
//! 1. 10M+ rows must fit in consumer RAM -> columnar, typed, bit-packed.
//! 2. Scrolling must never touch more than a viewport of cells -> O(1) reads.
//! 3. Aggregations run over typed slices, not boxed values -> vectorizable.

pub mod arena;
pub mod bitmap;
pub mod column;
pub mod overlay;
pub mod search;
pub mod selection;
pub mod sheet;
pub mod tsv;
pub mod value;

pub use arena::{StrId, StringArena};
pub use bitmap::Bitmap;
pub use column::Column;
pub use overlay::{CellInput, EditOverlay};
pub use search::{IdSet, Query, SearchResults};
pub use selection::Selection;
pub use sheet::{column_name, CellRef, Sheet};
pub use value::{format_number, ErrorKind, Value, ValueTag};
