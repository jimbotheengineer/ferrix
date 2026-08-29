//! Parquet and Arrow IPC import/export.
//!
//! ## Why this exists
//!
//! Parquet is how columnar data actually moves between pandas, DuckDB, Spark
//! and everything else. Without it Ferrix can only talk to CSV and Excel — the
//! two formats that are *worst* at the scale Ferrix targets. Arrow IPC (the
//! `.arrow` file format) is the same story for zero-copy handoff.
//!
//! ## The scale invariant
//!
//! Both directions obey the rule `convert.rs` sets: **peak memory is bounded
//! and independent of row count.**
//!
//! * **Import.** A Parquet file is read one *row group* at a time through
//!   [`parquet`]'s Arrow reader with a bounded batch size. A large file is
//!   streamed straight into the columnar [`crate::format`] via the same
//!   [`crate::convert::Spill`] writer the CSV converter uses, and then
//!   memory-mapped. We never hold more than one batch. See
//!   [`convert_parquet`].
//! * **Export.** Written through Parquet's *low-level* writer rather than the
//!   Arrow `RecordBatch` writer, precisely so we can emit **one column chunk at
//!   a time**: for each row group we materialise a single column's stripe,
//!   hand it to the column writer, and drop it before touching the next
//!   column. Peak is one stripe (`ROW_GROUP_ROWS` cells), not the file. See
//!   [`export_parquet`].
//!
//! The one thing that stays resident on import is the string arena, exactly as
//! in `convert.rs`, and for the same reason: text columns are low-cardinality.
//! Which brings us to the reason dictionary columns get special handling.
//!
//! ## Dictionary columns map onto the arena, not onto rows
//!
//! `Value::Text` holds a [`StrId`] — a 4-byte index into
//! [`ferrix_core::StringArena`] — never the bytes. A Parquet dictionary-encoded
//! UTF-8 column is *already* in that shape: K distinct values plus one key per
//! row. So import interns the K dictionary values **once** and then maps keys
//! through a small `Vec<StrId>`. A 100M-row column with 3 distinct values costs
//! 3 arena entries, not 100M. Expanding the dictionary to per-row strings first
//! (the obvious `as_string()` call) would multiply arena occupancy by the row
//! count and break the invariant silently — `dictionary_column_does_not_expand`
//! in the tests below is the assertion that would catch it.
//!
//! Note the arena choice: this is the **per-sheet** [`ferrix_core::StringArena`],
//! not the process-wide formula-text interner in `ferrix_core::arena`. The
//! latter is for values the formula evaluator produces from `&self` and its
//! entries are leaked for the process lifetime; imported file data belongs to
//! the sheet and must go away when the sheet does.
//!
//! ## Dates
//!
//! [`Value`] has no date variant — a date is an f64 Excel serial (see
//! `ferrix_formula::datetime`). So Arrow `Timestamp`/`Date32`/`Date64` map to
//! `Value::Number` through [`serial_from_unix_ms`], and export maps back
//! through [`unix_ms_from_serial`] for columns the caller marks as dates. The
//! two are inverses, pinned by test.
//!
//! ## Unsupported types are reported, never coerced
//!
//! A `List`, `Struct`, `Map`, or `Decimal` column has no faithful spreadsheet
//! cell representation. Rendering it to its debug text would produce data that
//! looks fine and is wrong. Import fails with
//! [`ArrowIoError::UnsupportedType`] naming the column and the type instead.

use std::fs::File;
use std::io::BufWriter;
use std::path::{Path, PathBuf};

use arrow::array::{Array, ArrayRef, RecordBatch};
use arrow::datatypes::{DataType, Field, Schema, TimeUnit};
use ferrix_core::{CellRef, Column, Sheet, StrId, StringArena, Value};

use crate::convert::{ConvertError, ConvertStats};

/// Rows per Parquet row group on export, and per read batch on import.
///
/// This is the width of the *stripe* that bounds peak memory. 64K rows of f64
/// is 512KB; of interned text it is 64K `ByteArray` handles pointing at one
/// shared buffer. Large enough that per-row-group metadata is negligible
/// against a 200M-row file (about 3000 row groups), small enough that the
/// working set never approaches the dataset.
pub const ROW_GROUP_ROWS: usize = 64 * 1024;

/// Excel serial of the Unix epoch, 1970-01-01.
///
/// The same constant `ferrix_formula::datetime::now_serial` uses; if these two
/// ever disagree, `TODAY()` and an imported timestamp column would name
/// different days.
pub const UNIX_EPOCH_SERIAL: f64 = 25_569.0;

const MS_PER_DAY: f64 = 86_400_000.0;

/// Unix milliseconds -> Excel serial.
#[inline]
pub fn serial_from_unix_ms(ms: i64) -> f64 {
    UNIX_EPOCH_SERIAL + (ms as f64) / MS_PER_DAY
}

/// Excel serial -> Unix milliseconds. Inverse of [`serial_from_unix_ms`].
#[inline]
pub fn unix_ms_from_serial(serial: f64) -> i64 {
    ((serial - UNIX_EPOCH_SERIAL) * MS_PER_DAY).round() as i64
}

