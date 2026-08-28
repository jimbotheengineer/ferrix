//! # ferrix-io
//!
//! Ingest, conversion, and out-of-core storage.
//!
//! Two paths, chosen by file size:
//!
//! - **Small files** load straight into RAM via [`load_csv`] — simplest and
//!   fastest when the data comfortably fits.
//! - **Large files** are converted once into the columnar [`format`] and then
//!   memory-mapped via [`MappedSheet`], so a 10GB dataset is bounded by disk
//!   rather than RAM and opens instantly on subsequent runs.

pub mod convert;
pub mod csv;
pub mod edits;
pub mod format;
pub mod mapped;
pub mod xlsx;

pub use convert::{cache_is_fresh, cache_path_for, convert_csv, ConvertError, ConvertStats};
pub use csv::{load_csv, CsvError, CsvOptions, LoadStats};
pub use format::FormatError;
pub use mapped::MappedSheet;
pub use xlsx::{
    export_workbook, export_xlsx, export_xlsx_with_formulas, import_xlsx, import_xlsx_full,
    ImportStats, ImportedSheet, SheetExport, XlsxError, XLSX_MAX_COLS, XLSX_MAX_ROWS,
};

/// Files at or above this size are converted and memory-mapped rather than
/// loaded into RAM.
///
/// Rationale: in-RAM storage costs roughly 1.2x the CSV's size in heap. At
/// 1GB that is ~1.2GB resident, which is fine; at 10GB it is ~12GB, which is
/// not. 1GB is the point where the conversion cost starts paying for itself
/// on the second open.
pub const MMAP_THRESHOLD_BYTES: u64 = 1 << 30;

/// Should this file take the out-of-core path?
pub fn should_use_mmap(path: &std::path::Path) -> bool {
    path.metadata()
        .map(|m| m.len() >= MMAP_THRESHOLD_BYTES)
        .unwrap_or(false)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn threshold_is_one_gigabyte() {
        assert_eq!(MMAP_THRESHOLD_BYTES, 1_073_741_824);
    }

    #[test]
    fn small_files_stay_in_ram() {
        // A missing or tiny file must not take the conversion path.
        assert!(!should_use_mmap(std::path::Path::new(
            "definitely-does-not-exist.csv"
        )));
    }
}
