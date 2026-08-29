# Issue #43 — Multi-sheet formula references and 3D ranges

**Clone:** `C:/Users/Error/projects/ferrix-xsheet`
**Branch:** `feat/multi-sheet-refs` (branched from `fac2ec5`)
**Final commit:** see `git rev-parse HEAD` — last commit is `a352102` plus this report.

## Gates

All three run bare, exit code checked, never piped through `head`/`tail`:

| Gate | Result |
|------|--------|
| `cargo test --workspace` | **PASS** — 1436 passed, 0 failed |
| `cargo fmt --all --check` | **PASS** (exit 0) |
| `cargo clippy --workspace --all-targets -- -D warnings` | **PASS** (no output) |

Baseline at `fac2ec5` was 1381 tests. This branch adds **55**.

## Acceptance criteria

| Criterion | Status |
|---|---|
| `=Sheet2!A1` / `='My Sheet'!A1:B10` parse, evaluate, recalc | **Done** (largely pre-existing; verified and extended) |
| 3D ranges `=SUM(Sheet1:Sheet3!A1)` across consecutive sheets | **Done** (new) |
| Cross-sheet criteria ranges TESTED (SUMIF/COUNTIFS with XRange) | **Done** — and it was **broken**; see Bug 1 |
| Dependency graph spans sheets; editing `Sheet2!A1` recalcs Sheet1 | **Done** |
| Cross-sheet cycles detected exactly like same-sheet ones | **Done**, incl. cycles closing through a 3D run |
| Deleting a sheet → `#REF!` (not panic, no dangling ids) | **Done** — asserts formula TEXT *and* value |
| Renaming rewrites formula TEXT, quoting as needed, NOT string literals | **Done** |
| xlsx round trip for all of the above | **Done** — and it was **broken**; see Bug 4 |

Nothing was skipped.

## What landed

### New: 3D references (`Expr::X3D`)
- `Token::SheetSpan` + `Expr::X3D(first, last, top_left, bottom_right)`. A single-cell
  3D ref is the degenerate 1x1 rectangle — one variant, fewer match sites.
- Endpoint names are kept **as written**; which sheets lie between them is a
  tab-order question resolved at graph-build and eval time, never at parse time.
  Resolving in the parser would freeze the run against the tab order at typing
  time, so inserting a sheet between the endpoints would silently not join it.
- `depgraph::SheetIndex` replaces the old `Fn(&str) -> Option<SheetId>` closure
  the graph took. **This was the load-bearing change:** a closure can answer
  "what sheet is this name" but not "which sheets are in this run", so a 3D
  reference would have contributed zero edges and never recalculated.
- `CellSource::sheet_span` resolves a run to sheet names; SUM/COUNT/AVERAGE
  delegate to each sheet's own `sum_rect_in`/`count_rect_in`, so a 3D aggregate
  over 200M-row columns stays one columnar slice walk **per sheet**.

### Sheet rename rewrites formula TEXT
- `names::rename_sheet_in_formula` splices over the byte spans
  `refscan::qualifiers` reports. Textual, never an AST round trip — the parser
  discards the `$` markers, so re-rendering would unpin every absolute
  reference in the workbook (test: `renaming_a_sheet_keeps_absolute_markers`).
- **The asymmetry vs named ranges is implemented and tested.** A sheet name can
  also live inside a string literal. The scanner skips literals, and the test
  `renaming_a_sheet_does_not_touch_its_name_inside_a_string_literal` uses one
  formula with `Sheet2` in **both** positions:
  `=Sheet2!A1&" (from Sheet2)"` → `=Q1!A1&" (from Sheet2)"`.
- Re-quoting is bidirectional: `Sheet2` → `My Sheet` gains quotes, and back
  again drops them.
- `DepGraph.sheet_uses` records sheet names from formula TEXT. This is **not**
  derivable from the edges: a formula naming a missing sheet has no edges at
  all, and that is exactly the formula a rename must find.

### Sheet delete breaks referents visibly
- `delete_sheet` rewrites referents' TEXT so the qualifier becomes `#REF!`
  (`=Sheet2!A1*2` → `=#REF!*2`), returns the count, and the status line says it.
- Asserted on **text and value**, not "did not panic".
- The reason it must edit text, with its own test: without it, adding a new
  sheet that reuses the deleted name silently rebinds every orphaned formula to
  unrelated data.
- A broken range collapses to a single `#REF!`, not `#REF!:A3`.
- Parser gained `Token/Expr::Error` so the rewritten text parses back to a
  `#REF!` value instead of `#NAME?` (which blamed an unknown name for a broken
  reference). `#REF!` is scanned as one token so a sheet actually named `REF`
  cannot collide with it.

