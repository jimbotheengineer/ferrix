# Issue #40 — Command palette

Branch `feat/cmdpal` in `C:\Users\Error\projects\ferrix-cmdpal`. Not pushed; the
orchestrator merges.

## What landed

New module `crates/ferrix-ui/src/command.rs` (+ `command/tests.rs`), plus a
refactor of the menu bar in `app.rs` and one new key in `prefs.rs`.

The issue is not really "add a search box" — it is "make it impossible for a
command to exist in a menu and not in the palette". So the registry came first:

* `command.rs` declares `CommandId` and `REGISTRY` from ONE macro table, so a
  variant cannot exist without a row and vice versa. `menu_items()` draws every
  menu from it; `CommandPalette::matches()` searches it.
* The five hand-written menu closures in `app.rs` (~150 lines, ~15 `close_menu`
  sites) are now a 6-line loop over `Menu::ALL`. The `FileAction` / `ViewAction`
  deferral enums are gone, replaced by one `Option<CommandId>`.
* `FerrixApp::run_command` is the single dispatcher, exhaustive over
  `CommandId` — adding a registry row without behaviour is a compile error.
  Toolbar buttons route through it too, so a toolbar click ranks in the palette.

### Acceptance criteria

| criterion | state |
|---|---|
| Ctrl+Shift+P and Ctrl+/ open a fuzzy list of every command | met — `ctrl_shift_p_opens_the_palette_and_ctrl_slash_does_too`, `the_palette_lists_every_command_and_filters_to_the_typed_one` |
| One registry the menus also read | met — `menu_commands_and_palette_commands_come_from_one_registry` walks the real `for_menu()` construction the menu bar calls and requires every item in the palette's list; `menus_are_drawn_only_from_the_registry` equivalent in `command/tests.rs` |
| Shows each command's keyboard shortcut | met — `shortcuts_are_shown_for_the_commands_that_have_them` |
| Recently used rank first, persists across restart | met — `running_a_command_reorders_recency_and_it_survives_a_restart` builds a SECOND `Harness` (= a fresh process's `Prefs::load`) and asserts the restored order reaches the visible list |
| Enter runs; Escape closes and restores selection | met — `enter_runs_the_highlighted_command`, `escape_closes_and_leaves_the_selection_exactly_as_it_was` (multi-cell selection, and a following arrow key proves the grid really got the keyboard back) |
| Unavailable shown DISABLED WITH REASON, not hidden | met — `unavailable_commands_are_listed_with_their_reason_not_hidden` asserts Unfreeze is still listed *and* that the reason clears once panes are actually frozen |
| Opening does not disturb the current edit | met — `opening_the_palette_does_not_disturb_an_edit_in_progress` |

## Notable design points

* Named `CommandPalette` / `command_palette` throughout. `palette` alone already
  means the COLOUR palette here (`theme`, issue #19).
* `CommandPalette::keys` **consumes** keys via `input_mut`. Everything else in
  this app reads without consuming, but the in-cell editor checks Escape in the
  *paint* path later in the same frame — a merely-observed Escape closed the
  palette AND cancelled the user's edit. A failing test caught this.
* Ctrl+Shift+P is consumed before Ctrl+/ because `Modifiers::matches_logically`
  ignores an extra Shift.
* `CommandState` is a snapshot of scalars, not a borrow: the panel closure
  already holds `&mut self`.
* `disabled_reason` falls back to a sentence when the app hands it an empty
  hint string — a grey row with no explanation is the failure the criterion
  exists to prevent, and `every_disabled_reason_is_a_sentence_not_a_flag`
  caught exactly that against `file.save`.
* Scale invariant untouched: the registry is a `const` slice, recency is capped
  at 40 slugs. Nothing here is per row or per cell.

## Conflict minimisation

A concurrent agent is editing `app.rs` and `prefs.rs` for issue #45.

* `prefs.rs`: one new field, one parse arm, one serialise block. The existing
  zoom handling and formatting are untouched.
* `app.rs`: one struct field, one initialiser, one call in the frame path, the
  new `run_command` / `command_state` / `command_palette_frame` block, and the
  menu-bar refactor. The menu refactor is a large deletion in a region issue #45
  is unlikely to touch (the toolbar's five menu closures).
* `harness.rs`: additive only — new helpers at the end of the impl and new
  tests at the end of the test module.

## Gates (all bare, not piped)

```
cargo test --workspace                                  312 passed; 0 failed
cargo fmt --all --check                                 exit 0
cargo clippy --workspace --all-targets -- -D warnings   exit 0, clean
```

## What I did NOT verify

* **The GUI was never launched.** Every check is the headless harness driving
  the real `FerrixApp` through `RawInput`. That proves the model, the key
  routing and the paint call path; it does NOT prove the palette window looks
  right, is legible against either theme, or is positioned sensibly on a real
  monitor. The disabled-reason text, the shortcut column and the selection
  highlight are asserted as *data*, not as pixels.
* **No click-through test on a menu item.** `a_menu_click_records_recency_too`
  drives `run_command` — the shared dispatcher the menu closure calls — not a
  synthesised pointer click at a menu's pixel. Menu geometry moves with the
  theme and window width; per AGENT_GUIDE that is a layout test, not a registry
  test. So "the menus read the registry" is proven at the construction level
  (`for_menu`), not by clicking a real menu item and observing the effect.
* **Palette row clicks are untested.** `show()` returns a clicked id, but no
  test synthesises a click on a palette row; only Enter is exercised.
* **`FileOpen` / `FileExportCsv` / `FileOpenXlsx` / `FileExportXlsx` /
  `FileCompact` are dispatched but not exercised end to end** — they open native
  file dialogs or start worker threads. Their registry rows, availability
  reasons and dispatch arms are covered; the resulting dialogs are not.
* **Recency ties.** Ranking is score, then recency, then registry order (stable
  sort). Two commands with equal score and neither ever run keep registry order;
  that is asserted only indirectly.
* **No multi-process test of the prefs file.** Persistence is proven by a second
  `Harness` in the same process with `FERRIX_CONFIG_DIR` redirected, which is
  what the existing `theme_preference_survives_a_restart` does. A genuine
  second process was never spawned.
