# Ferrix roadmap #8 — Autosave & Crash Recovery

**Branch:** `feat/autosave`
**Clone:** `C:/Users/Error/projects/ferrix-save`
**Status:** complete, all three gates green.

## Gate results (final run, verified)

| Gate | Result |
|---|---|
| `cargo test --workspace` | **PASS** — exit 0, **844 passed / 0 failed** (was 820; +24) |
| `cargo fmt --all --check` | **PASS** — exit 0 |
| `cargo clippy --workspace --all-targets -- -D warnings` | **PASS** — exit 0 |

## What was implemented

### `crates/ferrix-io/src/edits.rs`
- Refactored `save_edits` onto a shared `write_atomic(path, tmp_suffix, ...)`, matching
  the `export.rs` pattern: temp sibling → `flush` → **`sync_all()` (fsync)** → rename.
  Added the fsync, which the original sidecar path lacked — a rename can otherwise
  land while contents are still in page cache.
- `sibling_with_suffix()` **appends** suffixes rather than using `with_extension()`,
  which would have collapsed `x.fxedits` and `x.fxedits.autosave` onto the same
  `.tmp` path so the two writers corrupted each other. Covered by a test.
- New public API:
  - `AUTOSAVE_SUFFIX`, `DEFAULT_AUTOSAVE_SECS` (30)
  - `autosave_path_for_sidecar()` → `<base>.fxedits.autosave`
  - `write_autosave()` — atomic, O(edits)
  - `discard_autosave()` — missing file is success
  - `find_recovery()` → `Option<RecoveryCandidate>`; **two `stat`s, no parsing**.
    `Some` only when the autosave is strictly newer than the sidecar, OR the
    sidecar does not exist (crash before first save).
  - `load_autosave()` — full staleness/fingerprint check, no exemption
  - `RecoveryCandidate::age_hhmm()` → `"HH:MM"`

### `crates/ferrix-core/src/overlay.rs`
- Added a private `revision: u64` counter bumped by `set`/`clear`, and by
  `update_cached` **only when the value actually changes**. Exposed via
  `revision()`. This is what makes the no-change tick O(1) instead of an
  overlay diff. Guarding `update_cached` matters: recalc runs constantly, and
  counting every recalc as an edit would make an idle timer rewrite forever.

### `crates/ferrix-ui/src/prefs.rs`
- `autosave_secs: Option<u64>`, persisted. `autosave_interval()` (default 30s),
  `autosave_enabled()`. Explicit `0` disables; a **malformed value falls back to
  the default rather than to disabled**, so a config typo cannot silently strip
  the user's safety net.

### `crates/ferrix-ui/src/app.rs`
- `tick_autosave()` runs every frame from `frame()`. Returns early unless:
  enabled, file open, no unanswered recovery prompt, no write in flight,
  interval elapsed, **and `overlay.revision()` differs from the last autosaved
  revision**. First tick after load starts the clock rather than firing.
- `spawn_autosave()` clones the overlay and writes on a **worker thread** — the
  UI thread never serializes. `poll_autosave()` collects the result and records
  the revision **only on success**, so a failed write retries next tick.
- `clear_autosave()` **waits for the in-flight write before deleting**. Deleting
  first would let a running write recreate the file and leave a stale autosave
  that prompts on next launch.
- Recovery prompt (`show_recovery_prompt`): "Recover edits from HH:MM ago?" with
  **Recover** / **Discard**.
  - `recover_autosave()` adopts the overlay, `rebuild_graph_and_recalc()` (so
    formula sources are re-evaluated against the base, not trusted from cache),
    and marks the workbook **dirty** — recovered edits are unsaved by definition.
  - `discard_recovery()` deletes the autosave and touches nothing else.
- Autosave deleted on **manual save** (in `save_edits`), on **discard-and-close**,
  on viewport close-request when the close proceeds, and via `eframe::App::on_exit`.
- `restore_edits` now returns a named `RestoredEdits` struct (was a 5-tuple —
  clippy `type_complexity`).

## Tests added (24 new)

**`ferrix-io` (13):** path/roundtrip, autosave never writes the sidecar,
recovery offered when newer, NOT offered when sidecar is newer, offered with no
sidecar at all, discard leaves sidecar byte-identical, discard-missing is ok,
stale base rejected, **concurrent truncation test**, O(edits) scale
(100 edits/200M rows < 8KB), temp-path collision, `age_hhmm` formatting.