#[derive(Debug, thiserror::Error)]
pub enum ArrowIoError {
    #[error("io error: {0}")]
    Io(#[from] std::io::Error),
    #[error("parquet error: {0}")]
    Parquet(#[from] parquet::errors::ParquetError),
    #[error("arrow error: {0}")]
    Arrow(#[from] arrow::error::ArrowError),
    #[error("conversion error: {0}")]
    Convert(#[from] ConvertError),
    /// Reported rather than coerced. The column name and the Arrow type are
    /// both in the message so the user can see exactly what to cast upstream.
    #[error("column '{column}' has unsupported type {data_type} — cast it before importing (Ferrix cells hold a number, text, bool, or error, and {data_type} has no faithful representation)")]
    UnsupportedType { column: String, data_type: String },
    #[error("file contains no columns")]
    NoColumns,
    #[error("sheet has {rows} rows x {cols} cols but no data to write")]
    Empty { rows: usize, cols: usize },
}

/// What an import produced.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ArrowStats {
    pub rows: usize,
    pub cols: usize,
    /// Distinct strings interned. For a dictionary column this is the
    /// dictionary's cardinality, NOT the row count — that difference is the
    /// whole point of the dictionary path.
    pub distinct_strings: usize,
    pub batches: usize,
    pub millis: u128,
}

/// Which on-disk shape a path names.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ArrowFormat {
    Parquet,
    /// Arrow IPC, either the file ("Feather v2") or stream framing.
    Ipc,
}

/// Route a path by extension. Returns `None` for anything this module does not
/// own, so the caller's existing csv/xlsx dispatch keeps its behaviour.
///
/// This is deliberately the *only* place the extension list lives; the UI's
/// open handler and the file dialog both read it rather than repeating the
/// strings.
pub fn format_for_path(path: &Path) -> Option<ArrowFormat> {
    let ext = path.extension()?.to_str()?.to_ascii_lowercase();
    match ext.as_str() {
        "parquet" | "pq" => Some(ArrowFormat::Parquet),
        "arrow" | "ipc" | "feather" => Some(ArrowFormat::Ipc),
        _ => None,
    }
}

/// Extensions this module opens, for the file dialog's filter.
pub const ARROW_EXTENSIONS: &[&str] = &["parquet", "pq", "arrow", "ipc", "feather"];

// ---------------------------------------------------------------------------
// Type mapping
// ---------------------------------------------------------------------------

/// How one Arrow column maps onto Ferrix cells.
///
/// Resolved once per column, before any row is touched, so an unsupported type
/// is reported before we have spent time on the file — and so the per-row loop
/// has no type dispatch in it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ColumnKind {
    /// Float64/Float32/Float16 and every integer width -> `Value::Number`.
    Number,
    /// Boolean -> `Value::Bool`.
    Bool,
    /// Utf8/LargeUtf8/Utf8View -> `Value::Text`, interned per value.
    Text,
    /// Dictionary(_, Utf8-ish) -> `Value::Text`, interned per *dictionary
    /// entry*. The distinction from [`ColumnKind::Text`] is the scale
    /// invariant, not an optimisation.
    DictText,
    /// Date32/Date64/Timestamp -> `Value::Number` holding an Excel serial.
    DateSerial,
    /// Null -> `Value::Empty` for every row.
    Null,
}

/// Classify an Arrow type, or report it as unsupported.
///
/// Integers wider than 2^53 lose precision in an f64 cell. That is inherent to
/// `Value::Number` being f64 (which is itself required for IEEE-754 spreadsheet
/// semantics), so it is documented rather than rejected: rejecting Int64 would
/// make the importer useless against real Parquet, and every spreadsheet in
/// existence has the same limit.
pub fn classify(field: &Field) -> Result<ColumnKind, ArrowIoError> {
    let unsupported = || ArrowIoError::UnsupportedType {
        column: field.name().clone(),
        data_type: field.data_type().to_string(),
    };
    Ok(match field.data_type() {
        DataType::Float16 | DataType::Float32 | DataType::Float64 => ColumnKind::Number,
        DataType::Int8 | DataType::Int16 | DataType::Int32 | DataType::Int64 => ColumnKind::Number,
        DataType::UInt8 | DataType::UInt16 | DataType::UInt32 | DataType::UInt64 => {
            ColumnKind::Number
        }
        DataType::Boolean => ColumnKind::Bool,
        DataType::Utf8 | DataType::LargeUtf8 | DataType::Utf8View => ColumnKind::Text,
        DataType::Null => ColumnKind::Null,
        DataType::Date32 | DataType::Date64 | DataType::Timestamp(_, _) => ColumnKind::DateSerial,
        DataType::Dictionary(_, value) => match value.as_ref() {
            DataType::Utf8 | DataType::LargeUtf8 | DataType::Utf8View => ColumnKind::DictText,
            _ => return Err(unsupported()),
        },
        _ => return Err(unsupported()),
    })
}

/// Classify every field up front. Errors name the first offending column.
fn classify_all(schema: &Schema) -> Result<Vec<ColumnKind>, ArrowIoError> {
    if schema.fields().is_empty() {
        return Err(ArrowIoError::NoColumns);
    }
    schema.fields().iter().map(|f| classify(f)).collect()
}

/// Multiplier taking a `Timestamp`'s unit to milliseconds.
fn timestamp_to_ms(unit: &TimeUnit, raw: i64) -> f64 {
    match unit {
        TimeUnit::Second => raw as f64 * 1000.0,
        TimeUnit::Millisecond => raw as f64,
        TimeUnit::Microsecond => raw as f64 / 1000.0,
        TimeUnit::Nanosecond => raw as f64 / 1_000_000.0,
    }
}

// ---------------------------------------------------------------------------
// Per-batch decoding
// ---------------------------------------------------------------------------

/// A decoded column stripe: one `Value` per row of the current batch.
///
/// This is the ONLY buffer that scales with anything, and it scales with the
/// batch (`ROW_GROUP_ROWS`), never with the file. It is reused across batches.
#[derive(Default)]
struct Stripe {
    values: Vec<Value>,
}

/// Decode one Arrow array into `out`, interning any text into `arena`.
///
/// `out` is cleared and refilled; the caller reuses it across batches so the
/// allocation is amortised.
fn decode_array(
    array: &ArrayRef,
    kind: ColumnKind,
    arena: &mut StringArena,
    out: &mut Vec<Value>,
) -> Result<(), ArrowIoError> {
    use arrow::array::*;
    use arrow::datatypes::*;

    let n = array.len();
    out.clear();
    out.reserve(n);

    /// Push `n` values from a primitive array, mapping each through `f`, with
    /// nulls becoming `Value::Empty`.
    macro_rules! prim {
        ($ty:ty, $f:expr) => {{
            let a = array
                .as_any()
                .downcast_ref::<PrimitiveArray<$ty>>()
                .expect("arrow array type matched its DataType");
            let f = $f;
            for i in 0..n {
                out.push(if a.is_null(i) {
                    Value::Empty
                } else {
                    #[allow(clippy::redundant_closure_call)]
                    Value::Number(f(a.value(i)))
                });
            }
        }};
    }

    match (kind, array.data_type()) {
        (_, DataType::Null) => out.resize(n, Value::Empty),

        (ColumnKind::Number, DataType::Float64) => prim!(Float64Type, |v: f64| v),
        (ColumnKind::Number, DataType::Float32) => prim!(Float32Type, |v: f32| v as f64),
        (ColumnKind::Number, DataType::Float16) => {
            prim!(Float16Type, |v| f64::from(v))
        }
        (ColumnKind::Number, DataType::Int8) => prim!(Int8Type, |v: i8| v as f64),
        (ColumnKind::Number, DataType::Int16) => prim!(Int16Type, |v: i16| v as f64),
        (ColumnKind::Number, DataType::Int32) => prim!(Int32Type, |v: i32| v as f64),
        (ColumnKind::Number, DataType::Int64) => prim!(Int64Type, |v: i64| v as f64),
        (ColumnKind::Number, DataType::UInt8) => prim!(UInt8Type, |v: u8| v as f64),
        (ColumnKind::Number, DataType::UInt16) => prim!(UInt16Type, |v: u16| v as f64),
        (ColumnKind::Number, DataType::UInt32) => prim!(UInt32Type, |v: u32| v as f64),
        (ColumnKind::Number, DataType::UInt64) => prim!(UInt64Type, |v: u64| v as f64),

        (ColumnKind::Bool, DataType::Boolean) => {
            let a = array.as_any().downcast_ref::<BooleanArray>().unwrap();
            for i in 0..n {
                out.push(if a.is_null(i) {
                    Value::Empty
                } else {
                    Value::Bool(a.value(i))
                });
            }
        }

        // Date32 counts DAYS since the Unix epoch; the serial is a plain
        // offset with no scaling, which is exactly why `UNIX_EPOCH_SERIAL`
        // exists as a constant rather than being folded into a magic number.
        (ColumnKind::DateSerial, DataType::Date32) => {
            prim!(Date32Type, |v: i32| UNIX_EPOCH_SERIAL + v as f64)
        }
        (ColumnKind::DateSerial, DataType::Date64) => {
            prim!(Date64Type, |v: i64| serial_from_unix_ms(v))
        }
        (ColumnKind::DateSerial, DataType::Timestamp(unit, _)) => {
            let unit = unit.clone();
            let vals: Vec<i64> = match unit {
                TimeUnit::Second => ts_raw::<TimestampSecondType>(array),
                TimeUnit::Millisecond => ts_raw::<TimestampMillisecondType>(array),
                TimeUnit::Microsecond => ts_raw::<TimestampMicrosecondType>(array),
                TimeUnit::Nanosecond => ts_raw::<TimestampNanosecondType>(array),
            };
            for (i, raw) in vals.iter().enumerate() {
                out.push(if array.is_null(i) {
                    Value::Empty
                } else {
                    Value::Number(UNIX_EPOCH_SERIAL + timestamp_to_ms(&unit, *raw) / MS_PER_DAY)
                });
            }
        }

        (ColumnKind::Text, DataType::Utf8) => {
            let a = array.as_any().downcast_ref::<StringArray>().unwrap();
            push_strings(n, |i| (!a.is_null(i)).then(|| a.value(i)), arena, out);
        }
        (ColumnKind::Text, DataType::LargeUtf8) => {
            let a = array.as_any().downcast_ref::<LargeStringArray>().unwrap();
            push_strings(n, |i| (!a.is_null(i)).then(|| a.value(i)), arena, out);
        }
        (ColumnKind::Text, DataType::Utf8View) => {
            let a = array.as_any().downcast_ref::<StringViewArray>().unwrap();
            push_strings(n, |i| (!a.is_null(i)).then(|| a.value(i)), arena, out);
        }

        // The dictionary path. Note what is NOT here: no call to
        // `arrow::compute::cast` to Utf8, no `a.value(i)` returning a fresh
        // &str per row into `arena.intern`. We intern the dictionary ONCE and
        // then only ever touch 4-byte ids.
        (ColumnKind::DictText, DataType::Dictionary(key, _)) => {
            decode_dictionary(array, key.as_ref(), arena, out)?;
        }

        // Reached only if `classify` and this match disagree, which would be a
        // bug in this file rather than in the input.
        (_, dt) => {
            return Err(ArrowIoError::UnsupportedType {
                column: String::from("<unknown>"),
                data_type: dt.to_string(),
            })
        }
    }
    Ok(())
}

/// Raw i64 values out of a timestamp array of a known unit.
fn ts_raw<T>(array: &ArrayRef) -> Vec<i64>
where
    T: arrow::datatypes::ArrowPrimitiveType<Native = i64>,
{
    let a = array
        .as_any()
        .downcast_ref::<arrow::array::PrimitiveArray<T>>()
        .expect("timestamp unit matched");
    a.values().to_vec()
}

fn push_strings<'a, F>(n: usize, get: F, arena: &mut StringArena, out: &mut Vec<Value>)
where
    F: Fn(usize) -> Option<&'a str>,
{
    for i in 0..n {
        out.push(match get(i) {
            Some(s) => Value::Text(arena.intern(s)),
            None => Value::Empty,
        });
    }
}

/// Intern a dictionary's values once, then map keys through the resulting
/// `Vec<StrId>`.
///
/// Cost is O(distinct) interning plus O(rows) index lookups. The alternative —
/// casting the dictionary to a flat `StringArray` and interning per row — is
/// O(rows) interning with O(rows) hash lookups of full string bodies, and on a
/// 100M-row column it is the difference between 3 arena entries and a hash
/// probe per cell.
fn decode_dictionary(
    array: &ArrayRef,
    key_type: &DataType,
    arena: &mut StringArena,
    out: &mut Vec<Value>,
) -> Result<(), ArrowIoError> {
    use arrow::array::*;
    use arrow::datatypes::*;

    /// One dictionary key width. `$k` is the arrow key type.
    macro_rules! dict {
        ($k:ty) => {{
            let d = array
                .as_any()
                .downcast_ref::<DictionaryArray<$k>>()
                .expect("dictionary key width matched its DataType");
            // Intern the dictionary body ONCE — this is the whole point.
            let ids = intern_dictionary_values(d.values(), arena)?;
            let keys = d.keys();
            for i in 0..d.len() {
                out.push(if keys.is_null(i) {
                    Value::Empty
                } else {
                    let k = keys.value(i) as usize;
                    match ids.get(k) {
                        // A null *inside* the dictionary body is a legitimate
                        // encoding of an empty cell.
                        Some(Some(id)) => Value::Text(*id),
                        _ => Value::Empty,
                    }
                });
            }
        }};
    }

    match key_type {
        DataType::Int8 => dict!(Int8Type),
        DataType::Int16 => dict!(Int16Type),
        DataType::Int32 => dict!(Int32Type),
        DataType::Int64 => dict!(Int64Type),
        DataType::UInt8 => dict!(UInt8Type),
        DataType::UInt16 => dict!(UInt16Type),
        DataType::UInt32 => dict!(UInt32Type),
        DataType::UInt64 => dict!(UInt64Type),
        other => {
            return Err(ArrowIoError::UnsupportedType {
                column: String::from("<dictionary key>"),
                data_type: other.to_string(),
            })
        }
    }
    Ok(())
}

/// Intern every entry of a dictionary's value array. One entry per DISTINCT
/// string, by construction — the array has one slot per dictionary entry.
fn intern_dictionary_values(
    values: &ArrayRef,
    arena: &mut StringArena,
) -> Result<Vec<Option<StrId>>, ArrowIoError> {
    use arrow::array::*;
    macro_rules! body {
        ($t:ty) => {{
            let a = values.as_any().downcast_ref::<$t>().unwrap();
            (0..a.len())
                .map(|i| (!a.is_null(i)).then(|| arena.intern(a.value(i))))
                .collect()
        }};
    }
    Ok(match values.data_type() {
        DataType::Utf8 => body!(StringArray),
        DataType::LargeUtf8 => body!(LargeStringArray),
        DataType::Utf8View => body!(StringViewArray),
        other => {
            return Err(ArrowIoError::UnsupportedType {
                column: String::from("<dictionary values>"),
                data_type: other.to_string(),
            })
        }
    })
}

// ---------------------------------------------------------------------------
// Import: in-RAM
// ---------------------------------------------------------------------------

/// An imported file's sheet plus what it cost.
#[derive(Debug)]
pub struct ImportedArrow {
    pub sheet: Sheet,
    pub stats: ArrowStats,
}

/// Read a Parquet file into an in-RAM [`Sheet`].
///
/// Still streams: batches arrive one at a time and are appended to the
/// columns. Peak *transient* memory is one batch; the retained `Sheet` is of
/// course proportional to the data, which is why the caller checks
/// [`crate::should_use_mmap`] first and routes big files to
/// [`convert_parquet`] instead.
pub fn import_parquet(path: &Path) -> Result<ImportedArrow, ArrowIoError> {
    use parquet::arrow::arrow_reader::ParquetRecordBatchReaderBuilder;

    let t = std::time::Instant::now();
    let file = File::open(path)?;
    let builder = ParquetRecordBatchReaderBuilder::try_new(file)?;
    let schema = builder.schema().clone();
    let reader = builder.with_batch_size(ROW_GROUP_ROWS).build()?;
    let name = sheet_name_for(path);
    import_batches(reader, &schema, name, t)
}

/// Read an Arrow IPC file (`.arrow` / Feather v2) into an in-RAM [`Sheet`].
///
/// Falls back to stream framing when the file has no IPC file footer, because
/// `.arrow` in the wild means both.
pub fn import_ipc(path: &Path) -> Result<ImportedArrow, ArrowIoError> {
    use arrow::ipc::reader::{FileReader, StreamReader};

    let t = std::time::Instant::now();
    let name = sheet_name_for(path);
    match FileReader::try_new(File::open(path)?, None) {
        Ok(r) => {
            let schema = r.schema();
            import_batches(r, &schema, name, t)
        }
        Err(_) => {
            let r = StreamReader::try_new(File::open(path)?, None)?;
            let schema = r.schema();
            import_batches(r, &schema, name, t)
        }
    }
}

/// Dispatch on extension. This is what the UI open path calls.
pub fn import_any(path: &Path) -> Result<ImportedArrow, ArrowIoError> {
    match format_for_path(path) {
        Some(ArrowFormat::Ipc) => import_ipc(path),
        // Default to Parquet: `import_any` is only reached for paths this
        // module claimed.
        _ => import_parquet(path),
    }
}

fn sheet_name_for(path: &Path) -> String {
    path.file_stem()
        .map(|s| s.to_string_lossy().into_owned())
        .filter(|s| !s.trim().is_empty())
        .unwrap_or_else(|| "Sheet1".to_string())
}

fn import_batches<I>(
    batches: I,
    schema: &Schema,
    name: String,
    t: std::time::Instant,
) -> Result<ImportedArrow, ArrowIoError>
where
    I: Iterator<Item = Result<RecordBatch, arrow::error::ArrowError>>,
{
    let kinds = classify_all(schema)?;
    let mut sheet = Sheet::new(name);
    sheet.set_headers(schema.fields().iter().map(|f| f.name().clone()).collect());
    let mut columns: Vec<Column> = (0..kinds.len()).map(|_| Column::new()).collect();

    let mut stripe = Stripe::default();
    let mut rows = 0usize;
    let mut batch_count = 0usize;

    for batch in batches {
        let batch = batch?;
        batch_count += 1;
        for (c, kind) in kinds.iter().enumerate() {
            decode_array(batch.column(c), *kind, &mut sheet.arena, &mut stripe.values)?;
            let col = &mut columns[c];
            for v in stripe.values.drain(..) {
                col.push(v);
            }
        }
        rows += batch.num_rows();
    }

    // A file with a schema but zero batches still has its columns, so the
    // sheet reports the right width rather than looking empty.
    for col in columns {
        sheet.push_column(col);
    }

    let distinct_strings = sheet.arena.len();
    Ok(ImportedArrow {
        sheet,
        stats: ArrowStats {
            rows,
            cols: kinds.len(),
            distinct_strings,
            batches: batch_count,
            millis: t.elapsed().as_millis(),
        },
    })
}

// ---------------------------------------------------------------------------
// Import: streaming to the columnar cache (files larger than RAM)
// ---------------------------------------------------------------------------

/// Stream a Parquet file into a `.ferrix` columnar cache, which the caller then
/// memory-maps.
///
/// This is the path that makes "a Parquet file larger than RAM opens" true
/// rather than aspirational. It holds:
///
/// * one row-group batch from the Parquet reader, and
/// * the string arena.
///
/// and nothing else. Everything else goes straight to per-column spill files
/// through [`crate::convert::Spill`] — the exact writer `convert.rs` and
/// `compact.rs` use, so there is one encoder for the on-disk format rather than
/// three that can drift.
pub fn convert_parquet<F>(
    source: &Path,
    dest: &Path,
    mut progress: F,
) -> Result<ConvertStats, ArrowIoError>
where
    F: FnMut(u64, u64),
{
    use parquet::arrow::arrow_reader::ParquetRecordBatchReaderBuilder;

    let t = std::time::Instant::now();
    let source_bytes = source.metadata().map(|m| m.len()).unwrap_or(0);

    let file = File::open(source)?;
    let builder = ParquetRecordBatchReaderBuilder::try_new(file)?;
    let schema = builder.schema().clone();
    let total_rows = builder.metadata().file_metadata().num_rows().max(0) as u64;
    let kinds = classify_all(&schema)?;
    let reader = builder.with_batch_size(ROW_GROUP_ROWS).build()?;

    let dir = dest
        .parent()
        .unwrap_or_else(|| Path::new("."))
        .to_path_buf();
    let scratch = ScratchDir::new(&dir)?;

    let mut spills = Vec::with_capacity(kinds.len());
    for i in 0..kinds.len() {
        spills.push(crate::convert::Spill::new(scratch.path(), i)?);
    }

    let mut arena = StringArena::new();
    let mut stripe = Stripe::default();
    let mut rows = 0u64;

    for batch in reader {
        let batch = batch?;
        for (c, kind) in kinds.iter().enumerate() {
            decode_array(batch.column(c), *kind, &mut arena, &mut stripe.values)?;
            let spill = &mut spills[c];
            for v in stripe.values.drain(..) {
                push_value(spill, v)?;
            }
        }
        rows += batch.num_rows() as u64;
        progress(rows, total_rows.max(rows));
    }

    let finished = spills
        .into_iter()
        .map(|s| s.finish())
        .collect::<Result<Vec<_>, _>>()?;
    let headers: Vec<String> = schema.fields().iter().map(|f| f.name().clone()).collect();
    let output_bytes = crate::convert::assemble(dest, &finished, &arena, rows, &headers)?;

    Ok(ConvertStats {
        rows,
        cols: kinds.len(),
        source_bytes,
        output_bytes,
        distinct_strings: arena.len(),
        millis: t.elapsed().as_millis(),
        // One decoded stripe, not the file. Reported so the status bar can
        // show the same "peak did not grow with the file" number the CSV
        // converter does.
        peak_block_bytes: stripe.values.capacity() * std::mem::size_of::<Value>(),
    })
}

fn push_value(spill: &mut crate::convert::Spill, v: Value) -> Result<(), ConvertError> {
    match v {
        Value::Empty => spill.push_empty(),
        Value::Number(n) => spill.push_number(n),
        Value::Bool(b) => spill.push_bool(b),
        Value::Text(id) => spill.push_text(id.0),
        Value::Error(e) => spill.push_error(e.to_code()),
    }
}

/// A temporary directory for spill files that removes itself, so a failed
/// import cannot leave gigabytes of `c0.tags` behind.
struct ScratchDir(PathBuf);

impl ScratchDir {
    fn new(near: &Path) -> std::io::Result<Self> {
        let p = near.join(format!(
            "ferrix-parquet-spill-{}-{:?}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.as_nanos())
                .unwrap_or(0)
        ));
        std::fs::create_dir_all(&p)?;
        Ok(Self(p))
    }
    fn path(&self) -> &Path {
        &self.0
    }
}

impl Drop for ScratchDir {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

// ---------------------------------------------------------------------------
// Export
// ---------------------------------------------------------------------------

/// Typed read access for the exporter.
///
/// Distinct from [`crate::export::ExportSource`], which yields display strings:
/// a Parquet column needs the *typed* value, and rendering to text first would
/// throw away exactly the type information the format exists to preserve.
pub trait ArrowSource {
    fn row_count(&self) -> usize;
    fn col_count(&self) -> usize;
    fn header(&self, col: usize) -> String;
    fn value(&self, cell: CellRef) -> Value;
    /// Resolve an interned id to its text.
    fn text(&self, id: StrId) -> String;
}

impl ArrowSource for Sheet {
    fn row_count(&self) -> usize {
        Sheet::row_count(self)
    }
    fn col_count(&self) -> usize {
        Sheet::col_count(self)
    }
    fn header(&self, col: usize) -> String {
        self.header_or_letter(col)
    }
    fn value(&self, cell: CellRef) -> Value {
        self.get(cell)
    }
    fn text(&self, id: StrId) -> String {
        self.resolve(id).to_string()
    }
}

impl ArrowSource for crate::MappedSheet {
    fn row_count(&self) -> usize {
        crate::MappedSheet::row_count(self)
    }
    fn col_count(&self) -> usize {
        crate::MappedSheet::col_count(self)
    }
    fn header(&self, col: usize) -> String {
        self.header_or_letter(col)
    }
    fn value(&self, cell: CellRef) -> Value {
        self.get(cell)
    }
    fn text(&self, id: StrId) -> String {
        crate::MappedSheet::resolve(self, id).to_string()
    }
}

/// The Parquet type one column will be written as.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ExportKind {
    Double,
    /// Int64 — chosen when every value in the column is integral, so a column
    /// of row ids does not land in pandas as `1.0`.
    Int64,
    Bool,
    Utf8,
    /// Excel serial -> `Timestamp(Millisecond)`. Only selected when the caller
    /// marks the column, because `Value` has no date type to infer from.
    Timestamp,
}

/// Export knobs.
#[derive(Debug, Clone, Default)]
pub struct ExportOptions {
    /// Column indices whose numbers are Excel date serials and should be
    /// written as `Timestamp(Millisecond)`.
    ///
    /// This has to be explicit: `Value::Number(45000.0)` is indistinguishable
    /// from a date, and guessing from the magnitude would silently turn a
    /// price column into 2023. The UI passes the columns whose *number format*
    /// is a date format.
    pub date_columns: Vec<usize>,
    /// Write column headers as Parquet field names. On by default via
    /// [`ExportOptions::default`] being empty + this bool defaulting false, so
    /// set it explicitly.
    pub use_headers: bool,
}

/// What an export produced.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ArrowExportStats {
    pub rows: usize,
    pub cols: usize,
    pub row_groups: usize,
    pub bytes: u64,
    pub millis: u128,
    /// Bytes of the largest single column stripe held at once. THIS is the
    /// number that must not grow with the file; it is reported so a test can
    /// assert on it rather than on a vague "it worked".
    pub peak_stripe_bytes: usize,
}

