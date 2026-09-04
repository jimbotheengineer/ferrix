# Ferrix

A source-available spreadsheet built in Rust for datasets that break Excel.
Free for noncommercial use.

Excel stops at 1,048,576 rows. Ferrix opens a **10.85 GB CSV — 200 million
rows — in 221 microseconds** and scrolls it at over 10,000 fps.

```
--- 10.85 GB CSV, 200,000,000 rows x 8 cols ---
conversion:   42.2 s @ 245.5 MB/s     (one time, peak RAM 64 MB)
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

## Status: public test

Ferrix is in **public testing**. The engine and most spreadsheet features
work at 200M-row scale; expect rough edges in the UI and please
[open an issue](../../issues) for anything surprising. What works today:

- Loading: CSV (any size), `.xlsx` (multi-sheet), Parquet / Arrow IPC
- Editing: formulas, undo/redo (including structural edits), fill,
  copy/paste (TSV interop with Excel), find & replace, comments,
  merged cells, data validation with dropdowns and autocomplete
- Formulas: 80+ functions (SUMIFS/AVERAGEIFS/INDEX/MATCH/VLOOKUP…),
  cross-sheet refs, named ranges, dynamic array spills, `$` anchoring,
  F4 cycling, point-mode / Ctrl+click cell linking, trace precedents
- Views: sort, filter, remove duplicates, subtotals, group/outline,
  freeze/split panes, hide rows/columns, conditional formatting,
  sparklines, zoom, per-sheet state
- Analysis: charts (line/bar/histogram/scatter, multi-series, custom
  labels, SVG export), pivot tables, goal seek, consolidate
- Output: CSV / `.xlsx` / Parquet export, print to PDF/HTML with page
  setup, print areas and page breaks
- Protection: sheet/workbook protection, lock/unlock cells
- **Agent bridge**: let an AI agent drive the live app, visibly (below)
- Not yet: collaborative editing, scripting hooks, a macro recorder

## Quick start

```bash
git clone https://github.com/jimbotheengineer/ferrix
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

The design has one governing invariant: **memory is bounded by what is on
screen plus what you have edited — never by the row count.** Every feature
below is shaped by it.

### Two storage paths

Files under 1 GB parse straight into RAM. Larger ones — or smaller ones that
the *measured* memory budget says would not fit — are converted once into a
columnar `.ferrix` file beside the source, then memory-mapped, so dataset
size is bounded by **disk, not memory**, and reopening is instant.

The conversion streams: CSV is read in 32 MB blocks (record-aligned and
quote-aware), split at exact record boundaries across every core, parsed in
parallel, and merged in source order into per-column spill files. Peak memory
is one block plus the string arena, independent of file size. Converting the
10.85 GB benchmark used 64 MB.

Once mapped, reading a cell indexes into the mapping and lets the OS fault in
the containing 4 KB page. The page cache keeps hot rows resident and evicts
cold ones under pressure — exactly the behaviour you want, implemented by the
kernel rather than by us.

### Columnar, type-segregated storage

A column keeps parallel arrays: a 1-byte type tag, an 8-byte float array (only
if the column ever holds a number), a 4-byte string-id array (only if it ever
holds text), and a 1-bit validity bitmap. A numeric column costs ~9.1
bytes/cell against the ~40 a `Vec<Option<Cell>>` of boxed enums would burn.
Every section of the on-disk format is 8-byte aligned so `f64` slices are
read straight out of the mapping — no copy, no unaligned loads.

### String interning

Spreadsheet text is overwhelmingly low-cardinality. All strings go into one
arena and cells hold a 4-byte `StrId`. The 200M-row benchmark contains **18
distinct strings**, stored once. This also keeps `Value` at 16 bytes, enforced
by a unit test.

### Editing without touching the base

Edits never modify the dataset. They live in a sparse copy-on-write overlay
consulted ahead of the read-only base:

```
get(cell) -> overlay.get(cell).unwrap_or_else(|| base.get(cell))
```

Editing is O(edits), not O(rows) — three edits in a 200M-row file cost three
entries — and saving writes only the overlay, to a `.fxedits` sidecar beside
the data:

```
sales.csv                  the original
sales.ferrix               columnar cache (12 GB, mmap'd)
sales.ferrix.fxedits       edits only (bytes)
```

A sidecar records a fingerprint of the base it was written against; if the
base changes, the sidecar is rejected with a visible warning rather than
silently applied to rows that may now hold something else. Comments, number
formats, page setup, pivots and column sizing each keep their own sidecar,
stored per column or range — never per cell.

### Structure as view transforms

Sort, filter, hide, group, remove-duplicates, and row/column insert/delete
are all **permutations of display order** over the immutable base — a run
list of row ranges, kilobytes at any scale. That is also what makes
structural undo cheap: undoing a 10M-row dedupe restores a snapshot of the
run list, never a copy of the removed rows. Everything keyed by display
position (edits, comments, formats, merges) moves in the same operation, so
data can never slide out from under its annotations.

### Formulas

