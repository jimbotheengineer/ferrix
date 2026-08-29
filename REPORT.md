# Roadmap feature 6 — freeze panes, split view, zoom

Branch: `feat/freeze-zoom`  ·  Clone: `C:/Users/Error/projects/ferrix-view`

**Commits (newest last):**
- `5437ae2` feat(ui): freeze panes, split view and zoom (roadmap #6)
- `0b678d5` test(ui): harness tests for freeze, split, zoom; fix zoom persistence

**HEAD: `0b678d5`** — working tree clean, all three gates green.

## Status

Implementation complete and building; all three gates green (see Gates below).

## What was implemented

### `crates/ferrix-ui/src/grid.rs`

- **`Metrics`** — every length the grid draws with, scaled by one zoom factor
  (row height, header height, row-header width, column widths, fonts, padding,
  the formula dot, the validation triangle, the fill handle). Zoom is applied
  at the layout level, so it is one multiply rather than a special case in each
  paint call.
- **`Panes`** — freeze and split are ONE mechanism. A leading band of `rows`
  rows / `cols` columns is painted from its own offset; `frozen: true` pins
  that offset at 0 (freeze panes), `frozen: false` lets the user scroll it
  (split view: two independent offsets per axis over one column layout).
  Column widths and row heights are shared by construction — both bands index
  the same `col_widths` prefix sum and the same `Metrics`.
- **Paint path**: `row_bands` and `col_bands` are built with the FROZEN BAND
  FIRST, then the body, and every paint loop (cells, grid lines, row headers,
  column headers, hit test, merge extents, header drop indicator) walks those
  lists. Both bands resolve through the SAME `RowResolver` — there is no second
  row mapping anywhere in the function.
- **Hit testing**: one `hit` closure covers both bands; y/x above the seam
  resolves inside the band, below/right through the body offset. Zoom is
  already inside `row_h` and the widths, so no separate zoom-aware path exists
  to fall out of step.
- `ScrollState::clamp_body` — body clamp aware of the zoomed row height and of
  the floor imposed by a frozen band, so the body cannot scroll up into rows
  the band already owns.
- Scrollbars span BODY space only (a frozen band is not scrollable).
- `GridResponse` gained `painted_rows` (screen row → underlying row, band
  first), `frozen_row_count`, `zoom` — recorded from the same walk that paints
  the row numbers, so "what is on screen" cannot disagree with what is.

### `crates/ferrix-ui/src/prefs.rs`

- `Prefs.zoom: Vec<(String, f32)>` — zoom per sheet, keyed by sheet NAME (ids
  are per-run and meaningless across a restart). Serialised as
  `zoom.<sheet> = <factor>` lines, following the existing `key = value` format.
  100% is removed rather than stored. Newlines in a sheet name are stripped on
  write so a preference file cannot forge a second key.

### `crates/ferrix-ui/src/app.rs`

- State: `panes`, `zoom`. Adopted per sheet on switch (`prefs.zoom_of(name)`),
  panes reset on switch since they are defined in the old sheet's row space.
- `freeze_at_cursor(rows, cols)`, `unfreeze()`, `split_at_cursor()`,
  `set_zoom/zoom_in/zoom_out/zoom_reset`. Freeze resolves the cursor to a
  SCREEN row through the app's own `row_resolver`, so freezing under a sort or
  filter freezes the rows the user can see.
- **View menu** in the toolbar: Freeze rows / columns / both, Unfreeze, Split
  at cursor, Zoom in / out / reset (with the live zoom % shown).
- **Keyboard**: Ctrl+`+` / Ctrl+`=` / Ctrl+`-` / Ctrl+`0`.
- Zoom-aware `viewport_rows`, `center_on_selection`, `last_viewport_h`, and
  table-decor prefetch window (which now also covers the frozen band).
- Test-facing readback: `painted_rows()`, `frozen_row_count()`,
  `painted_underlying_rows()`, `cell_center(cell)`, `scroll_body_to`,
  `body_row_offset`, `panes()`, `zoom()`.

### `crates/ferrix-ui/src/harness.rs`

- `freeze_at_cursor`, `unfreeze`, `split_at_cursor`, `set_zoom`,
  `scroll_body_to`, `click_cell` (aims at the cell's ACTUAL painted centre read
  back from the app, same discipline as the existing `click_header`),
  `click_point` (raw pixel, for the zoom hit-test check).

## Tests added (7, all in `harness.rs`, driving the real app)

1. **`freeze_at_row_5_keeps_row_1_on_screen_after_scrolling_to_row_1_000_000`**
   — THE acceptance test. 1.1M-row CSV, freeze 5 rows, `scroll_body_to(1_000_000)`.
   Asserts the body really moved (offset ≥ 999,000), that `painted_rows()[0]`
   is still `(screen 0, underlying 0)`, that its painted row NUMBER is 1, that
   its DATA is row 1's, that all five frozen rows are numbered 1..5, that the
   body's first row is ≥ 999,000, and that a frame painted < 200 rows (the
   scale invariant). Then **unfreezes and re-scrolls and asserts row 1 is NOT
   on screen** — so the earlier assertions cannot pass against a dead feature.
