# REPORT — issue #23, lookup functions

Branch `feat/lookup` in `/c/Users/Error/projects/ferrix-lookup`. Not pushed.

## Gates

All three, run bare (no pipe), at the final commit:

| gate | result |
|---|---|
| `cargo test --workspace` | **pass** — exit 0, 0 failed |
| `cargo fmt --all --check` | **pass** — exit 0 |
| `cargo clippy --workspace --all-targets -- -D warnings` | **pass** — exit 0 |

## Commits

| sha | what |
|---|---|
| `bb05b78` | `crate::lookup` + one guarded arm in `eval_call`; 40 tests |
| `50e3470` | lookup family pinned in `compose_tests.rs` |
| `53ba4eb` | `tests/lookup_alloc.rs` — allocation-counting scale test |
| `0d7c652` | xlsx round trip; `_xlfn.` prefix stripping on import |
| `74f8989` | INDIRECT/depgraph gap pinned as assertions |

## Files

Added:
- `crates/ferrix-formula/src/lookup.rs`
- `crates/ferrix-formula/src/lookup/tests.rs` (42 tests)
- `crates/ferrix-formula/tests/lookup_alloc.rs` (9 tests)

Modified:
- `crates/ferrix-formula/src/lib.rs` — `pub mod lookup;`
- `crates/ferrix-formula/src/eval.rs` — ONE guarded arm, placed last
- `crates/ferrix-formula/src/compose_tests.rs` — lookup added to the
  mutual-exclusion table; new cross-family and builtin-shadowing tests
- `crates/ferrix-io/src/xlsx.rs` — import allowlist asks
  `is_lookup_fn` directly; `strip_future_fn_prefixes`; round-trip tests;
  three pre-existing tests re-pointed from VLOOKUP to XMATCH

## Acceptance criteria

| criterion | status |
|---|---|
| VLOOKUP exact + approximate; approximate on unsorted returns Excel's answer | met |
| MATCH types -1, 0, 1 | met |
| XLOOKUP if_not_found, reverse search, match modes | met |
| INDEX row/col pair, and 0 = whole row/column | met **with a documented limit** (below) |
| 10M-row lookup does not materialise the column | met — asserted two independent ways |
| INDIRECT recursion budget yields `#REF!`; dependency decision documented | met |
| Missing key `#N/A`; wrong-typed arg `#VALUE!` | met |
| xlsx round trip with matching cached values | met |

### INDEX with 0 — the honest version

`INDEX(A1:A5, 3, 0)` and `INDEX(A1:E1, 0, 4)` work: `0` means "the whole
column/row", and where that collapses to exactly one cell the cell is
returned.

`INDEX(A1:C5, 3, 0)` — where `0` really would select three cells — returns
`#VALUE!`, **not** an array. This engine has no array value and dynamic-array
spilling is explicitly out of scope for #23. The alternative would have been
to silently return the first cell of the row, which is a plausible-looking
wrong answer. Test: `index_with_zero_means_the_whole_row_or_column`.

### The scale invariant — what was actually measured

Two independent instruments, because each catches a failure the other misses.

**1. Cells visited** (`src/lookup/tests.rs`). A `CellSource` that *reports*
10,000,000 rows, answers any cell in O(1), and counts every `get`. Measured:

| formula | reads | ceiling |
|---|---|---|
| exact VLOOKUP, hit at row 8 | 9 | 16 |
| exact MATCH, hit at offset 4 | 4 | 8 |
| approximate VLOOKUP, key near row 10M | 24 | 32 |
| XLOOKUP search_mode 2 | (under 40) | 40 |
| reverse XLOOKUP, hit at last row | 2 | 8 |
| INDEX direct address | **exactly 1** | 1 |
| unselected CHOOSE branch | **exactly 0** | 0 |

(Read counts other than the two marked "exactly" were observed once via
temporary instrumentation and then removed; the committed tests assert the
ceilings, not these figures.)

**2. Allocations** (`tests/lookup_alloc.rs`). Wrapping global allocator,
thread-local counter, compared across a 10x row change (20k vs 200k). Every
scan probes for an **absent** key so it runs the full column length — a test
whose key sits in row 500 of both sheets compares two identical 500-cell scans
and proves nothing.

**Both were verified to actually fail**, which is the part that matters:

- Injecting one `format!()` per visited cell into `linear_find`: 5 of the 9
  allocation tests fail with 20,003 vs 200,003 allocations.
- Making `binary_last` collect its lane into a `Vec` first: the visit-count
  tests fail with **10,000,024 reads against the ceiling of 32**.

An assertion that only checked the returned value passes against both of those
mutants and is therefore worthless for this criterion.

### The one place the invariant is NOT held, and why

`XLOOKUP` match_mode ±1 (nearest smaller/larger) with a **linear** search_mode
visits every cell in the lane. This is not a defect I could engineer away:
"nearest value" over data carrying no sortedness promise is not answerable
without looking at every candidate.

Memory is still **O(1)** — one running best offset and one borrowed `Key`,
never a buffer — so peak memory stays bounded by the viewport even though the
visit count is not. `xlookup_does_not_allocate_per_row_in_any_search_mode`
measures exactly this path. Callers who want the sublinear path pass
`search_mode` ±2, which is the same trade Excel exposes. Documented in the
module header table.

### INDIRECT and the dependency graph

