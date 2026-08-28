# Ferrix

A source-available spreadsheet built in Rust for datasets that break Excel.
Free for noncommercial use.

Excel stops at 1,048,576 rows. Ferrix opens a **10.85 GB CSV — 200 million
rows — in 221 microseconds** and scrolls it at over 10,000 fps.

```
--- 10.85 GB CSV, 200,000,000 rows x 8 cols ---
conversion:   109 s @ 95 MB/s     (one time, peak RAM 64 MB)
cold open:    221 us              (subsequent opens, from cache)
mapped:       12.0 GB             address space, NOT resident RAM
scrolling:    2.45 ms/viewport    (60fps budget: 16.67 ms)
SUM 200M rows: 638 ms          (exact, Kahan-compensated)

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

### Numerically exact aggregates

`SUM` uses Kahan compensated summation, not a naive accumulator. This is not
academic at spreadsheet scale: summing the integers 0…200,000,000 naively
returns `19,999,999,867,108,864` instead of `19,999,999,900,000,000` — off by
33 million — because once the running total passes 2^53 each addition rounds
away the addend's low bits. Ferrix returns the exact value, and it is *faster*
(2.5 s vs 3.4 s over 200M rows), because the loop is bound by memory bandwidth
rather than arithmetic. Four tests pin this, including one that reproduces the
original 200M-row drift in miniature.

### Search that scales with cardinality, not size

`Ctrl+F` over 1.6 billion cells (200M rows x 8 cols, 12 GB cache):

```
     needle          hits    pass 1   pass 2
      north    33,343,436   4663 ms   478 ms
 consulting    24,991,111    517 ms   432 ms
  cancelled    49,988,488    605 ms   596 ms
       4242           808    485 ms   319 ms   (numeric path)
 zzz-absent             0      0 ms     0 ms   (nothing matched the arena)
```

Two passes are reported because the cache is larger than RAM. The first needle
searched pays to fault ~12 GB off disk — 4663 ms for `north` is I/O, not search
work, and the same needle costs 478 ms once resident. Steady-state cost tracks
hit count as expected (`cancelled`, with 1.5x more hits, is the slowest). At
40M rows, where the cache fits in memory, both passes are identical.

The trick is inverting the problem. A naive search compares the needle against
every cell — 1.6 billion string comparisons, each needing the value formatted
first. But text cells don't store text; they store a 4-byte id into an arena
holding each *distinct* string once. So Ferrix:

1. Matches the needle against the arena — **18 comparisons** for this dataset,
   not 1.6 billion — producing a bitset of matching ids.
2. Scans the columns comparing 4-byte integers against that bitset, in
   parallel across cores.

Search cost therefore tracks the *cardinality* of the data, not its size. When
nothing matches the arena the column scan is skipped entirely, which is why the
absent-needle case costs 0 ms. Numbers are compared numerically against the
parsed needle, never by formatting 200M values into strings.

### Selection that scales

A selection is two corners — an anchor and a cursor — never a list of cells.
Selecting an entire 200M-row column is therefore 16 bytes, not 1.6 GB, and
`Selection` is asserted to stay exactly that size by a test.

Drag, Shift+click, and Shift+Arrow extend a range; `Ctrl+A` takes the used
range. Copy and paste speak **TSV**, the format Excel and Google Sheets put on
the clipboard, so blocks move between applications rather than only within
Ferrix.

Bulk operations are **one undo step**, not one per cell: clearing a 50-cell
block pushes a single entry, and one undo restores all 50. Clipboard and clear
operations are capped at 1,000,000 cells and refused with a message beyond
that — a whole-column select must not try to build 200M strings.

### Fill

Dragging the handle at a selection's corner fills. Two numeric cells continue
their progression (`0,1` becomes `0,1,2,3,…`); anything else tiles.

Formulas have their **relative references offset** — `=A1*2` filled down
becomes `=A2*2` — while `$` anchors stay pinned, so `=$A$1*2` fills unchanged
and the running-total idiom `=SUM($A$1:A1)` grows correctly to `=SUM($A$1:A2)`.

That rewriting happens on the formula *text* rather than its parsed tree. The
tokenizer records `$` as `abs_col`/`abs_row` flags, but `Expr::Ref` keeps only
a `CellRef`, so an AST rewrite would silently drop absolute markers. Rewriting
the source preserves `$`, the user's spacing, and anything the scanner does not
recognise — including `"A1"` inside a string literal, which is text, not a
reference.

### Editing without touching the base

Edits never modify the dataset. They live in a sparse copy-on-write overlay
consulted ahead of the base, so editing costs O(edits) rather than O(rows) and
the base can stay a read-only memory map.

Saving follows from that design: only the overlay is written, to a `.fxedits`
sidecar beside the data.

```
sales.csv                  the original
sales.ferrix               columnar cache (12 GB, mmap'd)
sales.ferrix.fxedits       edits only (bytes)
```

Measured on a 40M-row / 2.14 GB dataset: **5 edits saved to 181 bytes in
1.07 ms**, reloaded in 4.78 ms, with the base file untouched. Save cost tracks
the number of edits, not the size of the data.

Formulas are stored as **source text**, not just their last computed value, and
are re-evaluated in dependency order on load — a cached number can never
outlive the data it was derived from.

A sidecar records a fingerprint of the base it was written against (length,
mtime, row and column counts). If the base changes, the sidecar is **rejected
with a visible warning** rather than applied to cells that may now hold
something entirely different. Silent misapplication is the failure mode that
loses data, so it is the one case treated as fatal.

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
cargo test --workspace     # 262 tests
```

Tests assert real invariants, not happy paths: `Value` must stay <=16 bytes, a
numeric column must stay under 12 bytes/cell, parallel chunking must preserve
row order, embedded newlines must survive chunk splitting, mmap sections must
be 8-byte aligned, a corrupt cache must be rejected, formulas must recalculate
in dependency order, and cycles must be detected rather than hang.

## License

[PolyForm Noncommercial 1.0.0](LICENSE.md) — free for any noncommercial
purpose: personal projects, research, education, and use by charitable,
educational, public research, public safety, environmental, or government
organizations. Commercial use requires a separate license.

This is source-available rather than OSI open-source; the noncommercial
restriction is deliberate for now and can be relaxed later.