**`ferrix-ui` (11):** all five required scenarios plus clean-exit, untouched-file,
disabled, and formula recovery. All use the headless `harness.rs` (no synthetic
OS input).

### Required scenarios — all covered
| Required | Test |
|---|---|
| Crash after edits + tick, restart, prompt appears, Recover restores **every** edit | `crash_after_an_autosave_tick_offers_recovery_and_recover_restores_every_edit` |
| Manual save deletes the autosave | `a_manual_save_deletes_the_autosave` |
| Discard deletes it, sidecar untouched | `discard_deletes_the_autosave_and_leaves_the_sidecar_untouched` |
| Autosave over an existing one never observed truncated | `an_autosave_over_an_existing_one_is_never_observed_truncated` |
| No-change tick writes nothing at all | `a_no_change_tick_writes_nothing_at_all` |

Notes on test strength:
- The "crash" is a real `std::mem::drop(h)` with **no** save / close prompt /
  `on_clean_exit` — nothing on the clean-exit path runs.
- The crash test asserts A1 is still `"1"` *before* recovering, so it cannot pass
  if Recover does nothing.
- The no-change test asserts on **mtime and bytes** (file untouched, not merely
  rewritten identically), then edits again and asserts the mtime *does* move —
  so it can't pass against an autosave that simply stopped working.
- The truncation test runs a reader thread racing 40 writes of a 4000-edit
  overlay; `Truncated`/`BadMagic` fail it, transient Windows sharing-violation
  `Io` errors do not, and it asserts the reader achieved >0 successful reads.

### Negative controls (I verified the tests actually fail against a dead feature)
1. Replaced `write_autosave`'s atomic write with a direct `File::create` +
   write. → truncation test **FAILED**, reader observed **1266 truncated files**.
2. Made `tick_autosave` an immediate no-op. → **4 UI tests FAILED**.
Both controls were reverted (`git checkout --`); verified absent from the tree.

## Scale invariant
Autosave serializes the **overlay only**, never the sheet. Verified by
`autosave_cost_tracks_edits_not_rows`: 100 edits over a declared 200M-row /
12GB base → <8KB, <500ms. Cost tracks edit count, not row count.

## Not verified / caveats
- **No real 200M-row or 10GB file was exercised.** The scale test uses a
  synthetic `BaseFingerprint` declaring those dimensions; it proves the write is
  O(edits) and independent of declared row count, but is not a full-scale run.
- **No real process kill.** "Crash" is an in-process drop that bypasses every
  clean-exit path. A true SIGKILL mid-`write_atomic` was not performed; atomicity
  under real power loss rests on the temp+fsync+rename pattern and the
  concurrent-reader test.
- **`on_exit` signature is backend-specific.** This build uses wgpu, where
  `eframe::App::on_exit` takes no args. If the crate ever switches to the `glow`
  feature, that signature becomes `on_exit(&mut self, Option<&glow::Context>)`
  and will need updating.
- The recovery prompt's buttons are driven in tests via `recover_autosave()` /
  `discard_recovery()` (the same functions the buttons call) rather than by
  synthesising clicks on them, consistent with the harness's documented approach
  for position-dependent widgets. `recovery_prompt_open()` / `recovery_prompt_text()`
  assert the prompt itself is up and reads correctly.
- Multi-sheet: autosave persists the **active sheet's** overlay, matching the
  existing sidecar behaviour exactly. Not a regression, but note that the
  existing sidecar has the same single-overlay limitation.
- I did **not** add a settings UI control for the interval; it is configurable
  via `autosave_secs` in `prefs.toml` and by `set_autosave_secs()`.

## Note for the orchestrator
Two of my edits initially landed in the orchestrator checkout
(`C:/Users/Error/projects/ferrix`) because the patch tool resolved a bare
relative path there. Both were caught immediately, extracted with `git diff`,
reverted with `git checkout --`, and re-applied in this clone. I confirmed
`git status --porcelain` in `projects/ferrix` was **empty** afterwards. No peer
work was touched — the only modified file was `overlay.rs`/`prefs.rs` containing
solely my own additions.
