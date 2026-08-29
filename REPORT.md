# Issue #30 — Clipboard interop with Excel: HTML flavour, Paste Special

Branch `feat/clipboard-interop` in `C:/Users/Error/projects/ferrix-clipboard`.

## Gate results

All three green, run bare, exit codes checked:

| Gate | Result |
|---|---|
| `cargo test --workspace` | **pass** — 1443 tests, 0 failed |
| `cargo fmt --all --check` | **pass** |
| `cargo clippy --workspace --all-targets -- -D warnings` | **pass** |

## THE PLATFORM LIMITATION — read this first

**eframe's clipboard API is plain text only.** `Context::copy_text` takes a
`String` and `Event::Paste` delivers a `String`; there is no way to register
`CF_HTML` beside `CF_UNICODETEXT` the way a native Win32 app would. Ferrix
therefore **cannot publish the HTML flavour to the system clipboard**, and no
amount of work inside this codebase changes that without a new native
clipboard dependency (deliberately not added).

What this means concretely:

- **Pasting rich content FROM Excel works.** When the text arriving on the
  clipboard is an HTML table — which is what Excel and browsers put there —
  Ferrix parses it as one and keeps number formats, fills and typography.
- **Copying rich content TO Excel does not.** What reaches the system
  clipboard on Ctrl+C is TSV, exactly as before. Excel will receive the values
  and lose the formatting.
- The HTML rendering is still built on every copy and held in
  `FerrixApp::clip_html`, so it is exercised by the real copy path and asserted
  in tests. Making it reach the OS clipboard is a **one-call change** in
  `copy_selection` the day eframe grows a flavoured clipboard.

This is stated in the module docs of `ferrix-core/src/clipboard.rs` and on
`FerrixApp::copy_selection`, not only here.

## What landed

### `crates/ferrix-core/src/clipboard.rs` (new, + `clipboard/tests.rs`)

The testable heart, no UI and no I/O:

- `ClipBlock` / `ClipCell` — a rectangular payload carrying display text,
  formula **source text**, number format, styling, source coordinate, and
  per-column widths.
- `to_html` — renders the `<table>` flavour Excel speaks: `mso-number-format`,
  inline CSS, `<colgroup>` widths, plus `data-ferrix-*` attributes so a
  Ferrix→Ferrix trip is exact where a Ferrix→Excel one can only be faithful.
- `from_html` — reads it back, tolerating what Excel and browsers actually
  emit: `<th>`, `colspan`, nested `<font>`/`<span>`, entities, `rgb()` and
  `#abc` colours, `px` vs `pt` lengths.
- `parse_clipboard` — **prefers HTML over plain text**, falls back to TSV.
- `PasteWhat` / `PasteOp` / `PasteOptions` — the Paste Special vocabulary.
  `PasteOp::apply` is `dest op src` and **refuses** non-numeric pairs and
  division by zero rather than writing `#VALUE!` over untouched data.
- `merge_rectangles` — collapses a per-cell attribute grid into maximal
  rectangles. This is the scale invariant for a formatted paste.

### `crates/ferrix-ui/src/workbook.rs`

- `copy_clip_block` — reads a selection as a rich payload. Rule plan built
  **per column**, matching the painter, not per cell.
- `paste_special` — applies a request as **ONE bulk `UndoEntry`**, the same
  `bulk: true` mechanism `clear_range` and Replace All already push. Not a
  second undo mechanism.
- `merge_conflict` — refuses a paste that would **partially** overwrite a
  merged region, naming it, before anything is written. A paste that covers a
  merge exactly is allowed (it writes the merge as a unit).
- `paste_formats` — stores rectangles, never per-cell overrides.
- Transpose is applied **before** every bound, guard and merge check.

### `crates/ferrix-formula/src/remap.rs`

- `paste_formula` — delegates to `fill::offset_formula`, which **honours `$`**.
  A paste is a fill, not a structural remap: `=B2*$F$1` copied down must still
  read `F1`. Text rewriting throughout; an AST round trip drops every `$`.

### `crates/ferrix-ui/src/app.rs`, `harness.rs`

Copy builds both flavours; paste prefers HTML; `paste_status` reports cells
written and format rectangles so tests assert on numbers. Harness gains
`copy`/`cut`/`paste_text`/`paste_special`/`merge_range`/`number_format_at`/
`style_at`.

## Acceptance criteria

