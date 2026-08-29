# Issue #45 — Recent files, templates, and session restore

Branch `feat/recent` in clone `/c/Users/Error/projects/ferrix-recent`.
Not pushed; the orchestrator merges.

## What landed

New module `crates/ferrix-ui/src/recent.rs` (+ `recent/tests.rs`), mirroring the
`cond_format.rs` / `cond_format/` layout. `app.rs` and `prefs.rs` edits were
kept tight and localised, because a concurrent agent is editing both for #40.

### Acceptance criteria

| Criterion | State |
|---|---|
| Recent files (~15) with full path on hover, persisted via prefs.rs | met — `MAX_RECENT = 15`, hover shows `full_path()`, in the start screen and in File > Open Recent |
| Missing entries greyed and removable, not dropped | met — `is_available()` drives greying; removal is only ever the user's explicit `✖` / disabled-item choice |
| Start screen: recents, blank workbook, templates | met — `show_start_screen`, shown on a launch with no file argument and via File > Start screen |
| Reopening restores selection, scroll, zoom, frozen panes | met — `Session` recorded on clean exit and on file switch, applied after load |
| Zoom keyed on (workbook path, sheet name) | met — see below |
| Prefs written atomically | met — temp + `sync_all` + rename, following `ferrix_io::edits::write_atomic` |
| Missing/malformed prefs fall back to defaults | met — already true for missing; extended to the new keys and pinned |

### The zoom bug

`Prefs::zoom` went from `Vec<(String, f32)>` to `Vec<(String, String, f32)>`,
and `zoom_of` / `set_zoom` now take a `&Path` workbook. Serialised as
`zoom.<path>|<sheet> = <f32>`, with both halves percent-escaped
(`% = | space " \n \r`) so a path with spaces, a drive-letter colon,
backslashes, and a sheet name *containing the separator* all round-trip.
Backslashes and colons are deliberately left unescaped so the file stays
readable.

`set_zoom` still removes the entry at 100%, so "absent" continues to mean
default.

### Backward compatibility — DECIDED AND PINNED

An old-format `zoom.<sheet>` line names no workbook. Attaching it to a guessed
workbook would recreate exactly the cross-file bleed being fixed, so **old zoom
lines are DROPPED**. The user loses one remembered zoom level, re-zooms once,
and is correct from then on. Everything else in an old prefs file (theme,
show_empty_rows, autosave_secs) still loads.
Pinned by `an_old_format_zoom_line_is_dropped_and_does_not_break_the_rest_of_the_file`.

## Tests — 18 new, all through the real app or the real prefs round trip

Verified these FAIL against broken code rather than passing vacuously:

* Reverted the zoom keying to sheet-name-only → 3 tests failed
  (`identically_named_sheets_...`, `two_workbooks_with_a_same_named_sheet_...`,
  and the atomicity test's zoom-count check).
* Reverted `save()` to `std::fs::write` → the atomicity test failed with
  `bytes=0`, i.e. it really does catch an observable truncation.

The end-to-end tests drive the real `FerrixApp` through `harness.rs`, never
synthetic OS input. `two_workbooks_with_a_same_named_sheet_...` uses two files
with the same name in different directories, and asserts up front that their
sheet names really do collide — otherwise it would pass without exercising the
bug.

## Gates

All three run bare, never piped. Final run on the commit below:

```
cargo test --workspace              PASS — 305 passed, 0 failed
cargo fmt --all --check             PASS
cargo clippy --workspace --all-targets -- -D warnings   PASS
```

### A regression I caused and fixed

The first full-workspace run failed three pre-existing `compact` tests. Bisected
against pristine `9ac07a2` to confirm they were green before my change, so this
was mine: `adopt_cache_for_test` opens a cache directly without going through
`start_load`, so `show_start` stayed true, the start screen took the whole
frame, and the grid never ran. Fixed by having that seam clear `show_start` and
set `source_path`. Worth noting for review: any *other* path that opens data
without `start_load` needs the same treatment.


## WHAT I DID NOT VERIFY

* **The GUI was never launched.** Every UI assertion goes through the headless
  harness, which drives the real `update` path but paints to no window. That
  proves the widgets are constructed and the state changes; it does not prove
  the start screen *looks* right, that the greyed entries read as greyed to a
  human, or that the hover tooltips are positioned sensibly.
* **No test clicks the start screen's actual widgets.** `show_start_screen`
  returns a `StartChoice`, and the tests call `take_start_choice` directly.
  The painting code inside that function is exercised (the frame runs) but the
  click targets themselves are not hit-tested, unlike `cond_format`'s
  `cond_click_ok`. A mis-wired button would not be caught.
* **Atomicity is tested against concurrent readers, not against a real crash.**
  No process was killed mid-write and no power was cut. The claim proven is
  "a reader never observes a torn file across 60 rewrites", which is the
  same property `an_autosave_over_an_existing_one_is_never_observed_truncated`
  proves for the sidecar. `sync_all` durability against actual power loss is
  assumed from the OS, not measured.
* **`is_available()` is `Path::exists`.** A genuinely disconnected network
  share was not tested — no share was unmounted. The behaviour on an unplugged
  drive is inferred from `exists()` returning false, which is also what a
  deleted file returns; that ambiguity is the reason entries are kept rather
  than pruned.
* **Session restore is not clamped against the row count.** A file that shrinks
  between sessions restores a scroll offset past its end; the grid's existing
  per-frame `clamp_body` pulls it back. I did not add a test for the shrinking
  file case.
* **Templates are not round-tripped through save/export.** They open and
  evaluate; nothing writes one to disk and reads it back.
* **No concurrent-Ferrix test.** Two instances writing prefs at once each use a
  pid-suffixed temp file, so they cannot corrupt each other's temp, but the
  last rename wins and one instance's settings are lost. That is unchanged
  from the previous behaviour and was not addressed.