/// Decide each column's Parquet type with one bounded scan.
///
/// Scanning is O(rows) in time and O(1) in memory — we look at each cell and
/// keep four bools. That is the honest cost of a dynamic cell type meeting a
/// static column format; the alternative (write everything as text) would make
/// the export useless to every consumer.
///
/// Mixed columns fall back to `Utf8`, which is lossy in the sense that a number
/// comes back as text on re-import. That is reported through
/// [`ExportReport::mixed_columns`] rather than being silent.
fn plan_columns<S: ArrowSource + ?Sized>(
    src: &S,
    opts: &ExportOptions,
) -> (Vec<ExportKind>, Vec<usize>) {
    let rows = src.row_count();
    let mut kinds = Vec::with_capacity(src.col_count());
    let mut mixed = Vec::new();
    for c in 0..src.col_count() {
        if opts.date_columns.contains(&c) {
            kinds.push(ExportKind::Timestamp);
            continue;
        }
        let (mut num, mut boolean, mut text, mut err) = (false, false, false, false);
        let mut all_integral = true;
        for r in 0..rows {
            match src.value(CellRef {
                row: r as u32,
                col: c as u32,
            }) {
                Value::Empty => {}
                Value::Number(n) => {
                    num = true;
                    if !(n.fract() == 0.0 && n.abs() < 9.007_199_254_740_992e15) {
                        all_integral = false;
                    }
                }
                Value::Bool(_) => boolean = true,
                Value::Text(_) => text = true,
                // An error cell has no Parquet type. It is written as its
                // spreadsheet spelling ("#DIV/0!") in a text column, which is
                // what every other exporter here does.
                Value::Error(_) => err = true,
            }
        }
        let distinct = [num, boolean, text, err].iter().filter(|b| **b).count();
        if distinct > 1 {
            mixed.push(c);
            kinds.push(ExportKind::Utf8);
        } else if text || err {
            kinds.push(ExportKind::Utf8);
        } else if boolean {
            kinds.push(ExportKind::Bool);
        } else if num {
            kinds.push(if all_integral {
                ExportKind::Int64
            } else {
                ExportKind::Double
            });
        } else {
            // Entirely empty column. Utf8-of-all-nulls keeps the column
            // present with a name, which a Null type would too, but Utf8
            // survives more consumers.
            kinds.push(ExportKind::Utf8);
        }
    }
    (kinds, mixed)
}

