# Ferrix Feature Roadmap

> **For Hermes:** Hand each numbered feature to `tool-user` (or a parallel-agent-orchestration wave). Every feature listed here is self-contained and owns its own crate area, its own test surface, and its own set of gates. Merge serially per the `parallel-agent-orchestration` skill.

**Goal:** Grow Ferrix from a very fast viewer/editor into a spreadsheet a working analyst can live in, without giving up the 200M-row properties that make it worth having.

**Guiding rules for every feature below:**
1. Peak memory stays bounded by viewport + edit overlay, never by row count. If a feature can't hold that line at 200M rows, it needs a design note explaining why or it isn't done.
2. New behaviour ships with tests that pin the *invariant*, not just the happy path (mirroring the existing test style: `Value<=16 bytes`, cycle detection, chunk-order preservation, etc.).
3. Every merge passes the three gates: `cargo test --workspace`, `cargo fmt --all --check`, `cargo clippy --all-targets -D warnings`.
4. UI features get exercised through `crates/ferrix-ui/src/harness.rs` (egui RawInput), NOT through synthetic OS input. A failed synthetic click is not a bug.
5. Generated benchmark data lives under `benchdata/` and is deleted when the measurement ends. See `SCRATCH.md`.

---

## Snapshot of what's already in the tree

Merged and working (confirmed by reading the source, not by the README roadmap):
- Columnar engine, parallel CSV ingest, virtualized grid, incremental recalc, undo/redo, mmap out-of-core
- Multi-sheet workbooks with cross-sheet formulas and cross-sheet cycle detection
- `.fxedits` sidecar with staleness fingerprint (save path)
- Streaming CSV export (atomic rename, bounded memory)
- `.xlsx` import AND export via calamine + rust_xlsxwriter, incl. round-trippable error cells and formulas with cached values
- Structured tables with real OOXML table parts (name box, filter dropdowns, banded rows, per-column validation, conditional formatting scales/bars)
- Search + filter mode composed with table filters
- Chart aggregation (min/max decimation preserves single-row spikes)
- Column reorder (drag), row/col ops, cell formatting (font/size/bold/italic/underline), light/dark themes, resource guards
- Headless test harness driving the real app via egui RawInput

Roadmap items in the README that are ACTUALLY still open:
- Sort (filter is already done)
- Pivot tables
- Lua scripting
- CRDT collaboration groundwork
- "Save edits back to CSV / .ferrix" — CSV export exists; the missing piece is baking edits into a new `.ferrix` cache so the sidecar can be retired

The features below fill those gaps and add the shortest list of things a spreadsheet has to have that Ferrix currently doesn't.

---

## Feature 1 — Expand the formula library

**Why:** SUM/AVG/COUNT/MIN/MAX/IF/AND/OR/NOT + a handful of math builtins is not enough to do actual analysis. Every real dataset needs lookups, text massage, dates, and conditional aggregation. This is the single highest-leverage feature for the "why would I use this over Excel" argument, because the existing engine already scales the additions to 200M rows for free.

**Scope (one PR per group, in this order):**

1. **Conditional aggregation:** `SUMIF`, `COUNTIF`, `AVERAGEIF`, plus `SUMIFS`/`COUNTIFS`/`AVERAGEIFS` (multi-criteria). Excel-compatible criteria syntax including `">100"`, `"<>foo"`, wildcards `*`/`?`.
2. **Error handling:** `IFERROR`, `IFNA`, `ISBLANK`, `ISNUMBER`, `ISTEXT`, `ISERROR`, `ISERR`, `ISNA`, `NA()`, `ERROR.TYPE`.
3. **Lookups:** `VLOOKUP`, `HLOOKUP`, `INDEX`, `MATCH`, `XLOOKUP` (with `if_not_found` and match modes), `CHOOSE`, `INDIRECT` (evaluated but cycle-safe).
4. **Text:** `LEFT`, `RIGHT`, `MID`, `LEN`, `UPPER`, `LOWER`, `PROPER`, `TRIM`, `CLEAN`, `SUBSTITUTE`, `REPLACE`, `FIND`, `SEARCH`, `CONCAT`/`CONCATENATE`, `TEXTJOIN`, `TEXT` (number → formatted string), `VALUE`, `REPT`.
5. **Dates/times:** `TODAY`, `NOW`, `DATE(y,m,d)`, `YEAR`, `MONTH`, `DAY`, `HOUR`, `MINUTE`, `SECOND`, `WEEKDAY`, `EOMONTH`, `EDATE`, `DATEDIF`, `DAYS`, `NETWORKDAYS`. Underlying storage stays f64 serial (already the case in the xlsx path).
6. **Statistics:** `MEDIAN`, `MODE`, `STDEV.P`/`STDEV.S`, `VAR.P`/`VAR.S`, `PERCENTILE.INC`, `QUARTILE.INC`, `RANK`, `LARGE`, `SMALL`.
7. **Math extras:** `MOD`, `POWER`, `SIGN`, `TRUNC`, `ROUNDUP`, `ROUNDDOWN`, `MROUND`, `SUMPRODUCT`.

