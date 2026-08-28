# Ferrix

An open-source spreadsheet built in Rust for datasets that break Excel.

Excel stops at 1,048,576 rows. Ferrix opens a **10.85 GB CSV — 200 million
rows — in 221 microseconds** and scrolls it at over 10,000 fps.

```
--- 10.85 GB CSV, 200,000,000 rows x 8 cols ---
conversion:   109 s @ 95 MB/s     (one time, peak RAM 64 MB)
cold open:    221 us              (subsequent opens, from cache)
mapped:       12.0 GB             address space, NOT resident RAM
scrolling:    2.45 ms/viewport    (60fps budget: 16.67 ms)
SUM 200M rows: 638 ms

--- 530 MB CSV, 10,000,000 rows x 8 cols (in-RAM path) ---
parse:        2.5 s @ 200 MB/s
heap:         0.61 GB             7.63 bytes/cell
scrolling:    0.001 ms/viewport
SUM 10M rows: 13.7 ms
```

Measured on a 16-core Windows 11 machine with 31 GB RAM — note the 10.85 GB
file is larger than the free memory on that box, which is the entire point.
Reproduce with `just bench` (see below).

## Status

Early, but the hard parts work. Loading, scrolling, editing, formulas, and
recalculation all function at 200M rows today. Not yet built: `.xlsx` import,
saving edits back out, sort/filter/pivot.

## Quick start

```bash
git clone https://github.com/ferrix-sheets/ferrix
cd ferrix
cargo build --release
./target/release/ferrix your-data.csv
```

Requires Rust 1.82+. On Windows also install the MSVC build tools
(`winget install Microsoft.VisualStudio.2022.BuildTools`, "C++ build tools"
workload); on Linux a working `cc` plus X11 or Wayland dev headers.

### Benchmarks

```bash
just bench              # 10M rows, generates + measures + deletes the data
just bench 200000000    # 200M rows (~11 GB, needs ~23 GB free disk)
```

`just bench` cleans up after itself. Use `just bench-keep` to retain the file,
then `just clean-data` when finished.

## How it works

### Two storage paths

Files under 1 GB parse straight into RAM. Larger ones are converted once into a
columnar `.ferrix` file beside the source, then memory-mapped — so dataset size
is bounded by **disk, not memory**, and reopening is instant.

The conversion streams: CSV is read in 64 MB blocks, parsed into per-column
spill files, then concatenated into the final layout. Peak memory is one block
plus the string arena, independent of file size. Converting the 10.85 GB
benchmark used 64 MB.

Once mapped, reading a cell indexes into the mapping and lets the OS fault in
the containing page. The page cache keeps hot rows resident and evicts cold
ones under pressure — exactly the behaviour you want, implemented by the kernel
rather than by us.

### Columnar, type-segregated storage

A column keeps parallel arrays: a 1-byte type tag, an 8-byte float array (only
if the column ever holds a number), a 4-byte string-id array (only if it ever
holds text), and a 1-bit validity bitmap. A numeric column costs ~9.1
bytes/cell against the ~40 a `Vec<Option<Cell>>` of boxed enums would burn.

### String interning

Spreadsheet text is overwhelmingly low-cardinality. All strings go into one
arena and cells hold a 4-byte `StrId`. The 200M-row benchmark contains **18
distinct strings**, stored once. This also keeps `Value` at 16 bytes, enforced
by a unit test.

### Viewport virtualization

The renderer paints only the rows and columns intersecting the visible rect —
about 280 cells, whether the sheet holds 100 rows or 200 million. Cells are
painted directly onto the `Painter` rather than as egui widgets, avoiding
~1,500 id allocations and hit-tests per frame.

Scroll position is an **f64 row index**, not a pixel offset. An f32 pixel
canvas (the obvious approach) silently breaks past ~16.7M rows because one ulp
grows larger than a row; f64 row indices stay exact past 10^15 rows. Two tests
pin this boundary.

### Editing without touching the base

Edits live in a sparse copy-on-write overlay consulted before the base:

```
get(cell) -> overlay.get(cell).unwrap_or_else(|| base.get(cell))
```

Editing is therefore O(edits), not O(rows) — three edits in a 200M-row file
cost three HashMap entries — and the base can stay a read-only memory mapping.
Undo restores a previous overlay entry and never touches the base.

The dependency graph holds **only formula cells**, with ranges stored as
rectangles rather than expanded edges. `SUM(A1:A200000000)` is one node with
one rectangular precedent, so a change anywhere in that range resolves via a
containment test rather than 200M edge lookups.

## Architecture

```
crates/
  ferrix-core     storage: columns, arena, bitmap, sheet, values, edit overlay
  ferrix-io       CSV ingest, .ferrix format, streaming conversion, mmap reader
  ferrix-formula  tokenizer, Pratt parser, evaluator, dependency graph
  ferrix-ui       egui front-end: virtualized grid, editor, formula bar
  ferrix-bench    data generator + benchmark harnesses
```

`ferrix-core` has no UI and no I/O dependencies, so it compiles fast and is
trivial to fuzz and benchmark in isolation.

## Editing

Click or arrow to a cell and type. `F2` edits in place, `Esc` cancels, `Enter`
commits and moves down, `Tab` commits and moves right. `Delete` clears.
`Ctrl+Z` / `Ctrl+Y` undo and redo. Formula cells are marked with a small dot.

Formulas: `SUM` `AVERAGE` `COUNT` `MIN` `MAX` `IF` `AND` `OR` `NOT` `ABS`
`SQRT` `ROUND` `FLOOR` `CEILING` `INT` `LN` `LOG10` `EXP`, operators
`+ - * / ^ & % = <> < > <= >=`, parentheses, ranges, and absolute refs.

Excel-compatible semantics are tested: right-associative `^` (`2^3^2` = 512),
error propagation through ranges, the full `#DIV/0!` / `#VALUE!` / `#NUM!` /
`#NAME?` error set, circular-reference detection, and the `LOG10` ambiguity
(bare `LOG10` is a cell reference; `LOG10(` is a function — same as Excel).

## Roadmap

- [x] Columnar engine, 10M+ rows in RAM
- [x] Parallel CSV ingest
- [x] Virtualized grid at 60fps+
- [x] Formula parser and evaluator
- [x] Cell editing, dependency graph, incremental recalc, undo/redo
- [x] Out-of-core mmap storage — 200M rows / 10GB+ verified
- [ ] Save edits back to CSV / `.ferrix`
- [ ] Native `.xlsx` import/export
- [ ] Sort, filter, pivot
- [ ] Lua scripting hooks
- [ ] CRDT groundwork for collaborative editing

## Testing

```bash
cargo test --workspace     # 165 tests
```

Tests assert real invariants, not happy paths: `Value` must stay <=16 bytes, a
numeric column must stay under 12 bytes/cell, parallel chunking must preserve
row order, embedded newlines must survive chunk splitting, mmap sections must
be 8-byte aligned, a corrupt cache must be rejected, formulas must recalculate
in dependency order, and cycles must be detected rather than hang.

## License

MIT OR Apache-2.0