/// Lossy-round-trip report, in the spirit of `rule_survives_xlsx`: the user
/// learns in the editor, not after opening the file in pandas.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ExportReport {
    /// Columns holding more than one Ferrix type, written as text.
    pub mixed_columns: Vec<usize>,
}

impl ExportReport {
    pub fn is_lossless(&self) -> bool {
        self.mixed_columns.is_empty()
    }
}

/// Build the Parquet schema for a plan.
fn parquet_schema(
    names: &[String],
    kinds: &[ExportKind],
) -> Result<std::sync::Arc<parquet::schema::types::Type>, ArrowIoError> {
    use parquet::basic::{LogicalType, Repetition, TimeUnit as PqTimeUnit, Type as PhysicalType};
    use parquet::format::MilliSeconds;
    use parquet::schema::types::Type as PqType;

    let mut fields = Vec::with_capacity(kinds.len());
    for (name, kind) in names.iter().zip(kinds) {
        let b =
            match kind {
                ExportKind::Double => PqType::primitive_type_builder(name, PhysicalType::DOUBLE)
                    .with_logical_type(None),
                ExportKind::Int64 => PqType::primitive_type_builder(name, PhysicalType::INT64)
                    .with_logical_type(None),
                ExportKind::Bool => PqType::primitive_type_builder(name, PhysicalType::BOOLEAN)
                    .with_logical_type(None),
                ExportKind::Utf8 => PqType::primitive_type_builder(name, PhysicalType::BYTE_ARRAY)
                    .with_logical_type(Some(LogicalType::String)),
                ExportKind::Timestamp => PqType::primitive_type_builder(name, PhysicalType::INT64)
                    .with_logical_type(Some(LogicalType::Timestamp {
                        is_adjusted_to_u_t_c: true,
                        unit: PqTimeUnit::MILLIS(MilliSeconds {}),
                    })),
            };
        // OPTIONAL, not REQUIRED: `Value::Empty` is a real null and writing it
        // as 0.0 or "" would corrupt the round trip.
        fields.push(std::sync::Arc::new(
            b.with_repetition(Repetition::OPTIONAL).build()?,
        ));
    }
    Ok(std::sync::Arc::new(
        PqType::group_type_builder("ferrix")
            .with_fields(fields)
            .build()?,
    ))
}

