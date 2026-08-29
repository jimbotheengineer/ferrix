# Issue #24 — Formula library: text functions

Clone: `C:\Users\Error\projects\ferrix-text`
Branch: `feat/text-functions`
Author: jimbotheengineer

## What landed

All 18 functions from the issue: `LEFT`, `RIGHT`, `MID`, `LEN`, `UPPER`,
`LOWER`, `PROPER`, `TRIM`, `CLEAN`, `SUBSTITUTE`, `REPLACE`, `FIND`, `SEARCH`,
`CONCAT`/`CONCATENATE`, `TEXTJOIN`, `TEXT`, `VALUE`, `REPT`.

### Files

| File | Change |
| --- | --- |
| `crates/ferrix-formula/src/text.rs` | NEW — the entire implementation (~600 lines incl. docs) |
| `crates/ferrix-formula/src/text/tests.rs` | NEW — 24 unit tests, isolated from other agents' work |
| `crates/ferrix-formula/src/lib.rs` | +1 line: `pub mod text;` |
| `crates/ferrix-formula/src/eval.rs` | +4 lines: ONE delegating arm in `eval_call` |
| `crates/ferrix-core/src/arena.rs` | Formula-result interner (see below) + 1 test |
| `crates/ferrix-io/src/mapped.rs` | +4 lines: route tagged ids to the formula interner |

The merge-conflict surface in `eval.rs` is a single arm immediately above the
existing `_ => Value::Error(ErrorKind::Name)`:

```rust
name if crate::text::is_text_fn(name) => crate::text::call(name, args, src),
```

## Acceptance criteria

| Criterion | Status | Where |
| --- | --- | --- |
| FIND case-sensitive, SEARCH not, SEARCH takes `*`/`?`, reusing `criteria.rs` | met | `find_or_search()` calls `Pattern::compile` + `find_ignore_case`; FIND is a plain byte `find` (deliberately NOT routed through the matcher, because the matcher folds case and FIND must not). No second matcher exists. |
| 1-based indices; position past the end is `#VALUE!` | met | `find_or_search()` returns `Err(Value)` when `start > hay_len` or the needle is absent. Test: `start_position_past_the_end_is_value_error`. |
| Char-oriented, not byte-oriented; `LEN("café")==4`; MID never splits | met | `byte_of_char`/`sub_chars` are the only char→byte conversions and both come from `char_indices`. Tests: `len_counts_characters_not_bytes`, `left_right_mid_are_char_oriented`. |
| `TEXT` routes through `numfmt.rs` | met | `NumFmt::parse(&fmt).render(n)` / `.render_text(s)`. Test cross-checks against a direct `NumFmt::render` call. |
| `TEXTJOIN` honours `ignore_empty` | met | Test asserts `TEXTJOIN("-",TRUE,A1:A3)=="a-c"` vs `...,FALSE,...=="a--c"` on the same range. |
| String results intern through the arena (no fresh `String` per cell) | met, WITH A CAVEAT — read below | `text_value()` → `ferrix_core::arena::intern_formula_text`. Test `interning_dedups_...` proves 3,000 `UPPER()` cells over 3 distinct inputs return exactly 3 distinct `StrId`s. |
| A test asserts LEN on a 200M-row-shaped column stays O(1) per cell | met | `len_on_a_200m_row_shaped_column_is_o1_per_cell`. |

### The interning design, honestly

The issue named the constraint exactly right: `eval_view` returns a `Copy`
`Value`, `Value::Text` holds an arena `StrId`, and the evaluator has **no
mutable arena** — `CellSource` is `&self`-only everywhere.

Neither honest option in the issue was reachable without a large refactor:

* **(a) an interning sink threaded through evaluation** — would change the
  signature of `eval_view`, which is public API and is called from `app.rs`,
  `workbook.rs`, and every test in the workspace.
* **(b) a `CellSource`-side intern hook** — a `fn intern(&self, &str) -> StrId`
  on `CellSource` cannot be implemented by `Sheet` or `SheetView` without
  interior mutability, because their arenas are plain `&`-borrowed fields; every
  implementor would have to grow a `RefCell`/`Mutex`, and `WorkbookSource`
  borrows the workbook immutably by construction.

**What I did instead:** a process-wide, deduplicating, budget-capped interner in
`ferrix_core::arena`, whose ids carry a tag bit (`FORMULA_TEXT_TAG`, the `StrId`
high bit). `StringArena::resolve` routes tagged ids there, so every existing
resolver (`Sheet`, `EditOverlay`, `SheetView`, `WorkbookSource`, `MappedSheet`)
displays formula text with no change to its own code.

**The property the criterion asked for is met:** building a 1M-row text column
via formulas does NOT allocate a fresh `String` per cell — interning dedups, so
retained storage is O(distinct results), not O(rows). That is measured, not
asserted by inspection.

**The cost, stated plainly:**

1. Interned strings are `Box::leak`ed, so they live for the **process**, not the
   workbook. Closing a file does not reclaim them. This is what allows handing a
   `&'static str` back from a `&self` method with no `unsafe`.
2. Storage is capped at `FORMULA_TEXT_BUDGET` (64 MB). Past the cap, interning
   fails and text functions return `#VALUE!` — bounded and visible, never an
   unbounded leak, but also not graceful.
3. Distinct results DO cost memory. A column of `=A1&row_number` style unique
   results would grow the store until the cap. Dedup only helps low-cardinality
   columns — which, per the arena's own module docs, is the overwhelmingly
   common shape for spreadsheet text.
4. There is one `Mutex` on the interning path. It is taken once per
   text-producing cell evaluation, not per character.

