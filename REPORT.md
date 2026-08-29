# Issue #39 — Trace Precedents and Dependents

**Branch:** `feat/trace-precedents`
**Clone:** `C:\Users\Error\projects\ferrix-trace` (isolated; the main checkout was never touched)
**Base:** `1dac3b2` on `main`

## What landed

A trace overlay built on top of the dependency graph that already existed
(`crates/ferrix-formula/src/depgraph.rs`). No graph changes were needed — this is
UI over machinery that was already correct and already tested.

### New file: `crates/ferrix-ui/src/trace.rs`

Pure BFS over `DepGraph`, deliberately free of egui so it unit tests without a
frame:

- `TraceState { origin: SheetCell, kind: TraceKind, depth: usize }`
- `edges_for(&DepGraph, TraceState) -> (Vec<Edge>, usize)` — the second value is
  the true total *before* the cap, which is what the "showing N of M" note needs.
- `MAX_ARROWS = 100`. The list is truncated; the total is not.
- A `seen` set is what makes a cycle terminate rather than spin.
- A **range** precedent (`SUM(A1:A10000000)`) yields ONE arrow at the range's
  top-left corner, not one arrow per contained cell. This is the scale invariant
  at the arrow level: the graph stores rectangles, and expanding them here would
  have thrown that away.

### `crates/ferrix-ui/src/app.rs`

- `trace: Option<TraceState>` plus `last_trace_arrows` / `last_trace_total`.
- `trace_precedents()`, `trace_dependents()`, `clear_trace()`, `trace_counts()`,
  `graph_snapshot()`, `active_sheet_id()`.
- Formula menu: Trace Precedents / Trace Dependents / Remove Arrows (the last
  disabled when there is nothing to remove).
- Shortcuts `Ctrl+[` and `Ctrl+]`, both suppressed while editing a cell.
- Arrow painting in the grid block, through the **same** `Grid::cell_screen_rect`
  the in-cell editor is positioned with — not a second copy of the geometry.
- Off-screen endpoints clamp to the viewport edge (`clamp_to_rect_edge`) rather
  than being drawn at wrong coordinates or dropped.
- Cycle edges are drawn dashed in `theme.error`, keyed off the existing
  `DepGraph::is_circular_at`.

### `crates/ferrix-ui/src/harness.rs`

Wrappers (`trace_precedents`, `trace_dependents`, `clear_trace`, `trace_counts`)
plus 9 harness tests. Assertions read `trace_counts()` (real paint output — the
counter is incremented inside the paint loop, per arrow actually drawn) and
`paint_shape_count()`, not a model flag.

## Repeated-invocation semantics

A second press on the SAME origin + direction increments `depth`, walking one
level further out, as Excel does. A press on a different cell or the other
direction starts fresh. Changing the selection deliberately does NOT clear the
arrows — they keep tracing their original origin until Remove Arrows.

## Test counts

| | before | after |
|---|---|---|
| `ferrix-ui` (bin `ferrix`) | 255 | 271 |
| workspace | 981 | 997 |

16 new tests: 6 pure-logic in `trace.rs`, 10 harness in `harness.rs`.

## Gates

All three run from one clean tree at `36d96a3`, bare (never piped through
`tail`/`head`), exit codes checked:

| gate | exit | result |
|---|---|---|
| `cargo test --workspace` | 0 | **997 passed, 0 failed** |
| `cargo fmt --all --check` | 0 | clean |
| `cargo clippy --workspace --all-targets -- -D warnings` | 0 | clean |

An earlier clippy run reported 3 errors. That was a false alarm: a sabotage
verification was editing the same working tree at the time and clippy linted the
neutered function. Re-run on the restored tree, it exits 0. Noting it because a
red gate in a log is worth explaining rather than quietly re-running.

## Sabotage verification

The point of the exercise: **would these tests fail if the feature did nothing?**
`edges_for` was neutered to `return (Vec::new(), 0);` as its first statement and
the suite re-run.

