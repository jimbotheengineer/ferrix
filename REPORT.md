# Issue #28 — Full cell styling: borders, alignment, wrap, rotation

Clone: `C:/Users/Error/projects/ferrix-styling`, branch `feat/full-cell-styling`.

## Gate results

All three, run bare, exit code checked (never piped):

| gate | result |
| --- | --- |
| `cargo test --workspace` | **pass** — 1412 tests |
| `cargo fmt --all --check` | **pass** |
| `cargo clippy --workspace --all-targets -- -D warnings` | **pass** |

## What landed

### Model — `crates/ferrix-core/src/format.rs`

`CellDecor` follows the `Typography` pattern exactly: every field `Option`,
`None` means inherit, `Copy`, no heap of its own. Borders per side
(none/thin/medium/thick/double/dotted/dashed + colour) plus diagonal,
h/v alignment, indent 0–15, wrap, shrink, rotation -90..90. Text colour
was already separate from fill on `ManualStyle` and stays so.

Added to the **existing** scopes rather than a parallel store: `ColumnFormat.decor`,
`RangeFormat.decor`, `CellOverride.decor`, with `set_column_decor`,
`set_range_decor`, `set_cell_decor`, `decor_plan` / `resolve_decor`
(the `plan`/`resolve` twins), `decor_count`, `has_decor`, `wrapping_cols`.

Wrapped-row height maths lives here too (`wrapped_line_count`,
`wrapped_row_height`, `WRAP_CHAR_PX`) so paint, hit-testing and
`cell_screen_rect` derive it from ONE definition.

### Rendering — `crates/ferrix-ui/src/grid.rs`

Wired into the **general** paint path in `Grid::show`, not a specialised view:

- `RowHeights` — the single row-height source. Consumed by the paint loop,
  the grid lines, the row gutter, the hit test, and `cell_screen_rect_h`
  (which `app.rs` now uses for `cell_center`, `cell_rect`, `cell_at_point`
  and the in-cell editor). No second height mapping exists.
- `row_bands` became `(row, y, height)` with a running-sum layout, so a tall
  row pushes its neighbours down rather than being drawn over them.
- Borders drawn per edge with `drawn_edges` dedup; double drawn as two lines,
  dotted/dashed as real segments.
- Alignment, indent, v-align, wrap (`layout` with a wrap width), shrink,
  rotation (`Shape::Text` with an angle).
- New paint counters on `GridResponse`: `border_segments` (per EDGE, not per
  stroke), `rotated_texts`, `wrapped_texts` — surfaced through `app.rs` and
  the harness.

Entry point: `FerrixApp::apply_decor`, which routes a whole-column selection
to COLUMN scope, a range to RANGE scope, a single cell to the override map.

### Persistence

- `.fxfmt` sidecar bumped to **v3**, fixed-width 22-byte decor record on all
  three scopes. `BorderStyle::None` (erase) stays distinct from `None`
  (inherit) across the round trip.
- `crates/ferrix-io/src/decor_xlsx.rs` — new. `decor_format` builds the OOXML
  cell format; column scope is ONE `<col>` record whatever the row count;
  range scope is per-cell and **capped** at `MAX_RANGE_CELLS` (1M), past which
  it is reported, never expanded. `decor_survives_xlsx` / `decor_xlsx_loss`
  in the `rule_survives_xlsx` shape, wired into the app's export status line
  via `decor_export_warnings()`.

## Bug the tests caught

`a_border_on_the_selection_is_actually_painted` failed on first run: the paint
path gated decor resolution on a non-empty column plan, but a **per-cell
override contributes no plan entry**, so every single-cell border was silently
dropped while the model stored it perfectly. Fixed to resolve whenever the
sheet has any decoration. This is exactly the wiring trap the brief warned
about, and only a rendered-output assertion could see it.

Three xlsx test expectations were also wrong against the real XML and were
corrected to what the bytes actually say (not the code): `-30°` is
`textRotation="120"` not `121`; `vertical="bottom"` is Excel's default and is
correctly omitted; indent+rotation is a reported loss so it cannot appear in
the "survives cleanly" case.

## Acceptance criteria