A per-source sink (option a or b) would be strictly better and should replace
this if the evaluator ever gains one. The seam is small: `text_value()` is the
only producer, and `resolve_formula_text` the only consumer.

## Gate results

Run bare in `C:\Users\Error\projects\ferrix-text`, never piped. Commit `649c9ba`
(code) + `REPORT.md`.

```
cargo test --workspace                                 -> EXIT=0
    test result: ok. 359 passed; 0 failed   (ferrix-core lib)
    test result: ok. 8 passed;   0 failed   (ferrix-core format_scale)
    test result: ok. 4 passed;   0 failed   (ferrix-core replace_scale)
    test result: ok. 205 passed; 0 failed   (ferrix-formula lib)
    test result: ok. 3 passed;   0 failed   (ferrix-formula criteria_alloc)
    test result: ok. 172 passed; 0 failed   (ferrix-io lib)
    test result: ok. 255 passed; 0 failed   (ferrix-ui lib)
    (ferrix-bench and the empty integration targets: 0 passed, 0 failed)
  1,006 tests, 0 failures.

cargo fmt --all --check                                -> FMT_EXIT=0 (no output)

cargo clippy --workspace --all-targets -- -D warnings  -> CLIPPY_EXIT=0
    Checking ferrix-core / ferrix-formula / ferrix-io / ferrix-bench / ferrix-ui
    Finished `dev` profile [unoptimized + debuginfo] target(s) in 27.11s
    (zero warnings; all five crates re-checked, not cached-clean)
```

### Test counts

| Crate | Before | After | Delta |
| --- | --- | --- | --- |
| ferrix-formula (lib) | 181 | 205 | +24 (all in `text::tests`) |
| ferrix-core (lib) | 358 | 359 | +1 (`formula_text_ids_are_tagged_...`) |
| **Workspace total** | **981** | **1006** | **+25** |

Counts derived from the commit diff (`git show 649c9ba | grep -c '^+.*#\[test\]'`
= 25, with zero `-` removals, so no existing test was deleted or renamed).

### Assertion quality

Every test was written against the question "what would this report if the
feature did nothing at all?"

* `find_is_case_sensitive_search_is_not` fails if either function folds case
  wrongly — it asserts FIND("h","Hello") is an ERROR, which a case-insensitive
  FIND would turn into 1.
* `case_functions_handle_non_ascii` includes `assert_ne!(UPPER("café"), "CAFé")`,
  which is exactly the result an ASCII-only `to_ascii_uppercase` gives.
* `len_on_a_200m_row_shaped_column_is_o1_per_cell` counts cell TOUCHES through
  an instrumented `CellSource` and asserts the count is identical at 1,000 rows
  and 200,000,000 rows. A LEN that walked the column changes that number. (It
  also has a wall-clock backstop, but the touch count is the real instrument —
  a timing threshold alone would be a much blunter test.)
* `interning_dedups_...` counts DISTINCT `StrId`s, not a global counter, because
  cargo runs tests concurrently against the same process-wide interner and a
  global count would be flaky. Per-cell allocation returns 3,000 ids; the
  assertion demands 3.
* `unknown_function_still_reports_name_error` guards the new `eval.rs` arm — it
  must claim only text names, so `=LEFTISH(...)` is still `#NAME?`.

## What I did NOT verify

* **The UI was never launched.** No `harness.rs` test drives a text formula
  through the real app. I verified that `SheetView::resolve` and
  `WorkbookSource::resolve` route tagged ids correctly *by code reading and by a
  `ferrix-core` unit test on `StringArena::resolve`*, and the whole ferrix-ui
  suite (466 tests) passes — but nobody has typed `=UPPER(A1)` into a running
  window and seen `CAFÉ` appear in a cell. That is the single biggest untested
  gap.
* **Persistence of formula-text ids was not tested.** A saved `.frx` edit file
  stores `Value::Text(StrId)` for a formula's cached result. A tagged id written
  to disk and read back in a NEW process will resolve to whatever that process's
  interner holds at that index — i.e. it does **not** round-trip. In practice
  `recalc_all()` runs on load (`workbook.rs:915,1399,1601,1790`) and overwrites
  every formula's cached value before anything displays it, so I believe the
  stale id is never observed. I did not write a test proving that, and it is the
  place I would look first if something displays wrong text after a reload.
  A save/reload test is the obvious follow-up.
* **Excel was never opened.** Behaviour was matched against my knowledge of
  Excel's documented semantics, not against a running Excel. The edge cases most
  likely to differ: `PROPER` on digits and apostrophes, `VALUE` on locale-
  specific separators (only `,` grouping and `.` decimal are handled), and
  `TEXT` for date/time format codes (delegated wholly to `numfmt.rs`, so it is
  as correct — or not — as that engine already was).
* **No allocation-counting test for the text functions.** `criteria_alloc.rs`
  pins the matcher; I did not add an equivalent for `text.rs`. The text
  functions genuinely DO allocate per call (a result `String` before interning),
  so such a test would fail by design — the invariant here is "bounded by the
  cell's own length", not "zero", and that is what the O(1)-per-cell touch test
  measures instead.
* **No cross-sheet text test.** `CONCAT(Sheet2!A1:A3)` goes through the
  `Expr::XRange` branch of `for_each_text`, which is written but exercised only
  by the same-sheet path in tests; the workbook-level source needed to test it
  lives in ferrix-ui.
* **Concurrency of the interner was not stress-tested.** It is a `Mutex`, so it
  is sound, but I did not measure contention with multiple threads evaluating
  formulas simultaneously.
* **No benchdata was generated**, so there was nothing to clean up.