2. **`a_click_at_200_percent_resolves_to_the_correct_data_cell`** — reads C8's
   painted centre at 100%, then at 200%, asserts the geometry MOVED (or the
   test would be vacuous), clicks the raw 200% pixel and asserts the cursor is
   C8. Repeats deeper down the viewport. Then asserts the SAME pixel at 100%
   resolves to a DIFFERENT cell — the property that makes the hit test
   genuinely zoom-aware rather than accidentally correct.
3. **`zoom_and_freeze_compose_with_a_sort_without_changing_which_record_a_row_shows`**
   — real header click to sort, asserts the sort actually permuted, captures
   the screen-row → underlying-row map, then applies zoom 200% AND freeze and
   asserts EVERY painted screen row still shows the record `visible_row_order()`
   says it should, band and body alike.
4. **`zoom_and_freeze_compose_with_a_filter_without_changing_which_record_a_row_shows`**
   — the same under search filter mode; also checks freeze counts SCREEN rows
   under a filter, and that the frozen band shows the filter's kept rows with
   their real row numbers.
5. **`split_view_scrolls_its_two_panes_independently`** — split at row 5, scroll
   the body to 2,000, assert the split band still shows rows 1..4 and the body
   moved.
6. **`frozen_columns_stay_on_screen_and_share_the_body_column_widths`** — 40
   columns, freeze 2, assert column A keeps a header and a paintable rect, and
   that clicking it selects A4 (the frozen band is hit-testable).
7. **`zoom_is_clamped_and_persists_per_sheet`** — 99.0 clamps to 4.0, 0.01 to
   0.25, and 2.0 survives constructing a FRESH app (what a restart is).

**Bugs these tests caught and I fixed:**
- Zoom was adopted at construction from the placeholder sheet name `"Sheet1"`
  and never re-adopted after the file load renamed the sheet, so the persisted
  zoom was silently lost on every restart. Fixed in `poll_load`.
- Tests that write prefs raced on the process-wide `FERRIX_CONFIG_DIR` / prefs
  file. Added `prefs::CONFIG_ENV_LOCK` and made every prefs-mutating test hold
  it (including the pre-existing `theme_preference_survives_a_restart`, which
  had the same latent race). Verified with 4 consecutive full-workspace runs.

## Not verified / known limits

- Split view's independent band offset is driven by the WHEEL over the band.
  There is no draggable splitter bar handle; the split is set from the menu at
  the cursor and scrolled with the wheel.
- Column-axis split shares `scroll.col_px` with the body for the trailing pane;
  the leading column band is pinned to `lead_col` (0 while frozen). A
  horizontally-independent split column offset is representable in `Panes`
  (`lead_col`) but is not wired to an input gesture.
- Zoom is per sheet NAME. Two sheets with the same name in different workbooks
  share a remembered zoom.

## Gates

Run from the clone root with `export PATH="$HOME/.cargo/bin:$PATH"`.

| gate | result |
|------|--------|
| `cargo test --workspace` | **PASS** — exit 0, **827 passed, 0 failed** (820 baseline + 7 new) |
| `cargo fmt --all --check` | **PASS** — exit 0 |
| `cargo clippy --workspace --all-targets -- -D warnings` | **PASS** — exit 0 |

The test suite was run 4 consecutive times end-to-end, all exit 0, to confirm
the prefs-locking fix removed the flake rather than hiding it.

`benchdata/` — none was generated in this clone; nothing to clean.