/// Write a sheet to Parquet, **one column chunk at a time**.
///
/// The loop shape is the load-bearing part:
///
/// ```text
/// for each row group (ROW_GROUP_ROWS rows):
///     for each column:
///         materialise THIS column's stripe        <- the only buffer
///         hand it to the column writer
///         drop it
/// ```
///
/// Peak is one stripe. A 200M-row, 20-column export holds 64K cells, not
/// 4 billion. This is why the low-level `SerializedFileWriter` is used instead
/// of `ArrowWriter`: the latter takes a whole `RecordBatch` (every column of
/// the row group at once), which would multiply the peak by the column count.
pub fn export_parquet<S: ArrowSource + ?Sized>(
    src: &S,
    dest: &Path,
    opts: &ExportOptions,
) -> Result<(ArrowExportStats, ExportReport), ArrowIoError> {
    use parquet::data_type::{BoolType, ByteArray, ByteArrayType, DoubleType, Int64Type};
    use parquet::file::properties::WriterProperties;
    use parquet::file::writer::SerializedFileWriter;

    let t = std::time::Instant::now();
    let rows = src.row_count();
    let cols = src.col_count();
    if cols == 0 {
        return Err(ArrowIoError::Empty { rows, cols });
    }

    let (kinds, mixed) = plan_columns(src, opts);
    let names: Vec<String> = if opts.use_headers {
        (0..cols).map(|c| sanitise(&src.header(c), c)).collect()
    } else {
        (0..cols)
            .map(|c| ferrix_core::column_name(c as u32))
            .collect()
    };
    let schema = parquet_schema(&names, &kinds)?;

    // Write to a temp file and rename, so a crash leaves the previous file
    // intact rather than a truncated one that looks complete — same discipline
    // as `export.rs`.
    let tmp = dest.with_extension("parquet.tmp");
    let props = std::sync::Arc::new(
        WriterProperties::builder()
            .set_compression(parquet::basic::Compression::SNAPPY)
            .build(),
    );
    let mut writer = SerializedFileWriter::new(BufWriter::new(File::create(&tmp)?), schema, props)?;

    let mut peak_stripe_bytes = 0usize;
    let mut row_groups = 0usize;

    // Reused buffers. Their capacity is the peak, and it stops growing after
    // the first full row group.
    let mut defs: Vec<i16> = Vec::with_capacity(ROW_GROUP_ROWS);
    let mut nums: Vec<f64> = Vec::new();
    let mut ints: Vec<i64> = Vec::new();
    let mut bools: Vec<bool> = Vec::new();
    let mut bytes: Vec<ByteArray> = Vec::new();

    let mut start = 0usize;
    while start < rows || (rows == 0 && row_groups == 0) {
        let end = (start + ROW_GROUP_ROWS).min(rows);
        let mut rg = writer.next_row_group()?;
        let mut c = 0usize;

        while let Some(mut col_writer) = rg.next_column()? {
            defs.clear();
            nums.clear();
            ints.clear();
            bools.clear();
            bytes.clear();

            // ---- materialise exactly ONE column stripe ----
            for r in start..end {
                let v = src.value(CellRef {
                    row: r as u32,
                    col: c as u32,
                });
                let present = !matches!(v, Value::Empty);
                defs.push(if present { 1 } else { 0 });
                if !present {
                    continue;
                }
                match kinds[c] {
                    ExportKind::Double => nums.push(match v {
                        Value::Number(n) => n,
                        Value::Bool(b) => {
                            if b {
                                1.0
                            } else {
                                0.0
                            }
                        }
                        _ => f64::NAN,
                    }),
                    ExportKind::Int64 => ints.push(match v {
                        Value::Number(n) => n as i64,
                        Value::Bool(b) => b as i64,
                        _ => 0,
                    }),
                    ExportKind::Timestamp => ints.push(match v {
                        Value::Number(n) => unix_ms_from_serial(n),
                        _ => 0,
                    }),
                    ExportKind::Bool => bools.push(matches!(v, Value::Bool(true))),
                    ExportKind::Utf8 => {
                        bytes.push(ByteArray::from(render_cell(src, v).into_bytes().as_slice()))
                    }
                }
            }

            match kinds[c] {
                ExportKind::Double => {
                    col_writer
                        .typed::<DoubleType>()
                        .write_batch(&nums, Some(&defs), None)?;
                }
                ExportKind::Int64 | ExportKind::Timestamp => {
                    col_writer
                        .typed::<Int64Type>()
                        .write_batch(&ints, Some(&defs), None)?;
                }
                ExportKind::Bool => {
                    col_writer
                        .typed::<BoolType>()
                        .write_batch(&bools, Some(&defs), None)?;
                }
                ExportKind::Utf8 => {
                    col_writer
                        .typed::<ByteArrayType>()
                        .write_batch(&bytes, Some(&defs), None)?;
                }
            }
            col_writer.close()?;

            peak_stripe_bytes =
                peak_stripe_bytes.max(stripe_bytes(&defs, &nums, &ints, &bools, &bytes));
            c += 1;
        }
        rg.close()?;
        row_groups += 1;
        start = end;
        if rows == 0 {
            break;
        }
    }

    writer.close()?;
    std::fs::rename(&tmp, dest)?;
    let bytes_written = dest.metadata()?.len();

    Ok((
        ArrowExportStats {
            rows,
            cols,
            row_groups,
            bytes: bytes_written,
            millis: t.elapsed().as_millis(),
            peak_stripe_bytes,
        },
        ExportReport {
            mixed_columns: mixed,
        },
    ))
}

