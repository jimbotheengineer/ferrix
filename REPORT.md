# Issue #26 — Formula library: statistical functions

Clone: `C:\Users\Error\projects\ferrix-stats`
Branch: `feat/stat-functions`

## What landed

New module `crates/ferrix-formula/src/stats.rs` (+ `src/stats/tests.rs`)
implementing: `MEDIAN`, `MODE` (and `MODE.SNGL`), `STDEV.P`, `STDEV.S`,
`VAR.P`, `VAR.S`, `PERCENTILE.INC`, `QUARTILE.INC`, `RANK` (and `RANK.EQ`),
`LARGE`, `SMALL`.

### Files touched

| File | Change |
| --- | --- |
| `crates/ferrix-formula/src/stats.rs` | new, ~400 lines |
| `crates/ferrix-formula/src/stats/tests.rs` | new, 28 tests |
| `crates/ferrix-formula/src/lib.rs` | +1 line (`pub mod stats;`) |
| `crates/ferrix-formula/src/eval.rs` | +1 match arm; `RangeSpec`/`range_spec`/`spec_get` widened from private to `pub(crate)` |

The `eval.rs` edit is deliberately minimal, for the two sibling agents adding
text and date functions to the same `eval_call` match:

```rust
name if crate::stats::is_stat_fn(name) => crate::stats::call(name, args, src),
```

The visibility widening of `RangeSpec` / `range_spec` / `spec_get` is the only
other `eval.rs` change. It exists so `stats.rs` reuses the *existing* SUMIF /
COUNTIFS range walker instead of adding a second one — open-ended range
clamping and cross-sheet ranges therefore work in the stats functions for free.
It is a whitespace-level diff on three signatures and should merge cleanly.

## Acceptance criteria — status

| Criterion | Status | Evidence |
| --- | --- | --- |
| Welford / numerically stable variance | **met** | `Welford` struct; test `variance_is_numerically_stable` |
| That test genuinely distinguishes naive form | **met** | the test *computes* `E[x^2]-E[x]^2` on the same input and asserts it equals exactly `0.0` before asserting Welford gives 2.5 / 2.0 |
| MEDIAN / PERCENTILE.INC do not sort a full copy | **met, with a documented memory cost** | see below |
| PERCENTILE.INC interpolates as Excel; boundaries pinned | **met** | `percentile_boundaries_and_interpolation` pins k=0, k=1, and two non-integer ranks |
| Text and empty cells skipped, not coerced | **met** | `median_skips_text_and_blanks`, `variance_skips_text_and_blanks`, `large_small_ignore_text_cells_when_bounding_k`, `rank_skips_text_cells` |
| Empty input → `#NUM!` | **met** | `empty_input_is_num_error` covers all ten range-taking functions |
| MODE with no repeat → `#N/A` | **met** | `mode_with_no_repeat_is_na` |
| LARGE/SMALL k out of range → `#NUM!` | **met** | `large_small_k_out_of_range_is_num` (k = 0, n+1, −1 for both) |

### The no-full-copy criterion, honestly

Two claims, kept separate on purpose:

1. **"Does not sort a full copy" — met.** Selection is
   `select_nth_unstable_by(k, f64::total_cmp)` (quickselect, O(n), in place).
   Nothing is ever sorted and no second, sorted buffer is created. `MEDIAN` on
   an even count and `PERCENTILE.INC` between ranks each need two adjacent
   order statistics, and both get them from **one** partition (scan the
   already-partitioned side for its min/max) rather than two selection passes.
   `median_uses_selection_not_a_sort` asserts the buffer comes back *not*
   fully sorted, so swapping in `sort()` fails the test rather than passing it.

