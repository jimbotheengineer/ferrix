# Issue #25 — Formula library: date and time functions

Clone: `C:\Users\Error\projects\ferrix-dates`
Branch: `feat/date-functions` (forked from `1dac3b2`)

## What landed

**New module: `crates/ferrix-formula/src/datetime.rs`** (+ `datetime/tests.rs`).
All fifteen functions in scope: `TODAY NOW DATE YEAR MONTH DAY HOUR MINUTE
SECOND WEEKDAY EOMONTH EDATE DATEDIF DAYS NETWORKDAYS`.

**Touched `eval.rs` with exactly one added match arm** (plus one `pub mod
datetime;` in `lib.rs`), as requested for merge-conflict avoidance:

```rust
name if crate::datetime::is_date_fn(name) => crate::datetime::call(name, args, src),
```

**`crates/ferrix-core/src/table.rs`** gained the *inverse* of the existing
calendar, next to it, so there is still exactly ONE calendar in the project:

- `serial_from_civil(y, m, d) -> Option<f64>` — exact inverse of the existing
  private `civil_from_serial`, phantom 1900-02-29 included.
- `days_in_month(y, m)` — what `EOMONTH` clamps to; February 1900 reports 29 so
  it agrees with the renderer.
- private `days_from_civil` (Hinnant), the mirror of the existing
  `civil_from_days`.

No new calendar arithmetic exists anywhere in `datetime.rs`; every conversion
goes through `serial_parts` / `serial_from_civil` / `render_serial`.

**`crates/ferrix-io/src/xlsx.rs`** — the import filter now asks
`ferrix_formula::datetime::is_date_fn` in addition to `SUPPORTED_FUNCTIONS`, so
date formulas survive an xlsx round trip as *live formulas* instead of being
downgraded to their cached value. (Asking the module rather than copying names
into the const list keeps one source of truth.)

## Acceptance criteria

| Criterion | Where |
|---|---|
| Storage stays f64 serial, no new `Value` variant | nothing added to `value.rs`; `value_stays_16_bytes` still passes |
| Serial 60 = phantom 1900-02-29, consistent with `render_serial` | `serial_60_is_excels_phantom_1900_02_29`, `every_function_agrees_with_render_serial_about_the_calendar` |
| `WEEKDAY` return-type 1/2/3 | `weekday_return_types`, `weekday_covers_a_whole_week_in_every_type`, `unknown_weekday_return_type_is_a_num_error` |
| `EOMONTH`/`EDATE` month-end clamp (31 Jan + 1 = 28/29 Feb) | `edate_clamps_to_the_month_end_instead_of_overflowing`, `eomonth_lands_on_the_last_day_of_the_target_month`, `eomonth_result_renders_as_the_month_end` |
| `NETWORKDAYS` optional holiday range | `networkdays_subtracts_holidays_from_a_range`, `a_holiday_listed_twice_is_deducted_once`, `networkdays_holidays_accept_a_single_cell_or_literal` |
| `TODAY`/`NOW` injectable, no wall-clock dependency | `TEST_CLOCK` thread-local + `set_test_clock`; commented at the definition. `today_and_now_read_the_injected_clock` asserts an *exact* serial |
| Date formula round-trips through xlsx with the same serial | `xlsx::tests::date_formulas_round_trip_with_the_same_serial` |

### How TODAY/NOW are injectable

`datetime::set_test_clock(Some(serial))` freezes the clock for the **current
thread**; `None` releases it. It is thread-local rather than global because
`cargo test` runs tests in parallel in one process and a global would let one
test's frozen clock leak into another's, producing failures that only reproduce
under particular scheduling. Tests use a `FrozenClock` RAII guard so a panicking
test cannot leave the override set. `releasing_the_clock_restores_the_wall_clock`
proves the override does not leak.

## Scale invariant

Nothing here allocates per row.

- `TODAY`/`NOW`/`DATE`/parts/`WEEKDAY`/`EDATE`/`EOMONTH`/`DAYS`/`DATEDIF` are
  pure scalar arithmetic — zero allocation.
- `NETWORKDAYS` counts weekdays in O(1) from the span length (full weeks × 5 +
  a ≤6-day remainder), *not* by walking days.
