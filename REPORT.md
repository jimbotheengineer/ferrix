# Issue #32 — Parquet and Arrow import/export

Clone: `C:/Users/Error/projects/ferrix-parquet`, branch `feat/parquet`, forked
from `main` @ `571c510`.

## Gate results

Run bare (never piped), from the clone root:

| Gate | Result |
|---|---|
| `cargo test --workspace` | **PASS** — 1279 passed, 0 failed |
| `cargo fmt --all --check` | **PASS** — exit 0 |
| `cargo clippy --workspace --all-targets -- -D warnings` | **PASS** — exit 0 |

Baseline on `main` was 1255. 1279 − 1255 = **24 new tests**, all passing:
21 in `ferrix-io/src/arrow_io/tests.rs`, 3 in `ferrix-ui/src/app.rs`.

## What landed

**`crates/ferrix-io/src/arrow_io.rs`** (new, plus `arrow_io/tests.rs`)

* `import_parquet` / `import_ipc` / `import_any` → in-RAM `Sheet`. Batched at
  `ROW_GROUP_ROWS` (64K), so transient peak is one batch.
* `convert_parquet` → streams a Parquet file into the `.ferrix` columnar cache
  via `convert.rs`'s `Spill` writer (the same encoder `convert.rs` and
  `compact.rs` use — deliberately not a fourth one), then the caller mmaps it.
  Holds one row-group batch plus the arena, nothing else.
* `export_parquet` → written through the **low-level `SerializedFileWriter`**,
  not `ArrowWriter`. That is the load-bearing choice: `ArrowWriter` takes a
  whole `RecordBatch` (every column of the row group at once), which would
  multiply peak by the column count. The low-level writer lets us materialise
  and drop **one column stripe** at a time.
* `export_ipc` → Arrow IPC file, batched. Honest caveat in the module docs:
  IPC genuinely needs every column of a batch live at once (that is what a
  `RecordBatch` is), so its bound is `ROW_GROUP_ROWS × cols` — still
  independent of row count, but not one stripe.
* `ExportReport` reports mixed-type columns written as text, rather than
  silently coercing (the `rule_survives_xlsx` convention).

**Dependencies added** to `crates/ferrix-io/Cargo.toml`: `arrow = "56"`
(default-features off, `ipc`) and `parquet = "56"` (default-features off,
`arrow` + `snap`). Resolved to 56.2.1. Fetched from crates.io successfully.

**`crates/ferrix-ui/src/app.rs`**

* `load_any` — the existing csv/xlsx dispatch — gained one arm calling
  `ferrix_io::format_for_path`. Extended, not duplicated.
* `load_arrow` chooses storage the same way a CSV does: a Parquet file that
  fails `should_use_mmap` streams into the cache and is memory-mapped; smaller
  files and all `.arrow` load in RAM.
* `export_parquet_dialog` + `CommandId::FileExportParquet` in the registry.
  Date columns are taken from each column's `NumberFormat::Date(_)`, never
  guessed from magnitude.
* Open dialog filters extended from `ferrix_io::ARROW_EXTENSIONS` (one list,
  shared with the router, so the dialog cannot offer a file the loader
  refuses).

**`crates/ferrix-ui/src/sheet_view.rs`** — `ArrowSource for OwnedSheet`, so the
export writes typed values from the base+overlay composite view rather than
display strings.

## Acceptance criteria