2. **The strict scale invariant — NOT met by the order-statistic family, and
   I am not claiming it is.** `MEDIAN`, `MODE`, `PERCENTILE.INC`,
   `QUARTILE.INC`, `LARGE`, `SMALL` hold one `f64` per numeric cell. Order
   statistics are not computable exactly in sublinear space in one pass, so
   this is unavoidable given the range machinery streams values rather than
   exposing a sorted column index. What I did instead:
   - one `f64` per numeric cell (8 bytes), not a `Value` and not a copy of the
     column's storage — a 10M-row column is one 80 MB buffer;
   - a hard cap, `MAX_BUFFERED_VALUES = 16_777_216` (128 MiB of `f64`, near the
     ~108 MB streaming peak the rest of the codebase targets, and comfortably
     above the 10M-row column the issue names). Past the cap the function
     returns `#NUM!`. It does **not** silently truncate — a median over a
     truncated input is a wrong answer that looks right.
   - `buffer_cap_refuses_rather_than_truncating` exercises the refusal path
     with an injected small cap.

   `STDEV.*`, `VAR.*` and `RANK` **do** hold the invariant: they stream in O(1)
   memory and are exact at any row count.

## Gates

All three run bare (no pipe into `tail`/`head`) from the clone root.

```
cargo test --workspace
test result: ok. 358 passed; 0 failed; 0 ignored; 0 measured; 0 filtered out; finished in 2.55s
test result: ok. 209 passed; 0 failed; 0 ignored; 0 measured; 0 filtered out; finished in 0.01s
test result: ok. 255 passed; 0 failed; 0 ignored; 0 measured; 0 filtered out; finished in 1.50s
test result: ok. 172 passed; 0 failed; 0 ignored; 0 measured; 0 filtered out; finished in 1.31s
test result: ok. 8 passed;   0 failed; ...
test result: ok. 4 passed;   0 failed; ...
test result: ok. 3 passed;   0 failed; ...
(plus doc-test / empty targets at 0)
TEST_EXIT=0

cargo fmt --all --check
FMT_EXIT=0        (no output)

cargo clippy --workspace --all-targets -- -D warnings
Finished `dev` profile [unoptimized + debuginfo] target(s) in 6.39s
CLIPPY_EXIT=0
```

Clippy initially failed on two lints of mine (`redundant_closure`,
`assertions_on_constants`); both fixed, then green.

## Test counts

Measured, not estimated — the "before" number comes from a `git worktree` on
`HEAD~1`.

| | ferrix-formula lib tests | workspace total |
| --- | --- | --- |
| before | 181 | 981 |
| after | 209 | 1009 |
| delta | +28 | +28 |

## What I did NOT verify

- **No Excel cross-check.** Every expected value is from the documented
  formula (`k*(n-1)` fractional rank, Welford, Excel's tie rules) or hand
  computation. Excel was never launched, so this proves the implementation
  matches my reading of Excel's spec, not that Excel agrees cell-for-cell.
  The likeliest divergences are `MODE` tie-breaking on exotic inputs and
  `RANK`'s treatment of booleans in a range.
- **No 10M-row measurement.** The "does not sort a full copy" claim is proved
  by algorithm choice and by a 1001-element test asserting the buffer is left
  unsorted. I did not build a 10M-row fixture and measure peak RSS, so the
  80 MB figure is arithmetic (10M × 8 bytes), not an observation.
- **The cap boundary at its real value is untested.** The refusal path is
  tested with an injected cap of 3; a genuine 16,777,217-value input was never
  constructed.
- **No UI-level test.** These were exercised through `parse` + `eval` against a
  `Sheet`, not through `crates/ferrix-ui/src/harness.rs`. Whether they render
  and recalculate correctly in the grid is unverified.
- **No cross-sheet test.** `stats.rs` inherits cross-sheet range support from
  `range_spec`, but I wrote no `Sheet2!A1:A10` test for it, so that path is
  reasoned-about, not exercised.
- **No `benchdata/` was generated,** so nothing needed cleanup.
- **Sibling merge conflicts are predicted, not observed.** I never merged
  against the text/date branches.

## Deviations from Excel, deliberate

- Empty input to `STDEV.*` / `VAR.*` returns `#NUM!`. Excel returns `#DIV/0!`.
  Issue #26 specifies `#NUM!` for empty input across this family and a uniform
  answer was judged worth more than the exact Excel code for a case that only
  arises from an empty range. `VAR.S` / `STDEV.S` over exactly *one* value
  still returns `#DIV/0!`, matching Excel.
- `MODE.SNGL` and `RANK.EQ` are accepted as aliases of `MODE` and `RANK`. Not
  in scope, but free, and their absence would be a surprising `#NAME?`.