**Files likely to change:**
- `crates/ferrix-formula/src/eval.rs` (dispatch + implementations)
- `crates/ferrix-formula/src/parser.rs` (only if a new operator lands; probably not — everything above is function-call shaped)
- `crates/ferrix-formula/src/depgraph.rs` (INDIRECT: dependency is data-dependent, must be re-collected on evaluate; document why cycles through INDIRECT are detected via the existing per-cell visit rather than statically)
- New `crates/ferrix-formula/tests/` files, one per group, each round-tripping against Excel behaviour we care about

**Verification per group:**
- `cargo test -p ferrix-formula` covering the excel-compat corner cases we already care about: right-associativity, `#DIV/0!` / `#VALUE!` / `#NUM!` / `#NAME?` propagation, `LOG10` ambiguity. Add: `VLOOKUP` exact vs approximate mode, `MATCH` -1/0/1 modes, `XLOOKUP` reverse search, `IFERROR` catching each error type.
- xlsx round-trip test: write a workbook using the new functions, read it back with calamine, cached values match.

**Risks:**
- `INDIRECT` is the only one that breaks static dep-graph assumptions. Guard it by evaluating with a small recursion budget and treating budget exhaustion as `#REF!`.
- Wildcards in `*IF` need the same collation as `SUBSTITUTE`; centralise the matcher in one module and use it from both.

---

## Feature 2 — Sort

**Why:** Filter shipped, sort is the obvious next thing on any tabular UI. Users expect click-a-header-to-sort to just work.

**Scope:**
- Click a column header (or a header inside a structured table) → toggles ascending/descending/none.
- Multi-column sort via right-click → "Sort by this, then by ..." dialog.
- Sort is a **view transform**, not a data move: same trick as filter — build a `RowOrder: Vec<u32>` mapping visible → underlying, composed after search-filter and table-filter in the same fixed order.
- Row numbers in the header stay the ORIGINAL row number (matches how filter already behaves).
- Stable sort. Empty cells always sort last regardless of direction (Excel behaviour).
- Numeric columns sort numerically; text columns sort by locale-aware case-insensitive comparison; mixed-type columns sort by type-tag first then by value within tag.

**Files likely to change:**
- New `crates/ferrix-core/src/order.rs` already exists — check whether it's already the sort skeleton; if so, extend it rather than starting over.
- `crates/ferrix-ui/src/grid.rs` compose step alongside the two existing filters.
- `crates/ferrix-ui/src/sheet_view.rs` header-click handling.

**Verification:**
- Test: sort 10M-row column in RAM path, assert visible order is monotone.
- Test: sort composes cleanly under an active search filter (sort operates on the FILTERED rows).
- Bench: sort 10M f64 column, target < 2s single-threaded, < 500ms with rayon.
- Harness test: click header three times, assert asc → desc → none cycle.

**Risks:**
- On mmap columns, sorting must not materialise the whole column into a Vec — indirect-sort the index vector, read cells through the mmap during compare.

---

## Feature 3 — Fill handle (drag-to-fill) UI

**Why:** `crates/ferrix-formula/src/fill.rs` already implements the formula-rewriting side and the module comment even names the invariant (rewrite text, not AST). What's missing is the pointer gesture. Users type this feature into muscle memory in Excel.