| criterion | status |
| --- | --- |
| Stores on existing column/range scope | done — extended `ColumnFormat`/`RangeFormat`/`CellOverride` |
| Column fill over 10M rows keeps store <1KB, asserting STORED SIZE | done — `a_column_scope_decor_over_10m_rows_stays_under_1kb` (core, `heap_bytes()`) and `..._keeps_the_store_under_1kb` (harness, through `apply_decor`) |
| Wrapped text grows the row; hit-test still correct | done — `wrapped_text_grows_its_row_and_the_click_still_lands_on_it` clicks a pixel the row only covers *because* it grew |
| Rotation changes what is painted, asserted via paint output | done — `rotation_changes_what_the_grid_paints` on `rotated_texts` |
| Shared borders not double-drawn | done — `a_shared_border_between_neighbours_is_not_double_drawn` asserts 7 edges for two boxed neighbours, not 8 |
| xlsx round trip for every combination, by unzipping XML | done — 12 tests in `decor_xlsx/tests.rs`, all reading real parts |
| Lossy cases reported like `rule_survives_xlsx` | done — `decor_xlsx_loss` + `decor_export_warnings` |

## WHAT I DID NOT VERIFY

Be specific here; this is the section that saves someone a day.

1. **Excel was never launched.** The xlsx tests prove the package contains
   well-formed OOXML with the attributes I expect (`style="double"`,
   `wrapText="1"`, `textRotation="120"`, `diagonalUp="1"`, …). They do NOT
   prove Excel accepts the file, renders it as intended, or agrees with my
   reading of the rotation encoding. No round trip back through
   `import_xlsx` either — Ferrix has no styles.xml *reader*, so decoration
   exported to xlsx and reopened in Ferrix is lost. That is a real gap, but it
   is an import feature, not something #28 asked for.
2. **The wrap line-count is an ESTIMATE, not a measurement.**
   `wrapped_line_count` uses a fixed 7.2px average advance (the same constant
   the existing autofit estimator uses) rather than measuring a real egui
   galley. It has to: the hit test and `cell_screen_rect` run where no `Fonts`
   is available, and two different measurements would drift. Consequence: for
   an unusually wide or narrow font the row may be one line taller or shorter
   than the text strictly needs. Paint and hit-testing always AGREE (both read
   `RowHeights`), which is the property that matters, and that agreement is
   what the click test asserts — but the row height is not pixel-exact.
3. **Shrink-to-fit is approximate and lightly tested.** The font scale-down
   uses the same character-width estimate; I have no test asserting the text
   actually fits after shrinking, only that the flag round-trips and that
   wrap wins over shrink. Treat it as the weakest feature in this change.
4. **No visual inspection of any kind.** Every assertion is on counts,
   geometry, or XML text. I did not screenshot the grid or look at it. Dotted
   vs dashed, the double-border gap width, and the diagonal's appearance are
   unverified aesthetically — only that they emit distinct, non-zero output.
5. **Rotated text placement is not asserted.** I assert a rotated cell emits a
   `Shape::Text` with a non-zero angle. I do NOT assert *where* the rotated
   galley lands, and rotation does not currently grow the row or column to
   make room for tall rotated text the way Excel does. Rotated text in a
   narrow column may be clipped.
6. **Bold is suppressed on rotated text.** `ty.bold && rot == 0` — the
   sub-pixel over-paint trick that fakes bold would draw a second unrotated
   galley. Deliberate, but it means bold+rotation renders un-bolded, and no
   test covers it.
7. **Not tested under a frozen/split pane.** Wrapped rows compose with sort
   (tested) and filters/hidden rows (by construction, via `RowResolver`), but
   I wrote no test for a tall wrapped row inside a frozen band. The band
   height sum in `cell_screen_rect_h` handles it in code and is unexercised.
8. **No performance measurement.** I argue the cost is viewport-bounded
   (`wrapping_cols` is capped at `WRAP_COL_SCAN_CAP = 64`, `has_decor()`
   short-circuits an undecorated sheet) but I ran no benchmark and there is no
   allocation-counting test for the decor path the way `format_scale.rs`
   has one for rules.

## Cut from scope

Nothing was cut. All seven areas the issue lists — borders (per side +
diagonal, 7 styles, with colour), h/v alignment, indent 0–15, wrap, shrink,
rotation, and text colour separate from fill — are implemented, wired to the
general paint path, and tested. Shrink-to-fit is the least solid (see #3
above); if anything here should be treated as provisional it is that.
