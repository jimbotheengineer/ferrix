# Issue #31 — Import wizard: encoding, delimiter, and header detection

Branch: `feat/import-wizard` in `C:/Users/Error/projects/ferrix-import`
(isolated clone; the canonical checkout was never touched).

## Gates

All three run bare, exit code checked, no pipes:

| gate | result |
|------|--------|
| `cargo test --workspace` | **PASS** — 0 failed. ferrix-io 280, ferrix-ui 374, ferrix-core 406, ferrix-formula 334, plus integration bins. |
| `cargo fmt --all --check` | **PASS** (exit 0) |
| `cargo clippy --workspace --all-targets -- -D warnings` | **PASS** (exit 0) |

## What landed

### `crates/ferrix-io/src/sniff.rs` (new)

Detection and preview, both reading **at most `PREFIX_BYTES` (128 KiB)**.

- `sniff_path` / `sniff_reader` / `sniff_bytes` → `Detection { encoding,
  delimiter, quote, has_headers, skip_rows, prefix_bytes, clean, reason }`.
- `preview_path` / `preview_reader` / `preview_bytes` → `Preview` of at most
  `PREVIEW_ROWS` (100) records.
- Encoding: BOM first (UTF-8 / UTF-16LE / UTF-16BE), then "valid UTF-8 wins",
  then chardetng. `ENCODING_CHOICES` + `encoding_for_label` for the override.
- Delimiter: modal-field-count agreement across the sampled records for each
  (quote, delimiter) pair. Named candidates `, ; \t |`, **plus discovered**
  ASCII punctuation present on ≥90% of lines, so a `~`-delimited file is found
  rather than silently read as one column.
- Quote: `"` and `'` scored in the same pass.
- Header: answers `true` unless there is positive evidence otherwise (a numeric
  cell in row 0 over a numeric column, or a blank cell in row 0).
- `skip_rows`: leading records whose field count differs from the modal one.

### `crates/ferrix-io/src/csv.rs`

`CsvOptions` extended (not forked) with `quote: u8`, `skip_rows: usize`,
`encoding: Option<&'static Encoding>`. Defaults are byte-identical to before
(`b','` / `true` / `b'"'` / `0` / `None`). `chunk_bounds`, `find_record_end`,
`split_record`, `parse_chunk` now take the quote character; `chunk_bounds`
keeps its old 2-arg signature as a shim so `convert.rs` is unchanged.
Transcoding is `Cow::Borrowed` (no copy) unless a non-UTF-8 encoding is named.

### `crates/ferrix-ui/src/import_wizard.rs` (new)

Modal dialog: encoding combo, delimiter radio + custom text box, quote box,
header yes/no/at-row-N, skip-N-leading-rows, live 100-row preview, and
"Remember these settings for <filename>". Preview rebuilds only when a setting
it depends on actually changed (`PreviewKey`).

### `crates/ferrix-ui/src/prefs.rs`

`ImportRule { name, encoding, delimiter, quote, has_headers, skip_rows }`
persisted as `import.<name> = enc|delim|quote|headers|skip`, keyed by **file
name**. Delimiter/quote written as decimal byte values so a tab or `|` cannot
be confused with the format's own separator. Malformed rules are dropped
(→ wizard shows), never guessed.

### `crates/ferrix-ui/src/app.rs`

`start_load` now: remembered rule → else bounded-prefix detection → clean loads
immediately, unclean opens the wizard. `load_any` takes `CsvOptions` and passes
the detected delimiter/headers to the out-of-core converter too.

## The bounded-prefix criterion, and how it is proved

`detection_never_reads_past_the_prefix` and `preview_never_reads_past_the_prefix`
hand in a `Read` impl that **panics** if asked for byte `PREFIX_BYTES + 1` and
otherwise never returns EOF (an infinite file).

**Mutation check performed.** Replacing `reader.take(PREFIX_BYTES as u64)` with
`take(u64::MAX)`:

- both poison-reader tests **FAIL** (panic at the poison boundary) — confirmed
  by actually running it;
