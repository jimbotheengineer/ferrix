# Conditional Formatting Editor — roadmap feature 11

Branch: `feat/conditional-format-ui`
Clone: `C:/Users/Error/projects/ferrix-cf`

## Gates — ALL THREE GREEN

```
cargo test --workspace                                   -> 0   (931 tests, was 909)
cargo fmt --all --check                                  -> 0
cargo clippy --workspace --all-targets -- -D warnings    -> 0
```

## What was built

The model layer already worked and was unreachable. This adds the editor, and
fixes two things that had to be true for the editor to work at all.

### `crates/ferrix-ui/src/cond_format.rs` (new, ~950 lines with tests)

- `CondTarget` — **Column | Range, no cell variant**. The scope invariant is
  enforced by the type: there is no way for this module to write per cell.
- `RuleForm` — every field of all eight variants, held simultaneously so
  flipping the kind selector does not discard typed input. `from_rule` /
  `to_rule` round-trip; a test asserts every variant survives, and that the
  fixture count equals `RuleKind::ALL.len()` so a ninth variant fails the build's
  tests rather than silently losing a dialog page.
- `CondFormatState` + `preview_format()` — the live preview.
- `xlsx_warning()` — delegates to `ferrix_io::table_xlsx::rule_survives_xlsx`,
  so the editor cannot drift from what export actually drops.
- `show()` — the dialog. Read-only over `SheetFormat`; every mutation is
  returned as a `CondOutcome` and applied by the caller.

### Live preview design

The preview is **additive, never a write-then-undo**. `preview_format` clones
the `SheetFormat` and splices the pending rule in at the position OK would put
it (appended for New, in place for Edit). The real store is untouched, so
Cancel is not an undo — there is nothing to undo. That is why
`cancelling_the_dialog_leaves_the_sheet_exactly_as_it_was` can compare the
whole `SheetFormat` for equality and expect it to hold. The clone is a handful
of rules and only exists while a modal is open.

### `crates/ferrix-core/src/format.rs`

Added `column_rules`, `set_column_rule`, `range_index_of`, `rules_for_range`,
`push_rule_for_range`, `set_range_rule`. `set_*_rule` replaces in place so an
edit keeps its precedence position rather than being promoted to winning.

### TWO BUGS FOUND AND FIXED

1. **Sheet-level conditional rules were never painted.** `grid.rs` read only
   `cell_override` typography out of `SheetFormat`; `plan`/`resolve` were never
   called from the UI. Every `ConditionalRule` stored at sheet scope resolved
   correctly and reached nothing. The editor would have been demonstrably dead
   without this. Plans are now built once per visible column per frame; window
   rules (scales, bars, top/bottom-N) use on-screen rows only.

2. **Clicks fell through modal windows into the grid.** The grid hand-hit-tests
   raw pointer state instead of using a widget `Response`, so pressing OK in a
   dialog ALSO clicked the cell behind the button and collapsed the selection at
   the moment the user was acting on it. Caught by two harness tests failing for
   reasons that had nothing to do with what they were testing. Fixed with an
   `is_pointer_over_area() && !ui_contains_pointer()` guard. This affected every
   pre-existing modal too (Name Manager, chart panel), not just this feature.

### `app.rs` / `harness.rs`

Format menu → New Rule / Manage Rules; state, preview wiring, `cond_apply`.
Harness gets `cond_new_rule`/`cond_manage`/`cond_form` plus `cond_click_ok` /
`cond_click_cancel`, which click the buttons at the rects the dialog reports
having actually painted (same discipline as `click_header`) — a dialog whose OK
is disabled or never drawn fails rather than passes.

## Tests — 22 net new

`resolved_style(cell, window)` on the app returns the `CellStyle` through
exactly the format the grid is painting from. Every assertion is on that, on
`rule_count()`, or on the whole `SheetFormat` — never on a status string and
never on "a rule appears in a list", both of which pass against a dead feature.

Harness (8, drive the real app):
- `creating_a_threshold_rule_restyles_matching_cells_and_leaves_others_alone`
  — matching cell filled, non-matching cell asserted `is_plain()`.
- `the_live_preview_shows_the_rule_before_it_is_committed` — cell resolves
  styled while `rule_count() == 0`; unchecking the box takes it back off.
- `cancelling_the_dialog_leaves_the_sheet_exactly_as_it_was` — starts from a
  populated store, asserts the preview was genuinely live first (otherwise the
  test proves nothing), then compares the whole `SheetFormat`.
- `reordering_two_overlapping_rules_changes_which_one_wins` — two rules both
  matching 150; the resolved fill flips red↔blue with the reorder.
- `a_topbottom_rule_surfaces_the_xlsx_lossy_warning_in_the_editor` — also
  asserts a Threshold does NOT warn.
- `a_rule_on_a_100k_row_column_stores_exactly_one_entry` — `rule_count() == 1`,
  `heap_bytes() < 4096`, `override_count() == 0`, and the rule still applies.
- `a_rule_changes_what_the_frame_actually_paints` — real `paint_shape_count()`
  goes up with the rule and returns to baseline when deleted.
- `manage_finds_the_column_rules_when_the_user_has_clicked_one_cell`.

Unit (14 in `cond_format/tests.rs`): variant round-trip, preview splice
semantics, scope isolation, precedence, inert-rule refusal, scale invariant.

## NOT verified

- Never run as a real GUI — headless harness only (as instructed).
- No actual .xlsx was exported and reopened in Excel to confirm a TopBottom rule
  is dropped. The warning is sourced from the exporter's own predicate, so they
  cannot disagree, but the end-to-end trip is untested here.
- Colour pickers, the kind combo box, and the ▲/▼/Edit/Delete buttons are
  driven through state or direct `SheetFormat` calls, not pixel clicks. Only OK
  and Cancel are clicked for real.
- Rules are not persisted through the `.ferrix` format sidecar by this work;
  `format_sidecar` already handles `SheetFormat`, but I did not test that a rule
  made in the editor survives save/load.
- No benchdata was generated, so nothing to clean.