**Scope:**
- Small square in the bottom-right corner of the selection ("fill handle"). Cursor changes to a crosshair on hover.
- Drag it down/right/up/left to extend the selection; on release, fill the new cells.
- Fill modes:
  - Single cell → copy (formula rewritten via existing `fill.rs`).
  - Two-cell numeric selection → linear series (detect stride, e.g. 1,2 → 3,4,5).
  - Text with trailing integer → increment ("Q1"→"Q2","Q3").
  - Date column → increment by day, with modifier for month/year.
- Ctrl-drag = force copy, no series detection.
- Double-click the handle when a column to the left has data = auto-fill down to that column's last row.

**Files likely to change:**
- `crates/ferrix-ui/src/grid.rs` (hit-test for the handle rect, drag state)
- `crates/ferrix-formula/src/fill.rs` (add series-detection helpers if not present)
- `crates/ferrix-core/src/overlay.rs` (fill applies through the same edit path as paste)

**Verification:**
- Harness: click A1, type `=B1*2`, select A1, fill down to A10 via drag; assert A5 evaluates as `B5*2` and A5's stored source is exactly `=B5*2` (proves text-rewrite path, not AST).
- Harness: A1=1, A2=2, select A1:A2, drag to A10; assert A10==10.
- Harness: double-click auto-fill mirrors the left column's extent.
- Undo: a fill is ONE undo step regardless of how many cells it wrote (matches the existing bulk-edit rule).

---

## Feature 4 — Named ranges

**Why:** Every serious formula sheet uses them. Also a prerequisite for pivot and for scripting.

**Scope:**
- Workbook-scoped and sheet-scoped names.
- Name Box (top-left, above row headers) shows the current cell's name or the range coordinate; typing a name and pressing Enter navigates to it; typing a NEW name and Enter defines it for the current selection.
- Formula > Name Manager modal for edit/delete.
- Names participate in the dep graph: renaming a name that other formulas reference rewrites their text (unlike sheet rename, because a name has no other meaning — silent rebind is safe here).
- Deletion of a referenced name turns dependents into `#NAME?`.

**Files likely to change:**
- New `crates/ferrix-formula/src/names.rs`
- `crates/ferrix-formula/src/parser.rs` (identifier resolution: try name table before falling back to `#NAME?`)
- `crates/ferrix-formula/src/depgraph.rs`
- `crates/ferrix-io/src/xlsx.rs` (`definedNames` OOXML on both sides)
- `crates/ferrix-ui/src/sheet_view.rs` (Name Box widget)

**Verification:**
- Test: define `Sales = Sheet1!B2:B1000`, formula `=SUM(Sales)` evaluates equal to `=SUM(Sheet1!B2:B1000)`.
- Test: renaming `Sales` to `Revenue` rewrites the SOURCE TEXT of dependent formulas.
- Test: xlsx round-trip preserves both workbook and sheet-scoped names.

---

## Feature 5 — Find & replace

**Why:** Search already highlights matches; replace is the other half.

**Scope:**
- Ctrl+H opens the panel next to the existing Ctrl+F search box.
- Match case, whole cell, regex, "within: sheet / workbook", "look in: values / formulas".
- Replace / Replace All. Replace All is ONE undo step (bulk-edit rule).
- Progress + cancel button for Replace All across 200M rows (use `budget.rs` / `cancel.rs` that already exist).

**Files likely to change:**
- `crates/ferrix-core/src/search.rs` (add replace iterator)
- `crates/ferrix-ui/src/sheet_view.rs` (panel)
- `crates/ferrix-core/src/overlay.rs` (batch apply)

**Verification:**
- Test: replace across 10M rows completes; peak RAM stays under overlay + viewport.
- Test: Replace All is one undo entry; Undo restores every changed cell.
- Test: cancel mid-replace leaves already-applied edits in place and reports the count.

---

## Feature 6 — Freeze panes, split, zoom

**Why:** Basic viewport control. Every spreadsheet ships this.

**Scope:**
- View > Freeze Rows / Freeze Columns / Freeze Both at selection.
- Frozen region paints from an independent scroll offset (0), rest of viewport scrolls under it. Column widths and row heights are shared.
- Split view: two independent scroll offsets sharing one column layout, per axis.
- Zoom 25%–400%. Zoom scales font size, column widths, and row heights proportionally; grid coordinates in `f64` unchanged.

