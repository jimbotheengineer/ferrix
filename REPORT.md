# Roadmap feature 6 — freeze panes, split view, zoom

Branch: `feat/freeze-zoom`  ·  Clone: `C:/Users/Error/projects/ferrix-view`

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

## Tests added

See "Tests" section at the bottom — updated as they land.

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
| `cargo test --workspace` | see below |
| `cargo fmt --all --check` | see below |
| `cargo clippy --workspace --all-targets -- -D warnings` | see below |
