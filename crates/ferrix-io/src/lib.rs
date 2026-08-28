//! # ferrix-io
//!
//! Ingest and export. CSV is implemented; `xlsx` and Parquet slot in beside it
//! behind the same `load_*` / `save_*` shape.

pub mod csv;

pub use csv::{load_csv, CsvError, CsvOptions, LoadStats};