| Criterion | Status | Evidence |
|---|---|---|
| `.parquet`/`.arrow` open through the normal File > Open path | **Done** | `a_parquet_file_opens_through_the_normal_load_path`, `an_arrow_ipc_file_opens_through_the_normal_load_path` — both call the real `load_any`, then build a real `Workbook` and assert decoded values at coordinates. |
| Export streams column by column, bounded peak | **Done** | `export_peak_is_one_stripe_not_the_file` exports the same sheet at 2 and 20 row groups and asserts `peak_stripe_bytes` is **identical**, while asserting the output file did grow 4x+. |
| Type mapping tested both directions | **Done** | `type_mapping_import_covers_every_supported_arrow_type` (9 Arrow types incl. an all-null row); `type_mapping_export_writes_the_declared_parquet_types` reads the **Parquet schema** back, so a write-everything-as-text implementation fails even though its values would round trip. |
| Dictionary Utf8 onto the arena, not expanded | **Done** | `dictionary_column_does_not_expand_to_per_row_strings`: 100k rows / 3 distinct → asserts `arena.len() == 3` **and** `arena.data_bytes() == 14`. Plus `..._with_many_distinct_values...` (5000 distinct over 50k rows → 5000) and `streaming_import_keeps_dictionary_bounded` (200k rows through the out-of-core path → 3). |
| Round trip preserves per-row identity AND order | **Done** | `parquet_round_trip_preserves_per_row_identity_and_order` — 64K+777 rows (crosses a row-group boundary, asserted), scattered holes, verified **per row at every index**, no checksum. Plus `round_trip_survives_a_shuffled_source_order`, where the source is a deliberate non-monotonic bijection so any internal sort/dedup is caught (a SUM would be identical either way). |
| Larger-than-RAM Parquet opens without materialising | **Partial — see below** | `large_parquet_streams_without_materialising` |
| Unsupported logical types reported, not coerced | **Done** | `unsupported_types_are_reported_not_coerced` (List/Struct/Decimal/Binary/Interval/non-Utf8-dictionary rejected, **and** the 8 supported types asserted to classify — otherwise the test would pass against a `classify` that rejects everything). `an_unsupported_column_fails_the_whole_import_before_reading_rows` checks the error names the column. |

## What I did NOT verify — read this part

1. **No file larger than actual RAM was ever opened.** The "larger than RAM"
   criterion is tested by *bound*, not by scale: a 400k-row × 4-col Parquet
   file is streamed and the reported `peak_block_bytes` is asserted to be
   ≤ 2 stripes and an order of magnitude below the ~25MB a materialising
   import would need. That proves the converter *holds* only a row group. It
   does **not** prove a 10GB file opens on an 8GB machine — nobody tried one.
2. **`peak_block_bytes` / `peak_stripe_bytes` are computed by the code under
   test**, from its own buffer capacities. They are not RSS measurements. A
   bug that allocated elsewhere (inside the parquet reader, say) would not
   show up in them. The invariance-across-row-counts assertion is the real
   signal; the absolute number is self-reported.
3. **No throughput measurement.** `convert.rs` does 245 MB/s; I did not
   benchmark `convert_parquet` against that or anything else. Performance is
   unknown beyond "the tests finish quickly".
4. **No interop check with pandas / DuckDB / Spark.** Files are written with
   Snappy and standard logical types, and they round trip through
   *Ferrix's own reader*. That proves self-consistency, not that pandas
   accepts them. Nobody opened one outside this process.
5. **`export_parquet_dialog` runs on the UI thread**, unlike the CSV export
   which has background progress/cancel plumbing. The write is still streaming
   so memory is bounded, but a very large Parquet export will freeze the UI.
   Documented in the function's doc comment. Reusing the background machinery
   needs it generalised past `ExportStats`, which is bigger than this issue.
6. **The export dialog and menu item were never clicked.** They are wired
   through the command registry and compile, and `export_parquet` itself is
   tested directly, but no headless-harness test drives the menu.
7. **Int64 → f64 precision.** Integers above 2^53 lose precision in a
   `Value::Number` cell. Inherent to the f64 cell type; documented in
   `classify`, not rejected (rejecting Int64 would make the importer useless),
   and **not** tested.
8. **Arrow IPC never takes the out-of-core path** — `.arrow` always loads in
   RAM. Deliberate, noted in `load_arrow`'s docs, but it means a huge
   `.arrow` file has no graceful degradation.
9. **`ScratchDir` cleanup on hard kill.** Spill files are removed via `Drop`;
   a SIGKILL mid-convert leaves a `ferrix-parquet-spill-*` directory beside
   the cache.

## Fixtures

All test fixtures are written to `std::env::temp_dir()` under self-deleting
guard structs and are removed on drop. No `benchdata/` was created, and
nothing was committed beyond source. Nothing outside this clone was touched.