**Files likely to change:**
- `crates/ferrix-ui/src/grid.rs` (paint pipeline: iterate frozen band before body band)
- `crates/ferrix-ui/src/sheet_view.rs` (menu, keyboard shortcuts)
- `crates/ferrix-ui/src/prefs.rs` (persist zoom per sheet)

**Verification:**
- Harness: freeze rows at row 5, scroll body to row 1M; row 1 still visible with row number "1".
- Harness: zoom 200%, click cell at (x,y) still resolves to the correct data cell.

---

## Feature 7 — Bake edits back into the `.ferrix` cache

**Why:** The README roadmap says "save edits back to CSV / .ferrix". CSV export is done. The `.ferrix` side is the interesting one: after enough edits accumulate, the sidecar becomes the point of failure and startup pays to reapply it every open.

**Scope:**
- Command: File > Compact (or automatic after N edits > threshold, opt-in).
- Rewrite `.ferrix` streaming, column by column, applying overlay values as we go. Peak memory = one column's stripe, same discipline as convert.
- Old `.ferrix` and `.fxedits` are deleted only after the new file has been fsync'd and renamed into place. Failure leaves both originals intact.
- Undo history is cleared on compact — same rule as save, for the same reason (the timeline no longer matches disk).

**Files likely to change:**
- New `crates/ferrix-io/src/compact.rs`
- `crates/ferrix-io/src/convert.rs` (share the writer half)
- `crates/ferrix-ui/src/app.rs` (menu entry, progress modal)

**Verification:**
- Test: edit 100 cells over a 200M-row cache, compact, reopen; cache has no sidecar, cells show edited values, mtime fingerprint written.
- Test: compact atomicity — kill the process mid-write, original files intact.
- Bench: 10GB compact stays under 128MB peak RAM.

---

## Feature 8 — Autosave and crash recovery

**Why:** With edits living in the sidecar and undo cleared on save, an unsaved crash loses everything typed since the last manual save. This is the failure mode a user will remember.

**Scope:**
- Every N seconds (default 30, configurable) write the current overlay to `<base>.ferrix.fxedits.autosave` — separate file from the "official" sidecar.
- On startup, if an autosave exists and is newer than the official sidecar, prompt: "Recover edits from HH:MM ago?" with Recover / Discard.
- Never overwrite the official sidecar without user action.
- Autosave file is deleted on clean exit and on manual save.

**Files likely to change:**
- `crates/ferrix-io/src/edits.rs`
- `crates/ferrix-ui/src/app.rs`

**Verification:**
- Test: kill the process after edits + one autosave tick, restart, recover prompt appears, choosing Recover restores overlay.
- Test: manual save clears the autosave file.

---

## Feature 9 — Pivot tables

**Why:** The one feature that turns a spreadsheet into an analysis tool. Also the one that most benefits from Ferrix's row count.

**Scope (phased — this one is the largest, break into 3 PRs):**

**9a — Aggregation kernel.** In `ferrix-core`, add `pivot::compute(rows: RowRange, group_by: &[ColIdx], values: &[(ColIdx, Agg)]) -> PivotResult` where `Agg` is Sum/Count/Avg/Min/Max/StdDev. Streams over the columnar store, hashes group keys into an ahash map, one pass. Test on 10M rows.

**9b — Pivot sheet type.** A pivot result is stored as a new kind of sheet in the workbook: it references the source sheet + spec, has read-only cells, and refreshes on demand (button) or on source-cell edit (opt-in).

**9c — Pivot builder UI.** Drag columns into Rows / Columns / Values wells. Filter widget uses the existing filter machinery.

**Files likely to change:**
- New `crates/ferrix-core/src/pivot.rs`
- `crates/ferrix-core/src/scene.rs` (Sheet variant)
- New `crates/ferrix-ui/src/pivot_panel.rs`

**Verification:**
- Test: pivot 10M rows, 1000 unique groups, single-Sum value; result matches naive Python-computed truth on a 100k subset.
- Bench: pivot 10M-row two-key sum in < 2s single-threaded.

---

## Feature 10 — Lua scripting

**Why:** README roadmap. Enables users to extend without recompiling.

