# Issue #35 — Goal Seek

Clone: `C:\Users\Error\projects\ferrix-goalseek`
Branch: `feat/goal-seek` (branched from `1dac3b2`, `main`)
Final commit: **`5bce5c9`**

## Commits

| SHA | What landed |
|---|---|
| `adcd0e4` | `DepGraph::depends_on_at` / `depends_on` — transitive precedent walk, cycle-safe, cross-sheet, sees through range precedents. Goal Seek result types. 6 unit tests. *(This was the pre-existing uncommitted work; committed as-is once it built and its tests passed.)* |
| `616a8ec` | `Workbook::goal_seek` — the secant solver, as ONE undo step. 10 unit tests. |
| `5bce5c9` | The Data > Goal Seek dialog, app wiring, harness wrappers, 6 harness tests. |

## Acceptance criteria

| Criterion | Where it is satisfied | Test |
|---|---|---|
| Set A to V by changing B, iterating to `\|A-V\| < eps` or ~100 iters | `Workbook::goal_seek`, `GOAL_SEEK_MAX_ITERS = 100`, `GOAL_SEEK_EPSILON = 1e-6` | `goal_seek_hits_a_linear_target` |
| Refuses immediately when A does not depend on B, using the dep graph | `depends_on_at` checked **before** any recalc; returns `GoalSeekError::NotDependent` | `goal_seek_refuses_when_the_target_does_not_depend_on_the_changing_cell` (asserts undo depth unchanged and no value moved), `goal_seek_refuses_a_cell_the_target_does_not_depend_on` (asserts the message contains "does not depend") |
| Non-convergence reports the closest value, not success | Solver tracks `best_x`/`best_a` by `err`, not the last sample; UI renders "No solution found … Closest: …" | `goal_seek_reports_the_closest_value_rather_than_claiming_success`, `a_non_convergent_goal_seek_reports_the_closest_value_not_success` |
| Whole run is ONE undo step; cancelling restores B | Probes bypass history entirely; state restored; then a single `commit_edit` bracketed by `end_edit_run` | `goal_seek_is_exactly_one_undo_step_and_undo_restores_b`, `a_goal_seek_run_is_one_undo_step_and_cancel_restores_the_changing_cell` |
| A divergent case terminates | Iteration cap + zero-secant-denominator break + `GOAL_SEEK_DIVERGENCE_LIMIT = 1e12` / non-finite break | `goal_seek_terminates_on_a_divergent_target` (asserts iters <= cap, elapsed < 5s, `final_b` finite) |
| Works when A is several hops downstream of B | Precedent walk is transitive; recalc goes through `recalc_order_at` | `goal_seek_works_several_hops_downstream` (4 hops), `goal_seek_solves_through_the_real_dialog_two_hops_downstream` |

### The "one undo step" choice, and why

Driving the iteration through `commit_edit` would push one undo entry per probe.
They *would* coalesce — same cell, inside `COALESCE_WINDOW` — but that is a
**timing accident**. One recalculation slower than a second (entirely plausible
on a large sheet) silently splits the run into two undo steps and leaves the
user's first Ctrl+Z on an intermediate guess.

So the search writes probes **straight to the overlay** (`goal_seek_probe`: no
undo entry, no dirty flag, no coalescing state), restores the pre-search state
exactly, then makes **one** real `commit_edit` of the winning value bracketed by
`end_edit_run` on both sides. That entry captures the original value as its
`before` and every dependent's original cache as side effects, so a single
`undo()` rewinds the entire run. Cancel is literally that `undo()`.

## Gates — all three pass, run bare

```
cargo test --workspace          → TEST_EXIT=0
  ferrix-ui   (bin ferrix)      test result: ok. 271 passed; 0 failed
  ferrix-formula (lib)          test result: ok. 187 passed; 0 failed
  ferrix-core (lib)             test result: ok. 358 passed; 0 failed
  ferrix-io                     test result: ok. 172 passed; 0 failed
  + criteria_alloc 3, xlsx 8, io integration 4, rest 0
  TOTAL: 1003 passed, 0 failed, 0 ignored

cargo fmt --all --check         → FMT_EXIT=0
cargo clippy --workspace --all-targets -- -D warnings
                                → CLIPPY_EXIT=0
```