fn stripe_bytes(
    defs: &[i16],
    nums: &[f64],
    ints: &[i64],
    bools: &[bool],
    bytes: &[parquet::data_type::ByteArray],
) -> usize {
    defs.len() * 2
        + nums.len() * 8
        + ints.len() * 8
        + bools.len()
        + bytes.iter().map(|b| b.len() + 16).sum::<usize>()
}

/// A cell as text, for a Utf8 column.
fn render_cell<S: ArrowSource + ?Sized>(src: &S, v: Value) -> String {
    match v {
        Value::Empty => String::new(),
        Value::Number(n) => ferrix_core::format_number(n),
        Value::Bool(b) => if b { "TRUE" } else { "FALSE" }.to_string(),
        Value::Text(id) => src.text(id),
        Value::Error(e) => e.to_string(),
    }
}

/// Parquet field names must be non-empty; fall back to the column letter.
fn sanitise(name: &str, col: usize) -> String {
    let n = name.trim();
    if n.is_empty() {
        ferrix_core::column_name(col as u32)
    } else {
        n.to_string()
    }
}

/// Write a sheet to an Arrow IPC file.
///
/// Batched at [`ROW_GROUP_ROWS`] rows for the same reason: peak is one batch.
/// Unlike Parquet this genuinely does need every column of a batch live at
/// once — that is what a `RecordBatch` is — so the bound here is
/// `ROW_GROUP_ROWS * cols`, still independent of row count.
pub fn export_ipc<S: ArrowSource + ?Sized>(
    src: &S,
    dest: &Path,
    opts: &ExportOptions,
) -> Result<(ArrowExportStats, ExportReport), ArrowIoError> {
    use arrow::array::{
        ArrayRef, BooleanArray, Float64Array, Int64Array, StringArray, TimestampMillisecondArray,
    };
    use arrow::ipc::writer::FileWriter;

    let t = std::time::Instant::now();
    let rows = src.row_count();
    let cols = src.col_count();
    if cols == 0 {
        return Err(ArrowIoError::Empty { rows, cols });
    }
    let (kinds, mixed) = plan_columns(src, opts);
    let names: Vec<String> = if opts.use_headers {
        (0..cols).map(|c| sanitise(&src.header(c), c)).collect()
    } else {
        (0..cols)
            .map(|c| ferrix_core::column_name(c as u32))
            .collect()
    };

    let fields: Vec<Field> = names
        .iter()
        .zip(&kinds)
        .map(|(n, k)| {
            let dt = match k {
                ExportKind::Double => DataType::Float64,
                ExportKind::Int64 => DataType::Int64,
                ExportKind::Bool => DataType::Boolean,
                ExportKind::Utf8 => DataType::Utf8,
                ExportKind::Timestamp => DataType::Timestamp(TimeUnit::Millisecond, None),
            };
            Field::new(n, dt, true)
        })
        .collect();
    let schema = std::sync::Arc::new(Schema::new(fields));

    let tmp = dest.with_extension("arrow.tmp");
    let mut writer = FileWriter::try_new(BufWriter::new(File::create(&tmp)?), &schema)?;
    let mut peak_stripe_bytes = 0usize;
    let mut batches = 0usize;

    let mut start = 0usize;
    while start < rows {
        let end = (start + ROW_GROUP_ROWS).min(rows);
        let n = end - start;
        let mut arrays: Vec<ArrayRef> = Vec::with_capacity(cols);
        let mut stripe = 0usize;
        for (c, kind) in kinds.iter().enumerate() {
            let cell = |r: usize| {
                src.value(CellRef {
                    row: r as u32,
                    col: c as u32,
                })
            };
            let a: ArrayRef = match kind {
                ExportKind::Double => {
                    stripe += n * 8;
                    std::sync::Arc::new(
                        (start..end)
                            .map(|r| match cell(r) {
                                Value::Number(v) => Some(v),
                                Value::Bool(b) => Some(if b { 1.0 } else { 0.0 }),
                                _ => None,
                            })
                            .collect::<Float64Array>(),
                    )
                }
                ExportKind::Int64 => {
                    stripe += n * 8;
                    std::sync::Arc::new(
                        (start..end)
                            .map(|r| match cell(r) {
                                Value::Number(v) => Some(v as i64),
                                Value::Bool(b) => Some(b as i64),
                                _ => None,
                            })
                            .collect::<Int64Array>(),
                    )
                }
                ExportKind::Timestamp => {
                    stripe += n * 8;
                    std::sync::Arc::new(
                        (start..end)
                            .map(|r| match cell(r) {
                                Value::Number(v) => Some(unix_ms_from_serial(v)),
                                _ => None,
                            })
                            .collect::<TimestampMillisecondArray>(),
                    )
                }
                ExportKind::Bool => {
                    stripe += n;
                    std::sync::Arc::new(
                        (start..end)
                            .map(|r| match cell(r) {
                                Value::Bool(b) => Some(b),
                                _ => None,
                            })
                            .collect::<BooleanArray>(),
                    )
                }
                ExportKind::Utf8 => {
                    let v: Vec<Option<String>> = (start..end)
                        .map(|r| match cell(r) {
                            Value::Empty => None,
                            other => Some(render_cell(src, other)),
                        })
                        .collect();
                    stripe += v
                        .iter()
                        .map(|s| s.as_ref().map_or(0, |x| x.len() + 24))
                        .sum::<usize>();
                    std::sync::Arc::new(v.into_iter().collect::<StringArray>())
                }
            };
            arrays.push(a);
        }
        peak_stripe_bytes = peak_stripe_bytes.max(stripe);
        writer.write(&RecordBatch::try_new(schema.clone(), arrays)?)?;
        batches += 1;
        start = end;
    }
    writer.finish()?;
    drop(writer);
    std::fs::rename(&tmp, dest)?;
    let bytes_written = dest.metadata()?.len();

    Ok((
        ArrowExportStats {
            rows,
            cols,
            row_groups: batches,
            bytes: bytes_written,
            millis: t.elapsed().as_millis(),
            peak_stripe_bytes,
        },
        ExportReport {
            mixed_columns: mixed,
        },
    ))
}

#[cfg(test)]
mod tests;
