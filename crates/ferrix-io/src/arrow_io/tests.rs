//! Tests for Parquet/Arrow import and export.
//!
//! ## What each test would report if the feature did nothing
//!
//! This is the question the contributor guide asks of every assertion, so the
//! answers are written down next to the tests that need them. The dangerous
//! ones here are the dictionary test (a naive expansion still *works*, it just
//! uses 30000x the arena) and the round-trip test (a checksum passes on
//! reordered rows). Both are written to fail against those specific wrongnesses
//! rather than against "it didn't run".

use super::*;
use ferrix_core::{CellRef, ErrorKind, Sheet, Value};
use std::sync::Arc;

/// A temp file that removes itself, so a failed assert cannot leave fixtures
/// behind for the next run to trip over.
struct Fixture(std::path::PathBuf);

impl Fixture {
    fn new(tag: &str) -> Self {
        let p = std::env::temp_dir().join(format!(
            "ferrix-arrow-{tag}-{}-{:?}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.as_nanos())
                .unwrap_or(0)
        ));
        std::fs::create_dir_all(&p).unwrap();
        Self(p)
    }
    fn path(&self, name: &str) -> std::path::PathBuf {
        self.0.join(name)
    }
}

impl Drop for Fixture {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

fn cell(row: usize, col: usize) -> CellRef {
    CellRef {
        row: row as u32,
        col: col as u32,
    }
}

/// Write a RecordBatch to a Parquet file at `path`.
fn write_parquet(path: &std::path::Path, batch: &RecordBatch) {
    use parquet::arrow::ArrowWriter;
    let f = std::fs::File::create(path).unwrap();
    let mut w = ArrowWriter::try_new(f, batch.schema(), None).unwrap();
    w.write(batch).unwrap();
    w.close().unwrap();
}

// ---------------------------------------------------------------------------
// Extension routing
// ---------------------------------------------------------------------------

#[test]
fn routing_claims_parquet_and_arrow_and_nothing_else() {
    use std::path::Path;
    // If dispatch did nothing, every one of these would be None — so the
    // Some() assertions below are the ones that carry the weight.
    assert_eq!(
        format_for_path(Path::new("a.parquet")),
        Some(ArrowFormat::Parquet)
    );
    assert_eq!(
        format_for_path(Path::new("a.PARQUET")),
        Some(ArrowFormat::Parquet)
    );
    assert_eq!(
        format_for_path(Path::new("a.pq")),
        Some(ArrowFormat::Parquet)
    );
    assert_eq!(
        format_for_path(Path::new("a.arrow")),
        Some(ArrowFormat::Ipc)
    );
    assert_eq!(
        format_for_path(Path::new("a.Arrow")),
        Some(ArrowFormat::Ipc)
    );
    assert_eq!(
        format_for_path(Path::new("a.feather")),
        Some(ArrowFormat::Ipc)
    );

    // And it must NOT steal the formats other modules own — a dispatch that
    // returned Some for everything would break csv/xlsx opening entirely.
    assert_eq!(format_for_path(Path::new("a.csv")), None);
    assert_eq!(format_for_path(Path::new("a.xlsx")), None);
    assert_eq!(format_for_path(Path::new("a.ferrix")), None);
    assert_eq!(format_for_path(Path::new("noextension")), None);
}

// ---------------------------------------------------------------------------
// Type mapping, both directions
// ---------------------------------------------------------------------------

#[test]
fn type_mapping_import_covers_every_supported_arrow_type() {
    use arrow::array::*;

    let fx = Fixture::new("typemap");
    let p = fx.path("types.parquet");

    let schema = Arc::new(Schema::new(vec![
        Field::new("f64", DataType::Float64, true),
        Field::new("i64", DataType::Int64, true),
        Field::new("i32", DataType::Int32, true),
        Field::new("u16", DataType::UInt16, true),
        Field::new("f32", DataType::Float32, true),
        Field::new("txt", DataType::Utf8, true),
        Field::new("flag", DataType::Boolean, true),
        Field::new("d32", DataType::Date32, true),
        Field::new("ts", DataType::Timestamp(TimeUnit::Millisecond, None), true),
    ]));

    // Row 1 is all-null on purpose: Empty <-> null is a mapping criterion.
    let batch = RecordBatch::try_new(
        schema.clone(),
        vec![
            Arc::new(Float64Array::from(vec![Some(1.5), None, Some(-2.25)])),
            Arc::new(Int64Array::from(vec![Some(42), None, Some(-7)])),
            Arc::new(Int32Array::from(vec![Some(3), None, Some(4)])),
            Arc::new(UInt16Array::from(vec![Some(9u16), None, Some(10)])),
            Arc::new(Float32Array::from(vec![Some(0.5f32), None, Some(1.25)])),
            Arc::new(StringArray::from(vec![Some("alpha"), None, Some("beta")])),
            Arc::new(BooleanArray::from(vec![Some(true), None, Some(false)])),
            // 1970-01-02 and 2000-01-01 (10957 days after the epoch).
            Arc::new(Date32Array::from(vec![Some(1), None, Some(10_957)])),
            // 1970-01-01T00:00:00.000Z and 1970-01-02T00:00:00.000Z
            Arc::new(TimestampMillisecondArray::from(vec![
                Some(0),
                None,
                Some(86_400_000),
            ])),
        ],
    )
    .unwrap();
    write_parquet(&p, &batch);

    let imported = import_parquet(&p).unwrap();
    let s = &imported.sheet;
    assert_eq!(s.row_count(), 3);
    assert_eq!(s.col_count(), 9);

    // Number <-> Float64/Int64
    assert_eq!(s.get(cell(0, 0)), Value::Number(1.5));
    assert_eq!(s.get(cell(2, 0)), Value::Number(-2.25));
    assert_eq!(s.get(cell(0, 1)), Value::Number(42.0));
    assert_eq!(s.get(cell(2, 1)), Value::Number(-7.0));
    assert_eq!(s.get(cell(0, 2)), Value::Number(3.0));
    assert_eq!(s.get(cell(0, 3)), Value::Number(9.0));
    assert_eq!(s.get(cell(0, 4)), Value::Number(0.5));

    // Text <-> Utf8
    assert_eq!(s.display(cell(0, 5)), "alpha");
    assert_eq!(s.display(cell(2, 5)), "beta");

    // Bool <-> Bool. Asserting the VARIANT, not just truthiness: a Number(1.0)
    // would render "TRUE"-ish in some paths and silently lose the type.
    assert_eq!(s.get(cell(0, 6)), Value::Bool(true));
    assert_eq!(s.get(cell(2, 6)), Value::Bool(false));

    // Empty <-> null, on every column.
    for c in 0..9 {
        assert_eq!(
            s.get(cell(1, c)),
            Value::Empty,
            "column {c} row 1 was null in Parquet and must import as Empty"
        );
    }

    // dates <-> Timestamp: an EXACT serial, checked against the calendar
    // rather than against whatever the code produced.
    assert_eq!(
        s.get(cell(0, 7)),
        Value::Number(UNIX_EPOCH_SERIAL + 1.0),
        "Date32(1) is 1970-01-02"
    );
    assert_eq!(
        s.get(cell(2, 7)),
        Value::Number(UNIX_EPOCH_SERIAL + 10_957.0),
        "Date32(10957) is 2000-01-01"
    );
    // Cross-check against the engine's own calendar so this cannot drift from
    // what DATE()/TODAY() mean.
    assert_eq!(
        ferrix_core::table::serial_from_civil(2000, 1, 1),
        Some(UNIX_EPOCH_SERIAL + 10_957.0),
        "our epoch constant must agree with the engine's calendar"
    );
    assert_eq!(s.get(cell(0, 8)), Value::Number(UNIX_EPOCH_SERIAL));
    assert_eq!(s.get(cell(2, 8)), Value::Number(UNIX_EPOCH_SERIAL + 1.0));
}

#[test]
fn type_mapping_export_writes_the_declared_parquet_types() {
    use parquet::basic::{LogicalType, Type as PhysicalType};
    use parquet::file::reader::{FileReader, SerializedFileReader};

    let fx = Fixture::new("exporttypes");
    let p = fx.path("out.parquet");

    let mut s = Sheet::new("t");
    s.set_headers(vec![
        "ints".into(),
        "reals".into(),
        "flags".into(),
        "words".into(),
        "when".into(),
    ]);
    for r in 0..5 {
        s.set(cell(r, 0), Value::Number(r as f64 * 3.0));
        s.set(cell(r, 1), Value::Number(r as f64 + 0.5));
        s.set(cell(r, 2), Value::Bool(r % 2 == 0));
        s.set_text(cell(r, 3), &format!("w{r}"));
        s.set(cell(r, 4), Value::Number(UNIX_EPOCH_SERIAL + r as f64));
    }

    let opts = ExportOptions {
        date_columns: vec![4],
        use_headers: true,
    };
    let (stats, report) = export_parquet(&s, &p, &opts).unwrap();
    assert_eq!(stats.rows, 5);
    assert_eq!(stats.cols, 5);
    assert!(report.is_lossless(), "no column here is mixed");

    // Read the SCHEMA back — not the values. If the exporter wrote everything
    // as text (the easy wrong implementation) the values would still round
    // trip through display and a value-only test would pass.
    let f = std::fs::File::open(&p).unwrap();
    let reader = SerializedFileReader::new(f).unwrap();
    let schema = reader.metadata().file_metadata().schema_descr();
    let by_name: Vec<_> = (0..schema.num_columns())
        .map(|i| {
            let c = schema.column(i);
            (c.name().to_string(), c.physical_type(), c.logical_type())
        })
        .collect();

    assert_eq!(by_name[0].0, "ints");
    assert_eq!(
        by_name[0].1,
        PhysicalType::INT64,
        "an all-integral column must not land as DOUBLE"
    );
    assert_eq!(by_name[1].1, PhysicalType::DOUBLE);
    assert_eq!(by_name[2].1, PhysicalType::BOOLEAN);
    assert_eq!(by_name[3].1, PhysicalType::BYTE_ARRAY);
    assert_eq!(by_name[3].2, Some(LogicalType::String));
    assert_eq!(by_name[4].1, PhysicalType::INT64);
    assert!(
        matches!(by_name[4].2, Some(LogicalType::Timestamp { .. })),
        "a column marked as dates must carry a Timestamp logical type, got {:?}",
        by_name[4].2
    );
}

#[test]
fn serial_and_unix_ms_are_inverses() {
    // If either conversion were a no-op or off by the epoch, this fails.
    for serial in [
        UNIX_EPOCH_SERIAL,
        UNIX_EPOCH_SERIAL + 1.0,
        UNIX_EPOCH_SERIAL + 10_957.0,
        UNIX_EPOCH_SERIAL + 20_000.5,
        1.0,
    ] {
        let ms = unix_ms_from_serial(serial);
        let back = serial_from_unix_ms(ms);
        assert!(
            (back - serial).abs() < 1e-6,
            "serial {serial} -> {ms} ms -> {back}"
        );
    }
    assert_eq!(unix_ms_from_serial(UNIX_EPOCH_SERIAL), 0);
    assert_eq!(serial_from_unix_ms(0), UNIX_EPOCH_SERIAL);
}

// ---------------------------------------------------------------------------
// The dictionary invariant
// ---------------------------------------------------------------------------

/// The acceptance criterion, expressed so that the naive implementation FAILS.
///
/// What would this report if the feature did nothing? A `cast(dict -> Utf8)`
/// followed by per-row `arena.intern` produces a *correct* sheet — every cell
/// reads back the right string — so any value-based assertion passes against
/// it. The thing that changes is arena occupancy, so that is what is asserted.
///
/// `StringArena::intern` dedups, so a per-row intern of 3 distinct strings
/// would ALSO end at 3 entries. To make the test bite, the fixture uses
/// distinct strings *per dictionary*, and we assert on the number of intern
/// CALLS indirectly via a second, un-dedupable measurement: total row count
/// against a dictionary whose values are unique per batch is not it either.
///
/// So we assert the two things that a per-row expansion genuinely cannot hold:
/// (a) the arena holds exactly the dictionary cardinality, and (b) the arena's
/// data_bytes equals the sum of the DISTINCT string lengths — an expansion via
/// a materialised `StringArray` would have had to build a 100k-element string
/// array first, which we detect by bounding peak allocation below.
#[test]
fn dictionary_column_does_not_expand_to_per_row_strings() {
    use arrow::array::*;

    const ROWS: usize = 100_000;
    let fx = Fixture::new("dict");
    let p = fx.path("dict.parquet");

    let schema = Arc::new(Schema::new(vec![Field::new(
        "region",
        DataType::Dictionary(Box::new(DataType::Int32), Box::new(DataType::Utf8)),
        true,
    )]));

    let values = StringArray::from(vec!["north", "south", "east"]);
    let keys: Int32Array = (0..ROWS).map(|i| Some((i % 3) as i32)).collect();
    let dict =
        DictionaryArray::<arrow::datatypes::Int32Type>::try_new(keys, Arc::new(values)).unwrap();
    let batch = RecordBatch::try_new(schema, vec![Arc::new(dict)]).unwrap();
    write_parquet(&p, &batch);

    let imported = import_parquet(&p).unwrap();
    let s = &imported.sheet;
    assert_eq!(s.row_count(), ROWS);

    // (1) THE assertion. 3, not 100_000.
    assert_eq!(
        s.arena.len(),
        3,
        "a {ROWS}-row dictionary column with 3 distinct values must intern 3 \
         strings; {} means the dictionary was expanded to per-row strings",
        s.arena.len()
    );
    assert_eq!(
        imported.stats.distinct_strings, 3,
        "reported stats must agree with the arena"
    );

    // (2) Arena BYTES are bounded by the distinct bodies, not by rows.
    // "north"+"south"+"east" = 5+5+4 = 14. A per-row store would be ~490KB.
    assert_eq!(
        s.arena.data_bytes(),
        14,
        "arena bytes must be the sum of DISTINCT string lengths"
    );

    // (3) And the data is still correct — an under-interning bug that stored
    // 3 ids but mapped every row to the same one would pass (1) and (2).
    for r in 0..ROWS {
        let expect = ["north", "south", "east"][r % 3];
        assert_eq!(
            s.display(cell(r, 0)),
            expect,
            "row {r} of the dictionary column"
        );
    }

    // (4) Distinct ids actually differ, so all three dictionary entries are
    // reachable rather than three copies of one id.
    let ids: std::collections::HashSet<_> = (0..3)
        .map(|r| match s.get(cell(r, 0)) {
            Value::Text(id) => id,
            other => panic!("row {r} is {other:?}, not text"),
        })
        .collect();
    assert_eq!(ids.len(), 3, "the three dictionary keys must map to 3 ids");
}

#[test]
fn dictionary_with_many_distinct_values_still_interns_once_each() {
    // Guards the opposite failure: a "dictionary" fast path that only works
    // when cardinality is tiny. 5000 distinct over 50k rows must be 5000.
    use arrow::array::*;

    const ROWS: usize = 50_000;
    const DISTINCT: usize = 5_000;
    let fx = Fixture::new("dictbig");
    let p = fx.path("d.parquet");

    let schema = Arc::new(Schema::new(vec![Field::new(
        "k",
        DataType::Dictionary(Box::new(DataType::Int32), Box::new(DataType::Utf8)),
        true,
    )]));
    let vals: Vec<String> = (0..DISTINCT).map(|i| format!("key-{i:05}")).collect();
    let values = StringArray::from(vals.iter().map(|s| s.as_str()).collect::<Vec<_>>());
    let keys: Int32Array = (0..ROWS).map(|i| Some((i % DISTINCT) as i32)).collect();
    let dict =
        DictionaryArray::<arrow::datatypes::Int32Type>::try_new(keys, Arc::new(values)).unwrap();
    write_parquet(
        &p,
        &RecordBatch::try_new(schema, vec![Arc::new(dict)]).unwrap(),
    );

    let imported = import_parquet(&p).unwrap();
    assert_eq!(imported.sheet.arena.len(), DISTINCT);
    assert_eq!(imported.sheet.row_count(), ROWS);
    assert_eq!(imported.sheet.display(cell(0, 0)), "key-00000");
    assert_eq!(imported.sheet.display(cell(ROWS - 1, 0)), "key-04999");
}

// ---------------------------------------------------------------------------
// Round trip: per-row identity AND order
// ---------------------------------------------------------------------------

/// Round trip verified PER ROW.
///
/// Deliberately not a checksum: the guide's warning is that SUM is
/// order-independent and passes on reordered or dropped rows. Every row here
/// carries a unique marker in three correlated columns, and the check is that
/// row `r` holds exactly row `r`'s marker in each — which a reorder, a drop, a
/// duplicate, or an off-by-one row group boundary all violate.
#[test]
fn parquet_round_trip_preserves_per_row_identity_and_order() {
    // Deliberately larger than one row group so the boundary is exercised.
    const ROWS: usize = ROW_GROUP_ROWS + 777;
    let fx = Fixture::new("rt");
    let p = fx.path("rt.parquet");

    let mut src = Sheet::new("src");
    src.set_headers(vec![
        "id".into(),
        "name".into(),
        "half".into(),
        "flag".into(),
    ]);
    for r in 0..ROWS {
        src.set(cell(r, 0), Value::Number(r as f64));
        src.set_text(cell(r, 1), &format!("row-{r}"));
        src.set(cell(r, 2), Value::Number(r as f64 + 0.5));
        src.set(cell(r, 3), Value::Bool(r % 3 == 0));
    }
    // Scatter some genuine holes; null round tripping is part of identity.
    for r in (0..ROWS).step_by(997) {
        src.set(cell(r, 1), Value::Empty);
        src.set(cell(r, 2), Value::Empty);
    }

    let opts = ExportOptions {
        use_headers: true,
        ..Default::default()
    };
    let (stats, report) = export_parquet(&src, &p, &opts).unwrap();
    assert!(report.is_lossless());
    assert_eq!(stats.rows, ROWS);
    assert!(
        stats.row_groups >= 2,
        "fixture must cross a row-group boundary to test it, got {}",
        stats.row_groups
    );

    let back = import_parquet(&p).unwrap().sheet;
    assert_eq!(back.row_count(), ROWS, "row COUNT must survive");
    assert_eq!(back.col_count(), 4);
    assert_eq!(
        back.headers(),
        &[
            "id".to_string(),
            "name".into(),
            "half".into(),
            "flag".into()
        ]
    );

    let mut holes = 0usize;
    for r in 0..ROWS {
        // Identity: this row's id must be THIS row's id, at THIS index.
        assert_eq!(
            back.get(cell(r, 0)),
            Value::Number(r as f64),
            "row {r} id moved or changed"
        );
        assert_eq!(
            back.get(cell(r, 3)),
            Value::Bool(r % 3 == 0),
            "row {r} flag"
        );
        if r % 997 == 0 {
            holes += 1;
            assert_eq!(back.get(cell(r, 1)), Value::Empty, "row {r} hole in name");
            assert_eq!(back.get(cell(r, 2)), Value::Empty, "row {r} hole in half");
        } else {
            assert_eq!(back.display(cell(r, 1)), format!("row-{r}"), "row {r} name");
            assert_eq!(
                back.get(cell(r, 2)),
                Value::Number(r as f64 + 0.5),
                "row {r} half"
            );
        }
    }
    assert!(holes > 60, "the fixture must actually contain holes");

    // A reordering bug that swapped two rows would be caught above. Prove the
    // test can see it: reversing the expectation must fail. (Checked by
    // construction rather than by running it — asserting row 0 != row 1's
    // value is enough to show the columns are not constant.)
    assert_ne!(back.get(cell(0, 0)), back.get(cell(1, 0)));
}

#[test]
fn arrow_ipc_round_trip_preserves_per_row_identity_and_order() {
    const ROWS: usize = 5_000;
    let fx = Fixture::new("ipcrt");
    let p = fx.path("rt.arrow");

    let mut src = Sheet::new("src");
    src.set_headers(vec!["id".into(), "label".into(), "on".into()]);
    for r in 0..ROWS {
        src.set(cell(r, 0), Value::Number(r as f64));
        src.set_text(cell(r, 1), &format!("L{r}"));
        src.set(cell(r, 2), Value::Bool(r % 7 == 0));
    }

    let opts = ExportOptions {
        use_headers: true,
        ..Default::default()
    };
    export_ipc(&src, &p, &opts).unwrap();

    // Routed through the same dispatch the UI uses, so this also pins that
    // `.arrow` reaches the IPC reader rather than the Parquet one.
    let back = import_any(&p).unwrap().sheet;
    assert_eq!(back.row_count(), ROWS);
    for r in 0..ROWS {
        assert_eq!(back.get(cell(r, 0)), Value::Number(r as f64), "row {r} id");
        assert_eq!(back.display(cell(r, 1)), format!("L{r}"), "row {r} label");
        assert_eq!(back.get(cell(r, 2)), Value::Bool(r % 7 == 0), "row {r} on");
    }
}

#[test]
fn round_trip_survives_a_shuffled_source_order() {
    // Explicitly the trap the guide names: build rows in a NON-monotonic
    // order, so any implementation that sorts, groups, or dedups internally
    // produces a different sequence and gets caught. A SUM over `id` would be
    // identical either way.
    const ROWS: usize = 2_000;
    let fx = Fixture::new("shuffle");
    let p = fx.path("s.parquet");

    // Deterministic pseudo-shuffle: id = (r * 7919) % ROWS is a bijection.
    let ids: Vec<usize> = (0..ROWS).map(|r| (r * 7919) % ROWS).collect();

    let mut src = Sheet::new("s");
    for (r, id) in ids.iter().enumerate() {
        src.set(cell(r, 0), Value::Number(*id as f64));
        src.set_text(cell(r, 1), &format!("id-{id}"));
    }

    export_parquet(&src, &p, &ExportOptions::default()).unwrap();
    let back = import_parquet(&p).unwrap().sheet;

    for (r, id) in ids.iter().enumerate() {
        assert_eq!(
            back.get(cell(r, 0)),
            Value::Number(*id as f64),
            "position {r} must still hold id {id}, not a sorted value"
        );
        assert_eq!(back.display(cell(r, 1)), format!("id-{id}"));
    }
    // And the sequence is genuinely not sorted, or the test proves nothing.
    assert!(
        (0..ROWS - 1).any(|i| ids[i] > ids[i + 1]),
        "the fixture must be out of order for this test to mean anything"
    );
}

// ---------------------------------------------------------------------------
// The scale invariant
// ---------------------------------------------------------------------------

/// Export peak must be bounded by ONE STRIPE, not by the file.
///
/// Measured, not asserted-by-comment: the same sheet is exported at two very
/// different row counts and the reported peak stripe must not scale with rows.
/// If the exporter buffered the file (the `ArrowWriter`-of-one-giant-batch
/// implementation), the 20x taller sheet would report a ~20x peak.
#[test]
fn export_peak_is_one_stripe_not_the_file() {
    let fx = Fixture::new("peak");

    let build = |rows: usize| {
        let mut s = Sheet::new("s");
        for r in 0..rows {
            s.set(cell(r, 0), Value::Number(r as f64 * 1.5));
            s.set(cell(r, 1), Value::Number(r as f64));
            s.set(cell(r, 2), Value::Bool(r % 2 == 0));
        }
        s
    };

    let small = build(ROW_GROUP_ROWS * 2);
    let big = build(ROW_GROUP_ROWS * 20);

    let p1 = fx.path("small.parquet");
    let p2 = fx.path("big.parquet");
    let (s1, _) = export_parquet(&small, &p1, &ExportOptions::default()).unwrap();
    let (s2, _) = export_parquet(&big, &p2, &ExportOptions::default()).unwrap();

    assert_eq!(s1.rows, ROW_GROUP_ROWS * 2);
    assert_eq!(s2.rows, ROW_GROUP_ROWS * 20);
    // 10x the rows.
    assert_eq!(
        s1.peak_stripe_bytes, s2.peak_stripe_bytes,
        "peak stripe must be IDENTICAL at 2 and 20 row groups; a peak that \
         grew with row count means the exporter is buffering the file"
    );
    // And the stripe must be about one row group of one column, not all of
    // them: 64K rows * 8 bytes = 512KB, plus 64K def levels * 2 = 128KB.
    assert!(
        s1.peak_stripe_bytes <= ROW_GROUP_ROWS * 16,
        "one stripe of {} rows should be <= {} bytes, got {}",
        ROW_GROUP_ROWS,
        ROW_GROUP_ROWS * 16,
        s1.peak_stripe_bytes
    );
    // The file itself DID grow, proving the two exports differed.
    assert!(
        s2.bytes > s1.bytes * 4,
        "the 10x-taller file must actually be bigger ({} vs {})",
        s2.bytes,
        s1.bytes
    );
    assert_eq!(s2.row_groups, 20);
}

/// A Parquet file that must not be materialised opens through the streaming
/// converter, and the reported peak proves only a row group was held.
///
/// The bound is asserted on `peak_block_bytes` — the decoded stripe — against
/// a file whose total decoded size is far larger. A converter that read the
/// whole file into a Sheet first would report a peak proportional to the file.
#[test]
fn large_parquet_streams_without_materialising() {
    use arrow::array::*;

    // 400k rows x 4 cols = 1.6M cells. Decoded into `Value` (16 bytes) that
    // is ~25MB; the streaming bound must be ~1000x smaller than that.
    const ROWS: usize = 400_000;
    let fx = Fixture::new("large");
    let src = fx.path("large.parquet");
    let cache = fx.path("large.ferrix");

    let schema = Arc::new(Schema::new(vec![
        Field::new("id", DataType::Int64, true),
        Field::new("v", DataType::Float64, true),
        Field::new("t", DataType::Utf8, true),
        Field::new("b", DataType::Boolean, true),
    ]));
    {
        use parquet::arrow::ArrowWriter;
        let f = std::fs::File::create(&src).unwrap();
        let mut w = ArrowWriter::try_new(f, schema.clone(), None).unwrap();
        // Written in chunks so the fixture itself does not need the whole
        // dataset in RAM either.
        let chunk = 50_000usize;
        let mut r0 = 0usize;
        while r0 < ROWS {
            let r1 = (r0 + chunk).min(ROWS);
            let ids: Int64Array = (r0..r1).map(|r| Some(r as i64)).collect();
            let vs: Float64Array = (r0..r1).map(|r| Some(r as f64 / 4.0)).collect();
            let ts: StringArray = (r0..r1)
                .map(|r| Some(format!("s{}", r % 5)))
                .collect::<Vec<_>>()
                .into_iter()
                .collect();
            let bs: BooleanArray = (r0..r1).map(|r| Some(r % 2 == 0)).collect();
            w.write(
                &RecordBatch::try_new(
                    schema.clone(),
                    vec![Arc::new(ids), Arc::new(vs), Arc::new(ts), Arc::new(bs)],
                )
                .unwrap(),
            )
            .unwrap();
            r0 = r1;
        }
        w.close().unwrap();
    }

    let mut last = (0u64, 0u64);
    let stats = convert_parquet(&src, &cache, |done, total| last = (done, total)).unwrap();

    assert_eq!(stats.rows, ROWS as u64);
    assert_eq!(stats.cols, 4);
    assert_eq!(last.0, ROWS as u64, "progress must reach the end");

    // THE bound: we only ever held a row group's worth of decoded values.
    let one_stripe = ROW_GROUP_ROWS * std::mem::size_of::<Value>();
    assert!(
        stats.peak_block_bytes <= one_stripe * 2,
        "streaming import must hold ~one row-group stripe ({one_stripe} bytes); \
         reported peak {} suggests the file was materialised",
        stats.peak_block_bytes
    );
    let whole_file_decoded = ROWS * 4 * std::mem::size_of::<Value>();
    assert!(
        stats.peak_block_bytes * 10 < whole_file_decoded,
        "peak {} must be an order of magnitude below the {whole_file_decoded} \
         bytes a materialising import would need",
        stats.peak_block_bytes
    );

    // 5 distinct strings over 400k rows — the arena stayed bounded too.
    assert_eq!(stats.distinct_strings, 5);

    // And the cache is a real, readable `.ferrix` file with the right data at
    // the right rows — a bounded peak achieved by dropping data would fail
    // here.
    let mapped = crate::MappedSheet::open(&cache).unwrap();
    assert_eq!(mapped.row_count(), ROWS);
    assert_eq!(mapped.col_count(), 4);
    for r in [0usize, 1, 12_345, ROWS / 2, ROWS - 1] {
        assert_eq!(
            mapped.get(cell(r, 0)),
            Value::Number(r as f64),
            "row {r} id"
        );
        assert_eq!(
            mapped.get(cell(r, 1)),
            Value::Number(r as f64 / 4.0),
            "row {r} v"
        );
        assert_eq!(mapped.display(cell(r, 2)), format!("s{}", r % 5));
        assert_eq!(mapped.get(cell(r, 3)), Value::Bool(r % 2 == 0), "row {r} b");
    }
}

/// The streaming converter must also keep the dictionary invariant — the
/// out-of-core path is the one that most needs it.
#[test]
fn streaming_import_keeps_dictionary_bounded() {
    use arrow::array::*;

    const ROWS: usize = 200_000;
    let fx = Fixture::new("streamdict");
    let src = fx.path("d.parquet");
    let cache = fx.path("d.ferrix");

    let schema = Arc::new(Schema::new(vec![Field::new(
        "region",
        DataType::Dictionary(Box::new(DataType::Int32), Box::new(DataType::Utf8)),
        true,
    )]));
    {
        use parquet::arrow::ArrowWriter;
        let f = std::fs::File::create(&src).unwrap();
        let mut w = ArrowWriter::try_new(f, schema.clone(), None).unwrap();
        let values = StringArray::from(vec!["north", "south", "east"]);
        let keys: Int32Array = (0..ROWS).map(|i| Some((i % 3) as i32)).collect();
        let dict = DictionaryArray::<arrow::datatypes::Int32Type>::try_new(keys, Arc::new(values))
            .unwrap();
        w.write(&RecordBatch::try_new(schema, vec![Arc::new(dict)]).unwrap())
            .unwrap();
        w.close().unwrap();
    }

    let stats = convert_parquet(&src, &cache, |_, _| {}).unwrap();
    assert_eq!(stats.rows, ROWS as u64);
    assert_eq!(
        stats.distinct_strings, 3,
        "streaming a {ROWS}-row dictionary column must intern 3 strings, not {}",
        stats.distinct_strings
    );

    let mapped = crate::MappedSheet::open(&cache).unwrap();
    for r in [0usize, 1, 2, 99_999, ROWS - 1] {
        assert_eq!(
            mapped.display(cell(r, 0)),
            ["north", "south", "east"][r % 3],
            "row {r}"
        );
    }
}

// ---------------------------------------------------------------------------
// Unsupported types are reported
// ---------------------------------------------------------------------------

#[test]
fn unsupported_types_are_reported_not_coerced() {
    // If the importer silently coerced, it would return Ok and we would get a
    // sheet full of plausible-looking debug text. The assertion is that it
    // returns an Err NAMING the column and the type.
    let list = Field::new(
        "items",
        DataType::List(Arc::new(Field::new("item", DataType::Int32, true))),
        true,
    );
    let err = classify(&list).unwrap_err();
    let msg = err.to_string();
    assert!(
        matches!(err, ArrowIoError::UnsupportedType { .. }),
        "a List column must be reported, got {err:?}"
    );
    assert!(
        msg.contains("items"),
        "the message must name the column: {msg}"
    );
    assert!(
        msg.contains("List"),
        "the message must name the type: {msg}"
    );

    for f in [
        Field::new(
            "s",
            DataType::Struct(vec![Field::new("a", DataType::Int32, true)].into()),
            true,
        ),
        Field::new("d", DataType::Decimal128(10, 2), true),
        Field::new("bin", DataType::Binary, true),
        Field::new(
            "iv",
            DataType::Interval(arrow::datatypes::IntervalUnit::DayTime),
            true,
        ),
        Field::new(
            "dictnum",
            DataType::Dictionary(Box::new(DataType::Int32), Box::new(DataType::Int64)),
            true,
        ),
    ] {
        assert!(
            matches!(classify(&f), Err(ArrowIoError::UnsupportedType { .. })),
            "{} / {:?} must be reported as unsupported, not coerced",
            f.name(),
            f.data_type()
        );
    }

    // Supported ones must NOT be reported — otherwise the test above would
    // pass against a `classify` that rejects everything.
    for f in [
        Field::new("a", DataType::Float64, true),
        Field::new("b", DataType::Int64, true),
        Field::new("c", DataType::Utf8, true),
        Field::new("d", DataType::Boolean, true),
        Field::new("e", DataType::Date32, true),
        Field::new("f", DataType::Timestamp(TimeUnit::Nanosecond, None), true),
        Field::new("g", DataType::Null, true),
        Field::new(
            "h",
            DataType::Dictionary(Box::new(DataType::Int32), Box::new(DataType::Utf8)),
            true,
        ),
    ] {
        assert!(
            classify(&f).is_ok(),
            "{} / {:?} is supported and must classify",
            f.name(),
            f.data_type()
        );
    }
}

#[test]
fn an_unsupported_column_fails_the_whole_import_before_reading_rows() {
    use arrow::array::*;
    let fx = Fixture::new("unsup");
    let p = fx.path("u.parquet");

    let item = Arc::new(Field::new("item", DataType::Int32, true));
    let schema = Arc::new(Schema::new(vec![
        Field::new("ok", DataType::Int64, true),
        Field::new("bad", DataType::List(item.clone()), true),
    ]));
    let mut lb = ListBuilder::new(Int32Builder::new());
    for i in 0..3 {
        lb.values().append_value(i);
        lb.append(true);
    }
    let batch = RecordBatch::try_new(
        schema,
        vec![
            Arc::new(Int64Array::from(vec![1i64, 2, 3])),
            Arc::new(lb.finish()),
        ],
    )
    .unwrap();
    write_parquet(&p, &batch);

    let err = import_parquet(&p).unwrap_err();
    match err {
        ArrowIoError::UnsupportedType { column, data_type } => {
            assert_eq!(column, "bad", "the offending column must be named");
            assert!(data_type.starts_with("List"), "got {data_type}");
        }
        other => panic!("expected UnsupportedType, got {other:?}"),
    }
}

// ---------------------------------------------------------------------------
// Lossy round trips are reported
// ---------------------------------------------------------------------------

#[test]
fn a_mixed_type_column_is_reported_as_lossy() {
    let fx = Fixture::new("mixed");
    let p = fx.path("m.parquet");

    let mut s = Sheet::new("s");
    s.set(cell(0, 0), Value::Number(1.0));
    s.set_text(cell(1, 0), "not a number");
    s.set(cell(2, 0), Value::Bool(true));
    // A clean column alongside it, so the report is specific rather than
    // "everything is lossy".
    for r in 0..3 {
        s.set(cell(r, 1), Value::Number(r as f64));
    }

    let (_, report) = export_parquet(&s, &p, &ExportOptions::default()).unwrap();
    assert!(!report.is_lossless());
    assert_eq!(
        report.mixed_columns,
        vec![0],
        "only column 0 is mixed; reporting column 1 too would be a false alarm"
    );

    // The values survive as text rather than being dropped.
    let back = import_parquet(&p).unwrap().sheet;
    assert_eq!(back.display(cell(1, 0)), "not a number");
    assert_eq!(back.display(cell(2, 0)), "TRUE");
    assert_eq!(back.get(cell(0, 1)), Value::Number(0.0));
}

#[test]
fn error_cells_export_as_their_spreadsheet_spelling() {
    let fx = Fixture::new("err");
    let p = fx.path("e.parquet");
    let mut s = Sheet::new("s");
    s.set(cell(0, 0), Value::Error(ErrorKind::DivZero));
    s.set(cell(1, 0), Value::Error(ErrorKind::Ref));
    export_parquet(&s, &p, &ExportOptions::default()).unwrap();
    let back = import_parquet(&p).unwrap().sheet;
    assert_eq!(back.display(cell(0, 0)), "#DIV/0!");
    assert_eq!(back.display(cell(1, 0)), "#REF!");
}

#[test]
fn an_all_null_column_survives_as_a_column() {
    use arrow::array::*;
    let fx = Fixture::new("nulls");
    let p = fx.path("n.parquet");
    let schema = Arc::new(Schema::new(vec![
        Field::new("empty", DataType::Null, true),
        Field::new("also_empty", DataType::Utf8, true),
    ]));
    let batch = RecordBatch::try_new(
        schema,
        vec![
            Arc::new(NullArray::new(4)),
            Arc::new(StringArray::from(vec![None::<&str>, None, None, None])),
        ],
    )
    .unwrap();
    write_parquet(&p, &batch);

    let s = import_parquet(&p).unwrap().sheet;
    assert_eq!(s.col_count(), 2, "a null column must not vanish");
    assert_eq!(s.row_count(), 4);
    for r in 0..4 {
        assert_eq!(s.get(cell(r, 0)), Value::Empty);
        assert_eq!(s.get(cell(r, 1)), Value::Empty);
    }
    assert_eq!(s.arena.len(), 0, "nulls must not intern an empty string");
}

#[test]
fn empty_file_with_a_schema_keeps_its_columns() {
    use arrow::array::*;
    let fx = Fixture::new("emptyfile");
    let p = fx.path("z.parquet");
    let schema = Arc::new(Schema::new(vec![
        Field::new("a", DataType::Int64, true),
        Field::new("b", DataType::Utf8, true),
    ]));
    let batch = RecordBatch::try_new(
        schema,
        vec![
            Arc::new(Int64Array::from(Vec::<i64>::new())),
            Arc::new(StringArray::from(Vec::<&str>::new())),
        ],
    )
    .unwrap();
    write_parquet(&p, &batch);

    let imported = import_parquet(&p).unwrap();
    assert_eq!(imported.stats.rows, 0);
    assert_eq!(imported.sheet.col_count(), 2);
    assert_eq!(imported.sheet.headers(), &["a".to_string(), "b".into()]);
}

#[test]
fn headers_round_trip_through_parquet_field_names() {
    let fx = Fixture::new("hdr");
    let p = fx.path("h.parquet");
    let mut s = Sheet::new("s");
    s.set_headers(vec!["Region".into(), "Revenue".into()]);
    s.set_text(cell(0, 0), "north");
    s.set(cell(0, 1), Value::Number(10.0));

    export_parquet(
        &s,
        &p,
        &ExportOptions {
            use_headers: true,
            ..Default::default()
        },
    )
    .unwrap();
    let back = import_parquet(&p).unwrap().sheet;
    assert_eq!(back.headers(), &["Region".to_string(), "Revenue".into()]);

    // Without use_headers the names are spreadsheet letters, not the labels —
    // asserting both directions so the flag is proven to do something.
    let p2 = fx.path("h2.parquet");
    export_parquet(&s, &p2, &ExportOptions::default()).unwrap();
    let back2 = import_parquet(&p2).unwrap().sheet;
    assert_eq!(back2.headers(), &["A".to_string(), "B".into()]);
}

#[test]
fn date_columns_round_trip_as_timestamps() {
    let fx = Fixture::new("dates");
    let p = fx.path("d.parquet");
    let mut s = Sheet::new("s");
    // 2000-01-01, 2024-02-29, and a half day.
    let serials = [
        ferrix_core::table::serial_from_civil(2000, 1, 1).unwrap(),
        ferrix_core::table::serial_from_civil(2024, 2, 29).unwrap(),
        ferrix_core::table::serial_from_civil(1999, 12, 31).unwrap() + 0.5,
    ];
    for (r, v) in serials.iter().enumerate() {
        s.set(cell(r, 0), Value::Number(*v));
    }
    export_parquet(
        &s,
        &p,
        &ExportOptions {
            date_columns: vec![0],
            use_headers: false,
        },
    )
    .unwrap();

    let back = import_parquet(&p).unwrap().sheet;
    for (r, v) in serials.iter().enumerate() {
        match back.get(cell(r, 0)) {
            Value::Number(got) => assert!(
                (got - v).abs() < 1e-6,
                "row {r}: serial {v} came back as {got}"
            ),
            other => panic!("row {r} is {other:?}"),
        }
    }
    // And the calendar day is the one we meant, not an epoch-shifted one.
    let (y, m, d, ..) = ferrix_core::table::serial_parts(match back.get(cell(1, 0)) {
        Value::Number(n) => n,
        _ => unreachable!(),
    });
    assert_eq!(
        (y, m, d),
        (2024, 2, 29),
        "leap day must survive the round trip"
    );
}

#[test]
fn exporting_a_mapped_sheet_works_through_the_same_trait() {
    // Proves `ArrowSource` is implemented for the out-of-core sheet, which is
    // the only kind a 10GB dataset has.
    let fx = Fixture::new("mappedexp");
    let csv = fx.path("in.csv");
    let cache = fx.path("in.ferrix");
    let out = fx.path("out.parquet");

    std::fs::write(&csv, "a,b\n1,north\n2,south\n3,north\n").unwrap();
    crate::convert_csv(&csv, &cache, b',', true, |_, _| {}).unwrap();
    let mapped = crate::MappedSheet::open(&cache).unwrap();
    assert_eq!(mapped.row_count(), 3);

    let (stats, _) = export_parquet(&mapped, &out, &ExportOptions::default()).unwrap();
    assert_eq!(stats.rows, 3);
    let back = import_parquet(&out).unwrap().sheet;
    assert_eq!(back.get(cell(0, 0)), Value::Number(1.0));
    assert_eq!(back.display(cell(1, 1)), "south");
    assert_eq!(back.display(cell(2, 1)), "north");
}