| Criterion | Status | Where proven |
|---|---|---|
| Read Excel's HTML flavour, prefer it over plain text | **done** | `pasting_an_excel_html_table_lands_values_not_markup`, `the_html_flavour_is_preferred_over_the_text_one` |
| On copy write BOTH TSV and HTML | **partial — see limitation** | HTML is rendered and asserted (`copy_puts_tsv_on_the_clipboard_and_renders_the_html_flavour`) but only TSV reaches the OS clipboard |
| Round trip preserves values, number formats, styling | **done** | `a_round_trip_preserves_values_number_formats_and_styling` (harness), `a_round_trip_preserves_values_formats_and_styling` (core) |
| Paste Special: Values | **done** | `paste_values_drops_the_formula_and_keeps_the_number` |
| Paste Special: Formulas | **done** | `paste_formulas_rewrites_relative_refs_and_pins_absolute_ones` |
| Paste Special: Formats | **done** | `paste_formats_changes_formatting_without_touching_values` |
| Paste Special: Column Widths | **done** | `paste_column_widths_resizes_columns_and_leaves_cells_alone` |
| Paste Special: Transpose | **done** | `paste_transpose_swaps_rows_and_columns` |
| Paste Special: Add/Subtract/Multiply/Divide | **done** | `paste_add_combines_with_what_is_already_there`, `paste_multiply_and_subtract_use_the_right_operand_order` |
| Paste Special: Skip Blanks | **done** | `skip_blanks_leaves_the_destination_alone_under_empty_cells` + its contrast test |
| 100k-cell paste is ONE undo step | **done** | `a_100k_cell_paste_is_exactly_one_undo_step_and_one_undo_restores_it` — asserts exact depth before/after and that one Ctrl+Z restores 5 probes across the region |
| Paste over a merged region refused with a message | **done** | `a_paste_that_would_clip_a_merged_region_is_refused_with_a_message` + `a_paste_that_covers_a_merge_entirely_is_allowed` |
| Formulas rewritten via remap.rs, TEXT not AST | **done** | `paste_formula` delegates to `offset_formula`; 5 tests in `remap.rs` |

**Not done:** publishing the HTML flavour to the system clipboard on copy —
blocked by eframe, see above. Nothing else is outstanding.

## What I did NOT verify

Be specific here, because these are the gaps someone will otherwise assume are
covered:

1. **Excel was never launched.** No Microsoft Excel, LibreOffice, Google
   Sheets or browser was opened at any point. What is proven is that Ferrix
   emits *well-formed HTML matching the shape Excel documents and is observed
   to emit*, and that Ferrix *parses* HTML written in that shape. It is **not**
   proven that Excel accepts Ferrix's HTML, nor that real Excel clipboard
   payloads parse — the Excel-shaped fixtures in the tests are hand-written
   from the documented format, not captured from a real Excel copy.
2. **No real OS clipboard was exercised.** The harness has no system
   clipboard; `paste_text` synthesises the `Event::Paste` egui delivers. The
   app code downstream of that event is real, but the OS clipboard round trip
   itself is untested.
3. **No GUI was run.** Everything is the headless harness, per AGENT_GUIDE.
   Synthetic OS input was deliberately not used.
4. **The 100k-cell undo test pastes TSV, not HTML.** It proves the bulk-undo
   rule at 100k cells; it does not prove HTML *parsing* at that scale. The
   format-scale test uses a 20k-cell HTML table.
5. **Format-paste undo.** Pasting formats pushes no undo entry — formats are
   not cell contents and the existing format system has no undo integration.
   Ctrl+Z after a Paste Formats will not remove the formatting. This is a
   deliberate scope boundary, not an oversight, but it *is* a behaviour gap a
   user could notice.
6. **Column-width paste is not undoable** either, for the same reason (sizing
   lives outside the workbook's undo stack).
7. **Cut does not carry formats.** `Ctrl+X` clears cell contents only; the
   source's formatting is left behind on the emptied cells.
8. **No benchdata fixtures were generated**, so there was nothing to clean up.
   Nothing outside this clone was touched.

## Bugs the tests caught while being written

Three, all now fixed — recorded because they are the argument for the
assertion style the guide asks for:

1. `<col width>` is **pixels**, not points. A round trip shrank every column
   by 0.75× per trip. Caught by `a_round_trip_preserves_values_formats_and_styling`.
2. A naive CSS split truncated any two-section number format at its first
   `;` — `#,##0.00;[Red](#,##0.00)` became `#,##0.00`. Caught by
   `an_mso_number_format_with_a_semicolon_is_not_split_in_half`.
3. A blank clipboard cell pasted over a **base-backed** cell did not clear it:
   clearing the overlay merely revealed the base value again, so the paste
   looked like it had silently skipped that cell. Caught by
   `without_skip_blanks_a_blank_source_cell_does_clear_the_destination` — a
   test written specifically as the failing counterpart to the Skip Blanks
   test, so that Skip Blanks could not pass for the wrong reason.