**Scope:**
- `mlua` (Lua 5.4) sandboxed: no io, no os, no debug, no package.
- Two entry points:
  - Formula function: register a Lua function and it becomes callable from the grid, e.g. `=SCRIPT("myfunc", A1:A10)`.
  - Macro: run a script that mutates cells through the same `overlay` API editing goes through, so it's ONE undo step per macro invocation.
- Script files live in `<workbook>.scripts/`, discovered on load.
- Time and memory budget per invocation (existing `budget.rs`).

**Risks:**
- Determinism of formula-callable Lua matters for the dep graph. Document: script funcs are treated like `INDIRECT` — dependencies recollected each evaluate, no static caching.

---

## Feature 11 — Conditional formatting UI

**Why:** The xlsx path already writes conditional formatting rules for tables (per `table_xlsx.rs`). The interactive editor for defining them inside Ferrix is missing.

**Scope:**
- Right-click range → Conditional Formatting → New Rule.
- Rule types: cell value comparison, top/bottom N, above/below average, unique/duplicate, data bar, color scale, icon set (basic), formula-based.
- Rules live on the sheet, evaluated in the paint loop against visible cells only (bounded work per frame — do NOT evaluate rules for the whole sheet).
- xlsx round-trip: rules survive save/reload.

**Files likely to change:**
- `crates/ferrix-core/src/format/` (new module for rule storage + eval)
- `crates/ferrix-ui/src/sheet_view.rs`
- Round-trip in `crates/ferrix-io/src/xlsx.rs`

---

## Feature 12 — Comments / cell notes

**Why:** Very cheap to add, universally expected, useful for scripting handoff.

**Scope:**
- Right-click cell → Insert Comment. Small red triangle in the corner. Hover shows the note.
- Stored in a sparse map alongside the overlay.
- Round-trips through xlsx as threaded comments (simple author/text form, no reply threading in v1).

---

## Feature 13 — Print / PDF export

**Why:** Analysts email PDFs. Without it, exporting an analysis means screenshotting.

**Scope:**
- File > Export PDF: page setup (portrait/landscape, margins, fit-to-width, fit-to-page, repeat header rows).
- Render via `printpdf` or `genpdf`, one page per row band, streaming so a 1M-row PDF doesn't blow memory. Warn if the export will produce > 1000 pages.

---

## Feature 14 — CRDT collaboration groundwork

**Why:** README roadmap. This is the biggest architectural change on the list and should land LAST of the non-experimental items, once the overlay path is stable and after compact/autosave.

**Scope (foundation only in this feature; live sync ships later):**
- Wrap `EditOverlay` operations in a Yjs-style CRDT (`yrs` crate).
- Each edit produces a client-ordered op with a Lamport timestamp; overlay state is derivable from op log.
- On-disk format: `.fxedits` becomes an op log, `.fxedits.snapshot` a materialised checkpoint. Backward compatible read for existing sidecars.
- No network layer yet. No presence, no cursors. Just the local op log + convergence.

**Risks:**
- The 200M-row story means the op-count-vs-memory story matters. Snapshot compaction has to be automatic once the log exceeds a threshold.

---

## Suggested execution order

Ship in the order that pays off soonest:

1. Feature 1 (formulas — group 1 conditional aggregation and group 2 error handling FIRST, they unblock everything)
2. Feature 2 (sort)
3. Feature 3 (fill handle)
4. Feature 5 (find & replace)
5. Feature 4 (named ranges)
6. Feature 6 (freeze/split/zoom)
7. Feature 8 (autosave)
8. Feature 7 (compact)
9. Feature 11 (conditional formatting UI)
10. Feature 12 (comments)
11. Feature 13 (print/PDF)
12. Feature 9 (pivot — three-part)
13. Feature 10 (Lua)
14. Feature 14 (CRDT groundwork)

Features 1–6 are the "obvious spreadsheet gaps" that the largest number of users will hit within their first hour with Ferrix. Features 7–8 close the data-loss failure modes. 9–14 are the differentiation layer.

Each of the fourteen features is independently parallelisable — the branches share the workspace but touch different modules — so a two- or three-way `parallel-agent-orchestration` wave per tier is safe. Merge serially, run the three gates between each merge, per the skill.
