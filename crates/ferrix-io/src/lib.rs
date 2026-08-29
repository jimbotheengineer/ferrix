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

pub mod compact;
pub mod convert;
pub mod csv;
pub mod edits;
pub mod export;
pub mod format;
pub mod format_sidecar;
pub mod mapped;
pub mod pool;
pub mod table_xlsx;
pub mod xlsx;

pub use compact::{compact_cache, CompactError, CompactOutcome, CompactStats};
pub use convert::{
    cache_is_fresh, cache_path_for, convert_csv, convert_csv_cancellable, ConvertError,
    ConvertStats,
};
pub use csv::{load_csv, CsvError, CsvOptions, LoadStats};
pub use format::FormatError;
pub use format_sidecar::{format_path_for, load_format, save_format, FormatSidecarError};
pub use mapped::MappedSheet;
pub use table_xlsx::{import_tables, write_table, FerrixTag, ImportedTable};
pub use xlsx::{
    export_workbook, export_workbook_with_names, export_xlsx, export_xlsx_with_formulas,
    export_xlsx_with_tables, import_defined_names, import_xlsx, import_xlsx_full, ImportStats,
    ImportedSheet, SheetExport, XlsxError, XLSX_MAX_COLS, XLSX_MAX_ROWS,
};

/// Files at or above this size are converted and memory-mapped rather than
/// loaded into RAM, regardless of how much memory the machine has.
///
/// Rationale: in-RAM storage costs roughly 1.2x the CSV's size in heap. At
/// 1GB that is ~1.2GB resident, which is fine; at 10GB it is ~12GB, which is
/// not. 1GB is the point where the conversion cost starts paying for itself
/// on the second open.
pub const MMAP_THRESHOLD_BYTES: u64 = 1 << 30;

/// Heap cost of loading a CSV into RAM, as a multiple of its file size.
///
/// Measured against the in-RAM loader: typed columns plus an interned string
/// arena come to roughly 1.2x the source bytes for representative data. Used
/// to decide whether a file *below* the fixed threshold would still not fit.
pub const IN_RAM_COST_MULTIPLE: u64 = 12; // tenths, i.e. 1.2x

/// Should this file take the out-of-core path?
///
/// Two independent reasons to say yes:
///
/// 1. **Size.** At or above [`MMAP_THRESHOLD_BYTES`] the conversion pays for
///    itself on the second open, so it is worth doing even on a large machine.
/// 2. **Fit.** Below that threshold, an in-RAM load is still refused if its
///    estimated heap cost does not fit the *measured* memory budget. A 900 MB
///    CSV is comfortable on a workstation and fatal on a 2 GB VM, and only a
///    measurement can tell the two apart.
///
/// This is what makes "opening a file larger than available RAM degrades
/// gracefully" true rather than aspirational: the mmap path is chosen because
/// the RAM was counted, not because the file crossed a number somebody picked.
pub fn should_use_mmap(path: &std::path::Path) -> bool {
    let Ok(meta) = path.metadata() else {
        return false;
    };
    let len = meta.len();
    if len >= MMAP_THRESHOLD_BYTES {
        return true;
    }
    let estimated_heap = len.saturating_mul(IN_RAM_COST_MULTIPLE) / 10;
    ferrix_core::Budget::sample()
        .admit(estimated_heap, "in-RAM load")
        .is_err()
}

/// Estimated heap cost of loading `bytes` of CSV into RAM.
///
/// Exposed so the UI can explain *why* a file took the conversion path.
pub fn estimated_in_ram_bytes(bytes: u64) -> u64 {
    bytes.saturating_mul(IN_RAM_COST_MULTIPLE) / 10
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

    #[test]
    fn a_file_that_would_not_fit_takes_the_mmap_path() {
        // The acceptance criterion, expressed against the estimator rather
        // than by actually allocating 40 GB: a file whose in-RAM cost exceeds
        // the budget must be refused for the in-RAM path.
        let tiny_machine = ferrix_core::Budget::from_available(1 << 30);
        let big_file = 4u64 << 30;
        assert!(
            tiny_machine
                .admit(estimated_in_ram_bytes(big_file), "in-RAM load")
                .is_err(),
            "a 4GB file must not be admitted into a 1GB budget"
        );

        let workstation = ferrix_core::Budget::from_available(64 << 30);
        let modest_file = 500u64 << 20;
        assert!(
            workstation
                .admit(estimated_in_ram_bytes(modest_file), "in-RAM load")
                .is_ok(),
            "a 500MB file must load in RAM on a machine with room"
        );
    }

    #[test]
    fn the_in_ram_estimate_is_above_the_file_size() {
        // Typed columns plus an arena cost MORE than the raw bytes; an
        // estimate below 1.0x would systematically admit loads that then blow
        // the budget.
        assert!(estimated_in_ram_bytes(1_000_000) > 1_000_000);
    }
}