- The holiday argument is **streamed** through `for_each_serial`; ranges are
  clamped to the sheet's real extent (`A:A` costs populated rows, not 2^20).
  The only allocation in the module is a bitmap of one bit per day **in the
  requested date span** — ~46 bytes for a one-year span, 363 KB at the absolute
  limit of the 1900 date system — and it is independent of how many rows the
  holiday range covers. It also gives duplicate-free counting for free.

## Gate results (run bare, unpiped)

```
=== GATE 1: cargo test --workspace ===
test result: ok. 361 passed; 0 failed; 0 ignored; 0 measured; 0 filtered out   (ferrix-core)
test result: ok. 213 passed; 0 failed; 0 ignored; 0 measured; 0 filtered out   (ferrix-formula)
test result: ok. 173 passed; 0 failed; 0 ignored; 0 measured; 0 filtered out   (ferrix-io)
test result: ok. 255 passed; 0 failed; 0 ignored; 0 measured; 0 filtered out   (ferrix-ui)
test result: ok.   8 / 4 / 3 passed  (doc + integration)
TEST_EXIT=0

=== GATE 2: cargo fmt --all --check ===
FMT_EXIT=0

=== GATE 3: cargo clippy --workspace --all-targets -- -D warnings ===
CLIPPY_EXIT=0
```

## Test counts

| | before | after | delta |
|---|---|---|---|
| whole workspace | 981 | 1017 | +36 |
| ferrix-core lib | 358 | 361 | +3 |
| ferrix-formula lib | 181 | 213 | +32 |
| ferrix-io lib | 172 | 173 | +1 |

## Assertion quality — mutation checked

Three deliberate mutations were introduced and the suite was re-run, to confirm
the tests are not passing against dead code:

1. Made the xlsx import filter reject date functions (`|| false`) →
   `date_formulas_round_trip_with_the_same_serial` FAILED (`formulas_kept` 1 vs 6).
2. Removed the `EDATE` month-end clamp (`.min(dim)`) →
   `edate_clamps_to_the_month_end_instead_of_overflowing` FAILED.
3. Made `WEEKDAY` return-type 3 off by one →
   `weekday_return_types` and `weekday_covers_a_whole_week_in_every_type` FAILED.

All mutations were reverted; the committed tree is the unmutated one.

The core round-trip test `serial_from_civil_is_the_exact_inverse_of_the_renderer`
is **exhaustive** over all 2,958,466 serials rather than sampled, because a
one-day drift near a century rule or either side of serial 60 is exactly the bug
it exists to catch.

## What I did NOT verify

- **Excel was never opened.** The xlsx round trip proves that *Ferrix's own*
  writer and reader agree on the serial, that the formula text survives, and
  that the reimported formula re-evaluates to the same number. It does NOT prove
  Microsoft Excel accepts the file or computes the same answers.
- **No answer was cross-checked against a live Excel or LibreOffice.** Every
  expected value came from Excel's documented 1900 date system and from
  arithmetic done independently of the implementation (serials cross-checked
  with a Python `datetime` script). `DATEDIF`'s `MD`/`YM`/`YD` units are
  *undocumented* by Microsoft; the semantics implemented here are the widely
  reported ones, but they are the least confidently verified part of this change.
  In particular Excel's `MD` is known to have its own bugs for some inputs, which
  are **not** reproduced here.
- **Timezone: `NOW()` returns UTC**, not local time. `std` has no timezone
  database and adding one is a bigger dependency than this feature justifies. A
  user in UTC-5 will see a `NOW()` five hours ahead of their clock, and near
  midnight `TODAY()` can be a day ahead. This is a documented limitation in the
  module header, not an accident. No test covers local-time behaviour because
  there is none.
- **No UI work.** Nothing was added to `ferrix-ui`; there is no date-picker, no
  function-list entry, and the formula bar was not exercised through
  `harness.rs`. Date functions are reachable by typing them into a cell, which
  goes through the same `eval_call` path the unit tests use, but that specific
  route was not clicked through.
- **No performance measurement.** The scale claims above are read off the code
  (no per-row allocation, O(1) weekday counting, streamed holidays) and were not
  measured with `bench-*` or a memory probe. No `benchdata/` was generated, so
  there was nothing to clean up.
- **Concurrency of the test clock across threads within one test** was not
  exercised. A formula evaluated on a worker thread would see the wall clock,
  not a clock frozen on the main thread. Nothing in the current evaluator does
  that, but it is an untested edge if evaluation ever moves off-thread.