- `delimiter_detection_ignores_everything_past_the_prefix` (the real 64 MB
  file) **still passes**, because `buf.truncate` clips afterwards and 64 MB
  from page cache fits the time budget. That is written into its doc comment so
  nobody mistakes it for the primary proof. It is corroborating only.

No 10 GB file was generated. The 64 MB fixture is created and deleted inside
its own test.

## Test-isolation, and how it is proved non-vacuous

`the_per_thread_config_override_actually_isolates` claims a per-thread config
dir, then hammers save/load while a second thread does the same against a
different directory, asserting neither sees the other's rule.

**Mutation check performed.** Disabling the `test_config_dir()` branch in
`prefs::config_dir` makes this test **FAIL** — confirmed by actually running
it. The isolation is structural (`Harness::new` claims the dir), not a
convention every test author must remember.

## Assertions that can fail

- Latin-1: asserts the exact strings `"café"` / `"Zürich"` / `"crêpe"`.
  Reading those bytes as UTF-8 gives `caf\u{fffd}`, so the assertion fails if
  the encoding is ignored.
- `an_ordinary_csv_opens_without_the_wizard`: a plain UTF-8 comma CSV must NOT
  raise the wizard. Without it, "always show the wizard" would pass everything
  else.
- `the_preview_updates_when_settings_change`: switching to a delimiter the file
  does not use must make the preview collapse to 1 column. A preview computed
  once and cached fails.
- Wizard presence is asserted on **painted galley text** (`"Import settings"`,
  `"Delimiter"`), not on a state flag.
- `remembered_settings_skip_the_wizard_on_the_next_open` reads the rule back
  out of the preferences **file**, then opens a second `Harness` (a simulated
  restart) and asserts no wizard and the correct column count.

Two real over-triggers were caught by the *existing* suite during development
and fixed: single-column files were being called "unclean", and the header
heuristic was flipping `has_headers` off for all-text files. Both changes are
now pinned by their own tests.

## WHAT I DID NOT VERIFY

1. **The GUI was never launched.** Every check is the headless egui harness.
   This proves the dialog's widgets tessellate and its handlers run; it does
   not prove the layout looks right, that the combo box drops down under a real
   mouse, or that the preview grid is readable at any particular window size.
2. **The encoding override does not apply to the out-of-core path.** Files
   ≥1 GB (or refused by the memory budget) go through `convert.rs`, which now
   receives the detected delimiter and header flag but does **not** transcode.
   A 2 GB windows-1252 file will therefore still load with mangled accents.
   This is a known, documented gap (comment in `load_any`), not a silent one.
   Fixing it means adding streaming transcode to the converter, which is a
   larger change than issue #31's scope.
3. **Preview settings are not carried into the out-of-core convert for quote
   or skip_rows** — the converter's signature only takes delimiter and
   has_headers. A large file with a preamble or a `'` quote will convert with
   the defaults for those two.
4. **No real 10 GB file was timed.** The bounded-prefix property is proved
   structurally (poison reader) and corroborated on 64 MB. I did not measure
   an actual 10 GB open.
5. **chardetng's accuracy is not characterised.** I tested windows-1252,
   UTF-8, UTF-8-with-BOM and UTF-16LE. I did not test Shift_JIS, GBK, Big5, or
   windows-1251 against real-world files, even though the wizard offers them —
   the offer is "encoding_rs knows this label" (which IS tested), not "we
   verified detection picks it".
6. **Delimiter discovery on adversarial data.** The ≥90%-of-lines heuristic is
   tested against prose and against `~`. I did not test it against, say, a
   column of URLs (`/` and `:` appear on every line) — those bytes are
   discoverable, and such a file could plausibly get a wrong first guess. It
   would still be overridable in the wizard, and the wizard would be showing.
7. **Concurrent open of the same file from two windows** was not exercised.
8. **The `reopen_import_wizard` entry point has no UI affordance** — it is
   public and harness-reachable but not wired to a menu item or command
   palette entry, since adding a command would touch the command registry and
   that traces to a different issue.