### Test counts

| | before | after | added |
|---|---|---|---|
| `ferrix-formula` lib | 181 | 187 | +6 (`depends_on*`) |
| `ferrix-ui` bin | 255 | 271 | +16 (10 workbook + 6 harness) |
| **workspace total** | **981** | **1003** | **+22** |

## Assertion quality — mutation-checked

The guide warns about a UI test in this repo that asserted the status line was
non-empty and passed against a dead gesture. I did not assert on the status line
at all; the harness tests read **numbers back out of the grid** (`app().display(...)`)
and **undo depth**.

To prove that is not self-deception, I mutated `solve = true` → `solve = false`
in the dialog's Solve button handler and re-ran. Result: **4 of the 6 harness
tests failed**, with messages like `B1 must have been solved to 10: left "2",
right "10"`. The two that still passed are the two that do not depend on solving
(the seeding test and the "Cancel with nothing applied" test), which is correct.
File restored afterwards; the mutation is not in any commit.

## Scale invariant

Nothing added is per row. The solver holds a handful of `f64`s and one saved
`CellInput`. Each probe recalculates the changing cell's dependents via
`recalc_order_at`, which is a property of the formula graph, not the sheet's
height. `depends_on_at`'s `seen` set is bounded by the number of formula cells,
not rows. `GoalSeekState` is three short strings and two rects, and is `None`
whenever the dialog is closed. No `benchdata` was generated.

## Design decisions worth flagging

- **Secant, not bisection.** No derivative needed, fast on the linear models
  spreadsheets are mostly made of, degrades to "no progress" rather than to a
  wrong answer. Cost: it does not guarantee a bracket, which is why the
  divergence limit and the zero-denominator break both exist.
- **Extra refusal not in the issue:** `GoalSeekError::ChangingCellIsFormula`.
  Searching a formula cell would mean overwriting the formula with a bare
  number — silent data loss. Excel refuses this too.
- **`By changing cell` is not pre-filled.** Guessing which input drives the model
  and being silently wrong is worse than an empty field.

## What I did NOT verify

- **The app was never launched.** `ferrix.exe` was never run interactively. The
  dialog is exercised only through the headless egui harness, so this proves the
  widgets lay out, paint, and respond to synthesised clicks under the harness's
  `RawInput` — **not** that it looks right on a real screen, that the window
  anchors sensibly at unusual DPI/zoom, or that the Data menu item is reachable
  at a real mouse position.
- **The `Data` menu item itself was never clicked.** Per the project convention
  (`cond_new_rule`, `freeze_at_cursor`), the harness calls the same entry point
  the menu item calls. The menu-bar code path *does* run every frame in every
  harness test, so it is proven not to panic, but "clicking Data > Goal Seek
  opens the dialog" is asserted only from the handler down.
- **No multi-sheet Goal Seek.** `goal_seek` takes `CellRef`s on the ACTIVE sheet.
  `depends_on_at` is fully cross-sheet and unit-tested as such, but the solver
  and dialog only address the active sheet. A cross-sheet Goal Seek
  (`Set Sheet2!C1 by changing Sheet1!B4`) is neither implemented nor tested.
- **No performance measurement.** I did not time Goal Seek against a 200M-row
  sheet or a deep dependency chain. The scale argument above is a reading of the
  code (nothing iterates rows), not a benchmark.
- **Convergence quality is not characterised.** The suite covers linear,
  quadratic-unreachable, `1/x`-divergent, flat-text, and zero-start. I did not
  test discontinuous models, models with multiple roots (which root you get is
  unspecified and start-dependent), or `IF`-driven step functions.
- **The `.ferrix` sidecar round trip after a Goal Seek was not tested.** The run
  commits a normal literal edit through `commit_edit`, so it should persist like
  any other, but I did not save-and-reload to confirm.
- **`GOAL_SEEK_DIVERGENCE_LIMIT = 1e12` is a judgement call**, not a measured
  threshold. A legitimate model whose input genuinely exceeds 1e12 would be cut
  off early and reported as non-convergent.
