# Ferrix

An open-source spreadsheet built in Rust for datasets that break Excel.

Excel stops at 1,048,576 rows. Ferrix opens **10 million rows in 2.5 seconds**
and scrolls them at **8,900 fps**.

```
--- ingest (10,000,000 rows x 8 cols, 530 MB CSV) ---
parse time:   2493 ms
throughput:   201.6 MB/s
heap:         0.61 GB      (1.16x the file size on disk)
bytes/cell:   7.63

--- scrolling ---
per viewport: 0.001 ms     (budget at 60fps: 16.67 ms)

--- full-column aggregates over all 10M rows ---
SUM(E:E)      = 2503706828         (13.7 ms)
MAX(G:G)      = 5495               (24.9 ms)
AVERAGE(H:H)  = 49.997672999       (18.5 ms)
```

Measured on a 16-core Windows 11 machine. Reproduce with the commands below.

## Status

Early but real. The performance thesis is proven end-to-end: ingest, storage,
virtualized rendering, and formula evaluation all work at 10M rows today.
Editing, persistence, and .xlsx support are not built yet — see the roadmap.

## Quick start

```bash
git clone https://github.com/ferrix-sheets/ferrix
cd ferrix
cargo build --release

# Open a CSV
./target/release/ferrix your-data.csv
```

Requires Rust 1.82+. On Windows you also need the MSVC build tools
(`winget install Microsoft.VisualStudio.2022.BuildTools` with the
"C++ build tools" workload); on Linux, a working `cc` plus X11 or Wayland dev
headers.

### Reproduce the benchmark

```bash
cargo build --release
./target/release/gen-data 10000000 benchdata/bench10m.csv
./target/release/bench-load benchdata/bench10m.csv
```

## How it goes fast

The performance comes from four decisions, each measurable in the numbers above.

**Columnar, type-segregated storage.** A column keeps parallel arrays: a
1-byte type tag, an 8-byte float array (allocated only if the column ever holds
a number), a 4-byte string-id array (only if it ever holds text), and a 1-bit
validity bitmap. A numeric column costs ~9.1 bytes/cell instead of the ~40 a
`Vec<Option<Cell>>` of boxed enums would burn. That is the difference between
0.6 GB and 4 GB for this dataset.

**String interning.** Spreadsheet text is overwhelmingly low-cardinality —
statuses, categories, region codes. All strings go into one contiguous arena
and cells hold a 4-byte `StrId`. The benchmark's 10M rows contain 18 distinct
strings, stored once. This is also what keeps `Value` at 16 bytes, which a unit
test enforces.

**Parallel CSV ingest.** The file is `mmap`ed, split into one chunk per core at
exact record boundaries, and parsed with zero coordination between workers.
Finding those boundaries is the subtle part: quote state cannot be guessed from
a local window, because a `"` anywhere earlier flips the meaning of every later
newline. So a single linear pass tracks exact quote parity and emits boundaries
as it goes — cheap (multiple GB/s) relative to the parallel parse it unlocks,
and correct for embedded newlines. There is a test for exactly this.

**Viewport virtualization.** The renderer computes which rows and columns
intersect the visible rect and paints only those — about 280 cells, regardless
of whether the sheet holds 100 rows or 10 million. Cells are painted directly
onto the `Painter` rather than as egui widgets, avoiding ~1,500 id allocations
and hit-tests per frame. Aggregations like `SUM(E1:E10000000)` bypass the cell
API entirely and walk the typed `f64` slice.

### Known limits

Scroll offsets are f32 pixels into a virtual canvas of `rows * 22px`. Row
addressing stays exact while one ulp of that canvas is smaller than a row,
which holds to **~16.7M rows** (f32's 24-bit mantissa). Past that, scrolling
must switch to integer row units. Two unit tests pin this boundary so it cannot
regress silently.

## Architecture

```
crates/
  ferrix-core     storage: columns, string arena, bitmap, sheet, values
  ferrix-io       ingest/export: memmap + parallel CSV (xlsx next)
  ferrix-formula  tokenizer, Pratt parser, evaluator
  ferrix-ui       egui/eframe front-end: virtualized grid, formula bar
  ferrix-bench    data generator + benchmark harness
```

`ferrix-core` has no UI and no I/O dependencies, so it compiles fast and is
trivial to fuzz and benchmark in isolation.

## Formulas

Implemented: `SUM` `AVERAGE` `COUNT` `MIN` `MAX` `IF` `AND` `OR` `NOT` `ABS`
`SQRT` `ROUND` `FLOOR` `CEILING` `INT` `LN` `LOG10` `EXP`, the operators
`+ - * / ^ & % = <> < > <= >=`, parentheses, ranges (`A1:B10`), and absolute
refs (`$A$1`).

Excel-compatible semantics are tested, including right-associative `^`
(`2^3^2` = 512), error propagation through ranges, `#DIV/0!` / `#VALUE!` /
`#NUM!` / `#NAME?` error kinds, and the `LOG10` ambiguity (bare `LOG10` is a
cell reference; `LOG10(` is a function — same as Excel).

## Roadmap

- [x] Columnar engine, 10M+ rows
- [x] Parallel CSV ingest
- [x] Virtualized grid at 60fps+
- [x] Formula parser and evaluator
- [ ] Cell editing + dependency-graph recalc
- [ ] Native `.xlsx` import/export
- [ ] Undo/redo
- [ ] Lua scripting hooks
- [ ] CRDT groundwork for collaborative editing
- [ ] Sort, filter, pivot

## Testing

```bash
cargo test --workspace     # 88 tests
```

Tests assert real invariants, not just happy paths: `Value` must stay ≤16
bytes, a numeric column must stay under 12 bytes/cell, parallel chunking must
preserve row order across a 300k-row file, embedded newlines must survive
chunk splitting, and `SUM` over 100k cells must finish in under 100ms.

## License

MIT OR Apache-2.0