`collect_precedents` walks the parse tree. `INDIRECT("A"&B1)` contains an edge
to `B1` and **no edge to the cell it will read**, because that cell is named by
a runtime string.

The decision, documented in the `lookup.rs` module header and now asserted by
`indirect_contributes_no_static_precedent_edge_for_its_target`:

- The edge cannot be resolved at parse time — it changes whenever `B1` does.
- It cannot be cached after the first evaluate either. A cached
  `INDIRECT -> A7` is stale the instant `B1` becomes 8, and stale *silently*:
  the formula still recalculates (it depends on `B1`) but against the wrong
  precedent set, so a change to `A8` never wakes it. A wrong cached edge is
  worse than no edge because it looks like coverage.
- So the target is re-resolved on **every** evaluate. Cost: one A1 parse per
  evaluation. `indirect_re_resolves_its_target_on_every_evaluation` proves the
  behaviour by moving the driving cell and re-evaluating the same parsed
  expression.

Consequence, asserted by `the_dep_graph_cannot_see_an_indirect_cycle`: an
`INDIRECT` cycle is **invisible** to `DepGraph::is_circular_at`, which walks
static edges only. That is precisely why `MAX_INDIRECT_DEPTH` (16) exists — it
is the only defence, not a belt-and-braces extra.

### Interop defect found and fixed

The round-trip test found a real one. OOXML spells XLOOKUP as
`_xlfn.XLOOKUP`; a file **without** the prefix shows `#NAME?` in Excel. Export
was already correct (rust_xlsxwriter adds it). Import was not — it saw
`_xlfn.XLOOKUP(...)`, failed to parse, and silently degraded the formula to its
cached value. The symptom is a workbook that looks completely correct and never
recalculates.

`strip_future_fn_prefixes` normalises `_xlfn.` / `_xlws.` at the file boundary,
leaving text inside string literals alone (stripping there would corrupt user
data). Two tests cover it, one unit and one end-to-end that asserts the
exported file really does still carry the prefix.

---

## WHAT I DID NOT VERIFY

Read this section before trusting anything above.

1. **Excel was never launched.** The xlsx round trip proves the file survives
   *Ferrix's own* export/import cycle and that calamine can read it. It does
   **not** prove Excel accepts the file, renders these formulas, or agrees with
   any cached value. The `_xlfn.` fix is based on the OOXML convention and on
   what rust_xlsxwriter emits — it is well-founded, but no version of Excel
   confirmed it.

2. **No lookup was ever run against a genuinely 10-million-row sheet.** The
   scale tests use a synthetic `CellSource` that *reports* 10M rows. That is
   the right instrument for "does the algorithm scan or collect" and it caught
   a materialising mutant at 10,000,024 reads. It says nothing about real
   `Column` storage behaviour, cache effects, or wall-clock time at that size.
   The largest real `Sheet` any test builds is 200,000 rows.

3. **Excel-compatibility is asserted from documented behaviour, not from a
   differential test against Excel.** Every expected value in the tests is one
   I reasoned out from Excel's documented semantics. The most exposed of these:
   - Approximate VLOOKUP over **unsorted** data. The test pins that a specific
     probe sequence lands on offset 1 and returns 1000. Excel's binary search
     is documented but its exact probe order is not contractual; a different
     (equally valid) probe order gives a different answer. What I am confident
     of is the *class*: an answer, never an error. The exact landing is pinned
     so a change is at least visible.
   - `XLOOKUP` match_mode 2 (wildcard) combined with a **binary** search_mode.
     I degrade it to exact matching, on the argument that a glob has no
     position in a sort order. I did not confirm what Excel does here.
   - Cross-type collation (number < text < boolean) in the lookup comparator.
     Reused from the existing `criteria` module's conventions, not
     independently verified against Excel.

4. **`INDIRECT`'s R1C1 support is narrow.** Absolute `R5C3` only. Relative
   `R[1]C[1]` returns `#REF!` deliberately (it is defined relative to the
   formula's own cell, which evaluation here does not carry). Excel supports
   the relative form; Ferrix does not, and refuses rather than guessing.

5. **`MAX_INDIRECT_DEPTH = 16` was not tuned against real workbooks.** It is
   far past deliberate use and far below stack risk, but the number is a
   judgement call, not a measurement.

6. **No UI-layer testing at all.** I did not touch `ferrix-ui` and did not run
   the headless harness. Whether these functions behave correctly in the paint
   loop, in the formula bar, or through `WorkbookSource`'s `CellSource` impl is
   untested by me. `WorkbookSource` implements the same trait so I expect it to
   work, but expectation is not evidence.

7. **Concurrency.** `INDIRECT_DEPTH` is thread-local and the tests are
   single-threaded per test function. I did not test evaluation of `INDIRECT`
   across threads or with the depth counter under contention.

8. **The three re-pointed pre-existing tests.** I changed
   `unsupported_formulas_degrade_to_their_cached_value`,
   `every_listed_function_really_evaluates` and
   `unsupported_calls_are_caught_when_nested` from using VLOOKUP as their
   "unimplemented function" example to XMATCH, because VLOOKUP is implemented
   now. The properties they test are unchanged, but they are other people's
   tests and I changed them without asking.

9. **`benchdata/`** was never created, so there is nothing to clean up. No
   files outside this clone were touched.