Tokenizer → Pratt parser → evaluator, with a workbook-wide dependency graph
that stores ranges as rectangles: `SUM(A1:A200000000)` is one node with one
rectangular precedent, resolved by containment test rather than 200M edges.
Recalculation is incremental and ordered; cycles are detected, including
across sheets.

Reference rewriting (fill, insert/delete, paste) edits the formula **text**,
not the parsed tree — the parser discards `$` markers, so an AST round-trip
would silently drop anchoring. `=$A$1*2` fills unchanged; `=SUM($A$1:A1)`
grows to `=SUM($A$1:A2)`; inserting a row above turns `=$C$2+C3` into
`=$C$3+C4` with the anchors intact.

Formulas are entered the way you expect: type them (any casing — `=(g3*d2)`
works), or point — while the text ends in `=`, `(`, an operator or a comma,
clicking a cell links its reference instead of committing, Ctrl+drag links a
range, and each reference is coloured to match its outline in the grid.

### Numerically exact aggregates

`SUM` uses Kahan compensated summation. Summing 0…200,000,000 naively is off
by 33 million once the running total passes 2^53; Ferrix returns the exact
value, and is *faster*, because the loop is bound by memory bandwidth rather
than arithmetic.

### Search that scales with cardinality, not size

Text cells store arena ids, so `Ctrl+F` matches the needle against the arena
first — 18 comparisons, not 1.6 billion — then scans columns comparing 4-byte
integers against the resulting id set, in parallel. Search cost tracks how
many *distinct* values the data has, not how many rows. Filter mode renders
through a row-index mapping built once per search; a truncated result set is
labelled in red rather than pretending to be complete.

### Charts: aggregating more data than there are pixels

Charts aggregate in the columnar store before any geometry exists — min/max
decimation for lines (a one-row spike in a million rows always survives),
binning for histograms, density grids for scatter. Output size tracks the
canvas, never the input. The result is a `Scene` of geometry in data
coordinates that feeds both the screen and the SVG exporter, so what you see
is what exports. Titles, axis labels and series names are editable in the
chart window; multiple Y columns overlay as separate coloured series.

### The agent bridge

Ferrix is built to be **agent-compatible**: an AI assistant can drive the
running app while you watch. Toggle **View → 🤝 Agent bridge** and the app
watches `<workbook>.fxagent` for appended commands:

```
select E1:E200            move the visible selection
put H1 =SUMIFS(...)       type into a cell (validation + undo + status)
get M1:N6                 append displayed values to <file>.out as TSV
chart N1:N6 bar O         chart a range
label title=Profit by Region; y=Profit ($)
series I K                overlay more value columns
svg out/chart.svg         export the chart
```

Every command executes through the same paths keyboard input takes — the
selection moves on screen, edits are undoable, the status line narrates, and
the Agent window shows each executed line verbatim. It is off by default,
per session; stale command files never replay; and the Launch button runs
*any* agent CLI you configure (`agent_command = "claude -p {prompt}"`,
`hermes -z {prompt}`, `codex exec {prompt}`…) with no shell in between, so
prompt text cannot become arguments.

## Architecture

```
crates/
  ferrix-core     storage: columns, arena, bitmap, sheet, values, edit overlay
  ferrix-io       CSV/xlsx/Parquet ingest, .ferrix format, streaming
                  conversion, mmap reader, sidecars, PDF/HTML print
  ferrix-formula  tokenizer, Pratt parser, evaluator, dependency graph,
                  reference scanning/rewriting, fill
  ferrix-ui       egui front-end: virtualized grid, editors, ribbon, charts,
                  pivots, agent bridge, headless test harness
  ferrix-bench    data generator + benchmark harnesses
```

`ferrix-core` has no UI and no I/O dependencies, so it compiles fast and is
trivial to fuzz and benchmark in isolation. The UI is tested through a
headless harness that feeds real input events into the real app — see
`crates/ferrix-ui/src/harness.rs`.

## Testing

```bash
cargo test --workspace
cargo fmt --all --check
cargo clippy --workspace --all-targets -- -D warnings
```

2,100+ tests assert real invariants, not happy paths: `Value` must stay
≤16 bytes, a numeric column under 12 bytes/cell, parallel chunking must
preserve row order, embedded newlines must survive chunk splitting, mmap
sections must be 8-byte aligned and a corrupt cache rejected, formulas must
recalculate in dependency order, cycles must be detected rather than hang,
`$` anchors must survive structural rewriting, and a capped aggregation must
say so instead of looking complete.

## Contributing

Issues and PRs welcome. Read [.github/AGENT_GUIDE.md](.github/AGENT_GUIDE.md)
first — it encodes the scale invariants and workflow this codebase holds
contributors (human and AI alike) to, and every rule in it was learned from
something going wrong.

## License

[Ferrix Noncommercial 1.0.0](LICENSE.md) — free for noncommercial use:
personal projects, research, education, and use by charitable, educational,
public research, public safety, or environmental organizations.

**Commercial use and government use each require a separate license** —
government institutions (and contractors acting for them) are not covered
by the free grant, even for noncommercial purposes. Contact the author
via GitHub for licensing.

This is source-available rather than OSI open-source; the restrictions are
deliberate for now and can be relaxed later.