## Bugs found by the new tests (all pre-existing, all silent)

1. **Cross-sheet criteria ranges never matched TEXT.** `Value::Text` carries a
   `StrId` into the arena of the sheet it came from; the SUMIF family resolved
   those against the **home** sheet, so every text criterion over
   `Sheet2!A1:A4` matched nothing and returned `0` — a plausible answer, no
   error. Fixed with `CellSource::resolve_in`. **This is precisely the coverage
   gap issue #43 names**, and it was a live bug, not just missing tests.
2. **`SUM(#REF!)` folded to `0`** — a plausible total from a dead reference.
   `arg_error` now propagates a literal error constant.
3. **A criteria range that failed to resolve reported `#VALUE!`**, blaming the
   argument type for a deleted sheet. Now `#REF!`.
4. **SUMIF/SUMIFS/COUNTIF/COUNTIFS/AVERAGEIF/AVERAGEIFS were missing from
   `SUPPORTED_FUNCTIONS`** despite having `eval_call` arms, so any workbook
   using one lost the formula on xlsx load, keeping only the cached value, and
   never recalculated again.
5. **Scanner bug I introduced and my own test caught:** `qualifiers()` advanced
   one byte at a time, so in `Sheet1!A1:Sheet1!B4` the tail `1:Sheet1!` parsed
   as a 3D span and hid the `A1` reference from every reference rewriter.

## Scale invariant

Nothing added is per-cell.
- A 3D range is **one rectangle precedent per sheet** in the run, never one edge
  per cell — asserted directly on a 1,000,000-row range
  (`a_three_d_reference_becomes_one_precedent_per_sheet_in_the_run`).
- `SheetIndex` and `sheet_span` are bounded by the **sheet** count.
- `sheet_uses` is bounded by the number of qualifiers in a formula's text.
- 3D aggregation calls each sheet's existing columnar `sum_rect_in`, so the
  fast path is not lost by a formula becoming 3D; `for_each_3d` holds one
  `RangeSpec` at a time.

## WHAT I DID NOT VERIFY

Read this section before trusting anything above.

- **Excel was never launched.** The xlsx tests prove Ferrix writes a file
  Ferrix's own importer (calamine) reads back with identical formula text and
  cached values. They do **not** prove Excel accepts these files, renders the
  3D formulas, or agrees with the totals. In particular I did not confirm that
  Excel's on-disk spelling of a 3D reference matches what `rust_xlsxwriter`
  emits from our text — a real Excel file is the only way to settle that, and I
  did not open one.
- **No file produced by real Excel was imported.** Every round trip starts from
  a Ferrix export. If Excel writes 3D references in some form our tokenizer does
  not accept, the import would silently downgrade them to cached values
  (`formulas_dropped`), and no test here would notice.
- **The UI was never driven.** No `harness.rs` test was added. The rename and
  delete status-line strings in `app.rs` are exercised only by the compiler; I
  changed them from `Ok(())` to `Ok(count)` arms and did not assert the rendered
  text. The workbook-level behaviour behind them is fully tested.
- **No benchmark was run.** The scale claims are structural (counting
  precedents, reading the code paths), not measured. I did not run
  `ferrix-bench` or build a 200M-row fixture, so "one slice walk per sheet" is
  an argument about which function is called, not a timing.
- **`AVERAGEIFS` over a 3D range is untested** and, by design, refused: a 3D
  reference is not a `range_spec`, so the *IF family returns `#VALUE!` for one.
  That matches Excel, but I did not verify Excel's behaviour against a real
  Excel build.
- **Undo of a sheet rename/delete was not tested.** Neither rewrites go through
  `push_undo` — that was already true of `rename_sheet` before this change, and
  I did not extend it. Undoing a sheet delete is not a thing the workbook
  supports today, so the rewritten formula text is not restorable via Ctrl+Z.
- **Concurrency/threading untouched and unexercised.** No import/export
  cancellation path was retested beyond the existing suite passing.

## Cleanup

`benchdata/` was never created in this clone — no generated fixtures to remove.
The xlsx tests write to the system temp dir and delete on `Drop` (`TempXlsx`).
Nothing outside `C:/Users/Error/projects/ferrix-xsheet` was touched.

## Commits

- `842e94c` — `Expr::X3D` + tab-order `SheetIndex` for 3D references
- `b233182` — sheet rename rewrites formula TEXT; graph tracks sheet uses
- `533a7fa` — sheet delete breaks text to `#REF!`; cross-sheet criteria fixes
- `bb15eb5` — xlsx round trip for all of the above
- `a352102` — scan `#REF!` as one token, not a sheet named `REF`