**13 of 15 fail under sabotage. 2 pass.** (Run against the 15 tests that
existed at commit `36d96a3`; a 16th was added afterwards, see below.)

The 2 survivors are both negative controls that assert *zero* arrows:

- `trace::tests::what_would_this_report_if_tracing_did_nothing`
- `harness::tests::tracing_a_plain_data_cell_draws_no_arrows`

They pass under sabotage **by construction** — "the feature drew nothing" is
exactly what they assert. That is inherent to a negative control, not a defect,
but it must be said plainly: **these two prove nothing on their own.** They are
only meaningful as the paired half of the 13 positive tests. If the 13 were ever
deleted, these 2 would keep passing against a completely dead feature and would
certify broken behaviour.

Separately, the sabotage run surfaced a real trap: `cargo test ... trace` runs
only **11 of the 15** tests, because 4 names do not contain the substring
`trace` (`tracing_...`, `remove_arrows_...`, `a_cell_with_many_dependents_...`,
`an_offscreen_precedent_...`). Anyone spot-checking with that filter would
believe they covered 15. The full `--workspace` gate does run all 15.

## What I did NOT verify

Blunt list — these are real gaps, not hedging:

1. **Nothing was ever seen on a screen.** Every assertion is a shape/arrow
   COUNT from `egui`'s tessellated frame output. I verified that N arrows were
   emitted, never that they *look* like arrows, point the right way, or land on
   the right cells. Arrowhead geometry, the dashed-cycle stroke and the two
   colours are entirely unverified visually. The app was never launched.
2. **The "showing N of M" badge is rendered but never visually confirmed.**
   This gap was found while writing this report and then closed: the status bar
   now shows `↗ showing N of M arrows` when the cap bites and `↗ N arrows`
   otherwise, next to the existing invalid-cell badge. A 16th test
   (`exceeding_the_cap_reports_showing_n_of_m_honestly`) builds 120 dependents
   against `MAX_ARROWS = 100` and asserts drawn==100 while total==120, so the
   cap provably bites and the total provably survives it. What is still
   unverified: I never *looked* at the badge. I assert the numbers feeding it,
   not that the string renders legibly or at all.
3. **The 500k-dependents claim is graph-level only.** `trace.rs` pins the cap at
   500 dependents (5x MAX_ARROWS) against `DepGraph` directly. The UI test uses
   40. I never built a 500k-dependent sheet, so "a cell with 500k dependents
   must not attempt 500k arrows" is proven for the arrow-list computation and
   *inferred*, not measured, for the paint loop.
4. **Off-screen handling is tested in one direction only.** The test covers an
   off-screen *precedent* with an on-screen origin. The `(None, Some(_))` arm —
   off-screen origin, on-screen target — is written but never exercised by a
   test. It is reachable in normal use.
5. **Cross-sheet edges are skipped, not drawn or indicated.** If a formula reads
   `Sheet2!A1`, that edge is computed but silently not painted, and it still
   counts toward `total` while never appearing in `drawn`. So "showing N of M"
   would under-report on a cross-sheet trace. No test covers this.
6. **Menu items and shortcuts were never clicked.** `Ctrl+[` / `Ctrl+]` and the
   three Formula-menu entries are wired to the same methods the tests call
   directly, but no test drives the keyboard or the menu. Per AGENT_GUIDE.md
   synthetic OS input is not evidence, so I did not fake it — the binding itself
   is unverified.
7. **No zoom / frozen-pane / filtered-sort interaction test.** Arrows resolve
   through the shared `RowResolver` and `cell_screen_rect`, so they *should*
   compose, but I did not assert that arrows land correctly under a sort, a
   filter, a freeze, or a non-1.0 zoom.
8. **Persistence untested.** Trace state is deliberately session-only; I did not
   test that it survives (or correctly does not survive) a sheet switch or
   reload.
